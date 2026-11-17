use anyhow::{Context, Result, bail};
use url::Url;

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 3030;
pub const DEFAULT_PATH: &str = "/acp/ws";

pub fn normalize_acp_ws_url(value: &str, allow_insecure: bool) -> Result<String> {
    let trimmed = value.trim();
    let trimmed = if trimmed.is_empty() {
        DEFAULT_HOST
    } else {
        trimmed
    };
    let candidate = if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else {
        if trimmed.contains("://") {
            bail!("WebSocket URL must use ws:// or wss://");
        }
        let suffix_start = trimmed.find(['/', '?', '#']).unwrap_or(trimmed.len());
        let (authority, suffix) = trimmed.split_at(suffix_start);
        let authority = if authority.matches(':').count() > 1 && !authority.starts_with('[') {
            format!("[{authority}]")
        } else {
            authority.to_string()
        };
        format!("ws://{authority}{suffix}")
    };

    let explicit_port = explicit_port(&candidate)?;
    let mut url =
        Url::parse(&candidate).with_context(|| format!("invalid WebSocket URL: {value}"))?;
    if !matches!(url.scheme(), "ws" | "wss") {
        bail!("WebSocket URL must use ws:// or wss://");
    }
    if url.host_str().is_none() {
        bail!("WebSocket URL is missing a host");
    }
    if url.scheme() == "ws" && !is_loopback_url(&url) && !allow_insecure {
        bail!("remote endpoints require wss://; pass --allow-insecure to use ws://");
    }
    if explicit_port.is_none() {
        url.set_port(Some(DEFAULT_PORT))
            .map_err(|_| anyhow::anyhow!("cannot set default WebSocket port"))?;
    }
    if url.path().is_empty() || url.path() == "/" {
        url.set_path(DEFAULT_PATH);
    }

    let normalized = url.to_string();
    match (explicit_port, url.port()) {
        (Some(port), None) => Ok(insert_port(&normalized, port)),
        _ => Ok(normalized),
    }
}

pub fn safe_endpoint_label(value: &str) -> Result<String> {
    let url = Url::parse(value).context("invalid normalized WebSocket URL")?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("WebSocket URL is missing a host"))?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("WebSocket URL is missing a port"))?;
    Ok(format!("{}://{host}:{port}{}", url.scheme(), url.path()))
}

fn is_loopback_url(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

fn explicit_port(value: &str) -> Result<Option<u16>> {
    let authority_start = value
        .find("://")
        .map(|index| index + 3)
        .ok_or_else(|| anyhow::anyhow!("WebSocket URL is missing a scheme"))?;
    let authority_end = value[authority_start..]
        .find(['/', '?', '#'])
        .map(|index| authority_start + index)
        .unwrap_or(value.len());
    let authority = value[authority_start..authority_end]
        .rsplit_once('@')
        .map_or(&value[authority_start..authority_end], |(_, host)| host);
    let port = if let Some(bracket_end) = authority.find(']') {
        authority[bracket_end + 1..].strip_prefix(':')
    } else {
        authority.rsplit_once(':').map(|(_, port)| port)
    };
    port.map(|port| {
        port.parse::<u16>()
            .with_context(|| format!("invalid WebSocket port: {port}"))
    })
    .transpose()
}

fn insert_port(value: &str, port: u16) -> String {
    let authority_start = value.find("://").map_or(0, |index| index + 3);
    let authority_end = value[authority_start..]
        .find(['/', '?', '#'])
        .map(|index| authority_start + index)
        .unwrap_or(value.len());
    let mut value = value.to_string();
    value.insert_str(authority_end, &format!(":{port}"));
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_websocket_urls_structurally() {
        let cases = [
            ("127.0.0.1", "ws://127.0.0.1:3030/acp/ws"),
            ("localhost", "ws://localhost:3030/acp/ws"),
            ("::1", "ws://[::1]:3030/acp/ws"),
            ("ws://[::1]", "ws://[::1]:3030/acp/ws"),
            ("ws://host:80", "ws://host:80/acp/ws"),
            ("wss://host:443", "wss://host:443/acp/ws"),
            ("ws://[::1]:80", "ws://[::1]:80/acp/ws"),
            ("ws://host:9000", "ws://host:9000/acp/ws"),
            ("host/custom", "ws://host:3030/custom"),
            ("host?token=test", "ws://host:3030/acp/ws?token=test"),
            ("ws://127.0.0.1:3030/acp/ws", "ws://127.0.0.1:3030/acp/ws"),
            ("ws://127.0.0.1:3030/ws", "ws://127.0.0.1:3030/ws"),
            (
                "wss://host:9443/custom?token=test",
                "wss://host:9443/custom?token=test",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(normalize_acp_ws_url(input, true).unwrap(), expected);
        }
    }

    #[test]
    fn rejects_non_websocket_urls() {
        assert!(normalize_acp_ws_url("https://example.com", false).is_err());
    }

    #[test]
    fn plaintext_remote_endpoints_require_an_explicit_opt_in() {
        assert!(normalize_acp_ws_url("remote.example", false).is_err());
        assert!(normalize_acp_ws_url("ws://remote.example", false).is_err());
        assert_eq!(
            normalize_acp_ws_url("wss://remote.example", false).unwrap(),
            "wss://remote.example:3030/acp/ws"
        );
        assert_eq!(
            normalize_acp_ws_url("localhost", false).unwrap(),
            "ws://localhost:3030/acp/ws"
        );
        assert_eq!(
            normalize_acp_ws_url("::1", false).unwrap(),
            "ws://[::1]:3030/acp/ws"
        );
    }

    #[test]
    fn endpoint_label_redacts_credentials_query_and_fragment() {
        assert_eq!(
            safe_endpoint_label("wss://user:secret@example.com:443/acp/ws?token=secret#fragment")
                .unwrap(),
            "wss://example.com:443/acp/ws"
        );
    }
}
