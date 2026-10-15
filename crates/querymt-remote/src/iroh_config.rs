//! DNS policy for the shared iroh endpoint, independent of transport implementation types.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IrohDnsFallback {
    #[default]
    Deferred,
    Eager,
    IfSystemEmpty,
    Never,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IrohDnsProtocol {
    #[default]
    Udp,
    Tcp,
    Tls,
    Https,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IrohDnsNameserver {
    pub addr: SocketAddr,
    #[serde(default)]
    pub protocol: IrohDnsProtocol,
    pub tls_server_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct IrohDnsConfig {
    pub system_defaults: bool,
    pub fallback: IrohDnsFallback,
    pub nameservers: Vec<IrohDnsNameserver>,
    /// An empty list preserves upstream's public fallback resolvers.
    pub fallback_nameservers: Vec<IrohDnsNameserver>,
}

impl Default for IrohDnsConfig {
    fn default() -> Self {
        Self {
            system_defaults: true,
            fallback: IrohDnsFallback::Deferred,
            nameservers: Vec::new(),
            fallback_nameservers: Vec::new(),
        }
    }
}

impl IrohDnsConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.system_defaults
                || !self.nameservers.is_empty()
                || self.fallback != IrohDnsFallback::Never,
            "iroh DNS has no nameservers: enable system_defaults, configure nameservers, or enable fallback"
        );
        for server in self.nameservers.iter().chain(&self.fallback_nameservers) {
            anyhow::ensure!(
                server.addr.port() != 0,
                "iroh DNS nameserver port must not be zero"
            );
            if let Some(name) = &server.tls_server_name {
                anyhow::ensure!(
                    matches!(
                        server.protocol,
                        IrohDnsProtocol::Tls | IrohDnsProtocol::Https
                    ) && !name.trim().is_empty(),
                    "iroh DNS tls_server_name requires TLS/HTTPS and a nonempty name"
                );
            }
        }
        Ok(())
    }

    #[cfg(feature = "kameo-mesh")]
    pub(crate) fn resolver(&self) -> iroh::dns::DnsResolver {
        use iroh::dns::{DnsResolver, FallbackMode};
        let mode = match self.fallback {
            IrohDnsFallback::Deferred => FallbackMode::Deferred,
            IrohDnsFallback::Eager => FallbackMode::Eager,
            IrohDnsFallback::IfSystemEmpty => FallbackMode::IfSystemEmpty,
            IrohDnsFallback::Never => FallbackMode::Never,
        };
        let mut builder = DnsResolver::builder().with_fallback_mode(mode);
        if self.system_defaults {
            builder = builder.with_system_defaults();
        }
        for server in &self.nameservers {
            builder = builder.add_nameserver_config(server.config());
        }
        builder
            .fallback_nameserver_configs(
                self.fallback_nameservers
                    .iter()
                    .map(IrohDnsNameserver::config),
            )
            .build()
    }
}

#[cfg(feature = "kameo-mesh")]
impl IrohDnsNameserver {
    fn config(&self) -> iroh::dns::NameserverConfig {
        use iroh::dns::NameserverConfig;
        let config = match self.protocol {
            IrohDnsProtocol::Udp => NameserverConfig::udp(self.addr.ip()),
            IrohDnsProtocol::Tcp => NameserverConfig::tcp(self.addr.ip()),
            IrohDnsProtocol::Tls => NameserverConfig::tls(self.addr.ip()),
            IrohDnsProtocol::Https => NameserverConfig::https(self.addr.ip()),
        }
        .with_port(self.addr.port());
        match &self.tls_server_name {
            Some(name) => config.with_tls_server_name(name),
            None => config,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "kameo-mesh")]
    async fn answering_nameserver() -> (IrohDnsNameserver, tokio_util::task::AbortOnDropHandle<()>)
    {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let nameserver = IrohDnsNameserver {
            addr: socket.local_addr().unwrap(),
            protocol: IrohDnsProtocol::Udp,
            tls_server_name: None,
        };
        let task = tokio::spawn(async move {
            let mut packet = [0u8; 512];
            let (len, peer) = socket.recv_from(&mut packet).await.unwrap();
            let mut end = 12;
            while packet[end] != 0 {
                end += 1 + usize::from(packet[end]);
            }
            end += 5;
            assert!(end <= len);
            let mut response = packet[..end].to_vec();
            response[2..4].copy_from_slice(&[0x81, 0x80]);
            response[6..12].copy_from_slice(&[0, 1, 0, 0, 0, 0]);
            response.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 127, 0, 0, 42]);
            socket.send_to(&response, peer).await.unwrap();
        });
        (nameserver, tokio_util::task::AbortOnDropHandle::new(task))
    }

    #[cfg(feature = "kameo-mesh")]
    #[tokio::test]
    async fn custom_primary_dns_works_with_fallback_disabled() {
        let (server, task) = answering_nameserver().await;
        let config = IrohDnsConfig {
            system_defaults: false,
            fallback: IrohDnsFallback::Never,
            nameservers: vec![server],
            fallback_nameservers: Vec::new(),
        };
        config.validate().unwrap();
        let ips: Vec<_> = config
            .resolver()
            .lookup_ipv4("policy.test", std::time::Duration::from_secs(2))
            .await
            .unwrap()
            .collect();
        assert_eq!(ips, vec!["127.0.0.42".parse::<std::net::IpAddr>().unwrap()]);
        task.await.unwrap();
    }

    #[cfg(feature = "kameo-mesh")]
    #[tokio::test]
    async fn configured_fallback_answers_when_primary_does_not() {
        for fallback in [
            IrohDnsFallback::Deferred,
            IrohDnsFallback::Eager,
            IrohDnsFallback::IfSystemEmpty,
        ] {
            let (server, task) = answering_nameserver().await;
            let silent = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let nameservers = if fallback == IrohDnsFallback::IfSystemEmpty {
                Vec::new()
            } else {
                vec![IrohDnsNameserver {
                    addr: silent.local_addr().unwrap(),
                    protocol: IrohDnsProtocol::Udp,
                    tls_server_name: None,
                }]
            };
            let config = IrohDnsConfig {
                system_defaults: false,
                fallback,
                nameservers,
                fallback_nameservers: vec![server],
            };
            let ips: Vec<_> = config
                .resolver()
                .lookup_ipv4("fallback.test", std::time::Duration::from_secs(10))
                .await
                .unwrap()
                .collect();
            assert_eq!(ips, vec!["127.0.0.42".parse::<std::net::IpAddr>().unwrap()]);
            task.await.unwrap();
        }
    }

    #[cfg(feature = "kameo-mesh")]
    #[tokio::test]
    async fn never_policy_does_not_query_configured_fallback() {
        let (server, task) = answering_nameserver().await;
        let silent = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let config = IrohDnsConfig {
            system_defaults: false,
            fallback: IrohDnsFallback::Never,
            nameservers: vec![IrohDnsNameserver {
                addr: silent.local_addr().unwrap(),
                protocol: IrohDnsProtocol::Udp,
                tls_server_name: None,
            }],
            fallback_nameservers: vec![server],
        };
        assert!(
            config
                .resolver()
                .lookup_ipv4("never.test", std::time::Duration::from_millis(200))
                .await
                .is_err()
        );
        assert!(!task.is_finished(), "fallback must not receive a query");
    }

    #[test]
    fn defaults_preserve_system_dns_and_deferred_fallback() {
        let config: IrohDnsConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config, IrohDnsConfig::default());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn strict_policy_requires_a_primary_resolver() {
        let mut config = IrohDnsConfig {
            system_defaults: false,
            fallback: IrohDnsFallback::Never,
            ..Default::default()
        };
        assert!(config.validate().is_err());
        config.nameservers.push(IrohDnsNameserver {
            addr: "127.0.0.1:5353".parse().unwrap(),
            protocol: IrohDnsProtocol::Udp,
            tls_server_name: None,
        });
        assert!(config.validate().is_ok());
        config.nameservers[0].tls_server_name = Some("dns.internal".into());
        assert!(config.validate().is_err());
        config.nameservers[0].protocol = IrohDnsProtocol::Https;
        assert!(config.validate().is_ok());
        config.nameservers[0].addr.set_port(0);
        assert!(config.validate().is_err());
    }

    #[test]
    fn policies_and_nameservers_round_trip() {
        for fallback in [
            IrohDnsFallback::Deferred,
            IrohDnsFallback::Eager,
            IrohDnsFallback::IfSystemEmpty,
            IrohDnsFallback::Never,
        ] {
            let config = IrohDnsConfig {
                fallback,
                ..Default::default()
            };
            assert_eq!(
                serde_json::from_str::<IrohDnsConfig>(&serde_json::to_string(&config).unwrap())
                    .unwrap(),
                config
            );
        }
        assert!(serde_json::from_str::<IrohDnsConfig>(r#"{"fallback":"typo"}"#).is_err());
        assert!(serde_json::from_str::<IrohDnsConfig>(r#"{"unknown":true}"#).is_err());
    }
}
