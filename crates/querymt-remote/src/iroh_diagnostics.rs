//! Stable status DTOs; relay availability is not peer liveness or mesh authorization.

use serde::{Deserialize, Serialize};
use typeshare::typeshare;

#[typeshare]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrohRelayDiagnostics {
    pub url: String,
    pub connected: bool,
    pub last_error: Option<String>,
    pub auth_denied_reason: Option<String>,
}

#[typeshare]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrohLookupServiceDiagnostics {
    pub service: String,
    pub results: String,
    pub errors: String,
}

/// Cumulative counters are decimal strings to preserve u64 precision in JS clients.
#[typeshare]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrohMetricsDiagnostics {
    pub relay_connections: String,
    pub relay_connection_failures: String,
    pub relay_connections_closed: String,
    pub relay_connections_rate_limited: String,
    pub lookups: String,
    pub lookups_failed: String,
    pub lookup_services: Vec<IrohLookupServiceDiagnostics>,
}

#[typeshare]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrohTransportDiagnostics {
    pub closed: bool,
    pub relays: Vec<IrohRelayDiagnostics>,
    /// None means the querymt-remote iroh-metrics feature is not enabled.
    pub metrics: Option<IrohMetricsDiagnostics>,
}

#[cfg(feature = "kameo-mesh")]
#[derive(Clone, Default)]
pub(crate) struct IrohDiagnostics(std::sync::Arc<parking_lot::RwLock<DiagnosticsState>>);

#[cfg(feature = "kameo-mesh")]
#[derive(Default)]
struct DiagnosticsState {
    endpoint: Option<iroh::Endpoint>,
    final_snapshot: Option<IrohTransportDiagnostics>,
}

#[cfg(feature = "kameo-mesh")]
impl IrohDiagnostics {
    pub(crate) fn new(endpoint: Option<iroh::Endpoint>) -> Self {
        Self(std::sync::Arc::new(parking_lot::RwLock::new(
            DiagnosticsState {
                endpoint,
                final_snapshot: None,
            },
        )))
    }

    pub(crate) fn snapshot(&self) -> Option<IrohTransportDiagnostics> {
        let state = self.0.read();
        state
            .endpoint
            .as_ref()
            .map(snapshot)
            .or_else(|| state.final_snapshot.clone())
    }

    pub(crate) async fn close(&self, endpoint: iroh::Endpoint) {
        endpoint.close().await;
        let mut state = self.0.write();
        state.final_snapshot = Some(snapshot(&endpoint));
        // Endpoint clones retain bound sockets even after close. Cache the final
        // status and release this clone before acknowledging runtime shutdown.
        state.endpoint = None;
    }
}

#[cfg(feature = "kameo-mesh")]
pub(crate) fn snapshot(endpoint: &iroh::Endpoint) -> IrohTransportDiagnostics {
    use iroh::Watcher as _;
    let closed = endpoint.is_closed();
    let relays = endpoint
        .home_relay_status()
        .get()
        .into_iter()
        .map(|relay| IrohRelayDiagnostics {
            url: relay.url().to_string(),
            connected: !closed && relay.is_connected(),
            last_error: relay.last_error().map(|error| format!("{error:#}")),
            auth_denied_reason: relay.auth_denied_reason().map(str::to_owned),
        })
        .collect();
    IrohTransportDiagnostics {
        closed,
        relays,
        metrics: metrics_snapshot(endpoint),
    }
}

#[cfg(all(feature = "kameo-mesh", not(feature = "iroh-metrics")))]
fn metrics_snapshot(_endpoint: &iroh::Endpoint) -> Option<IrohMetricsDiagnostics> {
    None
}

#[cfg(feature = "iroh-metrics")]
fn metrics_snapshot(endpoint: &iroh::Endpoint) -> Option<IrohMetricsDiagnostics> {
    let metrics = endpoint.metrics();
    let socket = &metrics.socket;
    let lookup = &metrics.address_lookup;
    // The N0 preset uses these two services. Read without creating new metric labels.
    let lookup_services = ["dns", "pkarr"]
        .into_iter()
        .map(|service| {
            let labels = iroh::address_lookup::ServiceLabels::new(service);
            IrohLookupServiceDiagnostics {
                service: service.into(),
                results: lookup
                    .service_results
                    .get(&labels)
                    .map_or(0, |counter| counter.get())
                    .to_string(),
                errors: lookup
                    .service_errors
                    .get(&labels)
                    .map_or(0, |counter| counter.get())
                    .to_string(),
            }
        })
        .collect();
    Some(IrohMetricsDiagnostics {
        relay_connections: socket.relay_conns_success.get().to_string(),
        relay_connection_failures: socket.relay_conns_failed.get().to_string(),
        relay_connections_closed: socket.relay_conns_closed.get().to_string(),
        relay_connections_rate_limited: socket.relay_conns_ratelimited.get().to_string(),
        lookups: lookup.lookups.get().to_string(),
        lookups_failed: lookup.lookups_failed.get().to_string(),
        lookup_services,
    })
}

#[cfg(all(test, feature = "kameo-mesh"))]
mod tests {
    use super::*;

    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;

    #[derive(Debug)]
    struct RelayAccess(Arc<AtomicBool>);

    impl iroh_relay::server::AccessControl for RelayAccess {
        async fn on_connect(
            &self,
            _request: &iroh_relay::server::ClientRequest,
        ) -> iroh_relay::server::Access {
            if self.0.load(Ordering::SeqCst) {
                iroh_relay::server::Access::Allow
            } else {
                iroh_relay::server::Access::Deny {
                    reason: Some("test relay policy denied endpoint".into()),
                }
            }
        }
    }

    async fn relay(
        addr: std::net::SocketAddr,
        allowed: Arc<AtomicBool>,
    ) -> iroh_relay::server::Server {
        let mut relay = iroh_relay::server::RelayConfig::new(addr);
        relay.access = Arc::new(RelayAccess(allowed));
        let mut config = iroh_relay::server::ServerConfig::default();
        config.relay = Some(relay);
        iroh_relay::server::Server::spawn(config).await.unwrap()
    }

    async fn wait_for_status(
        endpoint: &iroh::Endpoint,
        predicate: impl Fn(&IrohTransportDiagnostics) -> bool,
    ) -> IrohTransportDiagnostics {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let status = snapshot(endpoint);
                if predicate(&status) {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("relay diagnostics did not reach expected state")
    }

    async fn relay_endpoint(url: iroh::RelayUrl) -> iroh::Endpoint {
        iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
            .relay_mode(iroh::RelayMode::Custom(iroh::RelayMap::from_iter([url])))
            .alpns(vec![b"diagnostics-test".to_vec()])
            .bind_addr("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap())
            .unwrap()
            .bind()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn relay_auth_denial_is_visible_and_clears_after_recovery() {
        let allowed = Arc::new(AtomicBool::new(false));
        let server = relay("127.0.0.1:0".parse().unwrap(), allowed.clone()).await;
        let url: iroh::RelayUrl = format!("http://{}", server.http_addr().unwrap())
            .parse()
            .unwrap();
        let endpoint = relay_endpoint(url.clone()).await;
        let denied = wait_for_status(&endpoint, |status| {
            status
                .relays
                .iter()
                .any(|relay| relay.auth_denied_reason.is_some())
        })
        .await;
        let denied = denied
            .relays
            .iter()
            .find(|relay| relay.auth_denied_reason.is_some())
            .unwrap();
        assert_eq!(denied.url, url.to_string());
        assert!(!denied.connected);
        assert!(denied.last_error.is_some());
        assert_eq!(
            denied.auth_denied_reason.as_deref(),
            Some("test relay policy denied endpoint")
        );
        allowed.store(true, Ordering::SeqCst);
        let recovered = wait_for_status(&endpoint, |status| {
            status.relays.iter().any(|relay| relay.connected)
        })
        .await;
        assert!(
            recovered
                .relays
                .iter()
                .all(|relay| relay.last_error.is_none() && relay.auth_denied_reason.is_none())
        );
        endpoint.close().await;
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn relay_outage_does_not_break_direct_connection_and_recovers() {
        let allowed = Arc::new(AtomicBool::new(true));
        let server = relay("127.0.0.1:0".parse().unwrap(), allowed.clone()).await;
        let relay_addr = server.http_addr().unwrap();
        let endpoint = relay_endpoint(format!("http://{relay_addr}").parse().unwrap()).await;
        wait_for_status(&endpoint, |status| {
            status.relays.iter().any(|relay| relay.connected)
        })
        .await;
        let peer = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
            .bind()
            .await
            .unwrap();
        let addr = iroh::EndpointAddr::new(endpoint.id()).with_ip_addr(endpoint.bound_sockets()[0]);
        let (accepted, direct) = tokio::join!(
            async { endpoint.accept().await.unwrap().await.unwrap() },
            peer.connect(addr, b"diagnostics-test"),
        );
        let direct = direct.unwrap();
        server.shutdown().await.unwrap();
        wait_for_status(&endpoint, |status| {
            !status.relays.is_empty() && status.relays.iter().all(|relay| !relay.connected)
        })
        .await;
        let mut send = direct.open_uni().await.unwrap();
        send.write_all(b"still direct").await.unwrap();
        send.finish().unwrap();
        let received = tokio::time::timeout(Duration::from_secs(3), async {
            accepted
                .accept_uni()
                .await
                .unwrap()
                .read_to_end(128)
                .await
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(received, b"still direct");
        let server = relay(relay_addr, allowed).await;
        wait_for_status(&endpoint, |status| {
            status.relays.iter().any(|relay| relay.connected)
        })
        .await;
        assert!(direct.close_reason().is_none());
        endpoint.close().await;
        peer.close().await;
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn diagnostics_remain_available_after_endpoint_shutdown() {
        let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .bind_addr("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap())
            .unwrap()
            .bind()
            .await
            .unwrap();
        let before = snapshot(&endpoint);
        assert!(!before.closed);
        assert!(before.relays.is_empty());
        assert_eq!(before.metrics.is_some(), cfg!(feature = "iroh-metrics"));
        endpoint.close().await;
        let after = snapshot(&endpoint);
        assert!(after.closed);
        assert!(after.relays.iter().all(|relay| !relay.connected));
    }

    #[cfg(feature = "iroh-metrics")]
    #[tokio::test]
    async fn metrics_include_relay_and_per_service_lookup_counters() {
        let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
            .bind()
            .await
            .unwrap();
        let metrics = endpoint.metrics();
        metrics.socket.relay_conns_failed.inc_by(3);
        metrics.socket.relay_conns_ratelimited.inc();
        metrics.address_lookup.lookups.inc_by(7);
        metrics
            .address_lookup
            .service_errors
            .get_or_create(&iroh::address_lookup::ServiceLabels::new("dns"))
            .inc_by(2);
        let snapshot = snapshot(&endpoint).metrics.unwrap();
        assert_eq!(snapshot.relay_connection_failures, "3");
        assert_eq!(snapshot.relay_connections_rate_limited, "1");
        assert_eq!(snapshot.lookups, "7");
        assert_eq!(snapshot.lookup_services[0].errors, "2");
        assert_eq!(snapshot.lookup_services[1].errors, "0");
        endpoint.close().await;
    }
}
