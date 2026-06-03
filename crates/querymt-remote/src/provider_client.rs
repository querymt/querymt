use crate::{ProviderChatRequest, ProviderStreamRequest};
use querymt::chat::{ChatMessage, Tool};

#[derive(Clone, Debug)]
pub struct RemoteProviderClientConfig {
    pub provider_name: String,
    pub model: String,
    pub target_locator: String,
    pub params: Option<serde_json::Value>,
    pub heartbeat_interval_secs: u64,
    pub lease_ttl_secs: u64,
}

impl RemoteProviderClientConfig {
    pub fn new(
        target_locator: impl Into<String>,
        provider_name: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            provider_name: provider_name.into(),
            model: model.into(),
            target_locator: target_locator.into(),
            params: None,
            heartbeat_interval_secs: 10,
            lease_ttl_secs: 60,
        }
    }

    pub fn target_locator(&self) -> &str {
        &self.target_locator
    }

    pub fn remote_session_id(&self) -> Option<&str> {
        self.params
            .as_ref()
            .and_then(|v| v.get("_remote_session_id"))
            .and_then(|v| v.as_str())
    }

    pub fn with_params(mut self, params: Option<serde_json::Value>) -> Self {
        self.params = params;
        self
    }

    pub fn with_stream_controls(
        mut self,
        heartbeat_interval_secs: u64,
        lease_ttl_secs: u64,
    ) -> Self {
        self.heartbeat_interval_secs = heartbeat_interval_secs.max(1);
        self.lease_ttl_secs = lease_ttl_secs.max(1);
        self
    }

    pub fn lease_renew_every(&self) -> std::time::Duration {
        std::time::Duration::from_secs((self.lease_ttl_secs / 3).max(1))
    }

    pub fn build_chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> ProviderChatRequest {
        ProviderChatRequest {
            provider: self.provider_name.clone(),
            model: self.model.clone(),
            messages: messages.to_vec(),
            tools: tools.map(|t| t.to_vec()),
            params: self.params.clone(),
        }
    }

    pub fn build_stream_request<TRouterRef>(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
        session_id: String,
        request_id: String,
        stream_router_ref: TRouterRef,
        reconnect_grace_secs: u64,
    ) -> ProviderStreamRequest<TRouterRef> {
        ProviderStreamRequest {
            provider: self.provider_name.clone(),
            model: self.model.clone(),
            messages: messages.to_vec(),
            tools: tools.map(|t| t.to_vec()),
            session_id,
            request_id,
            stream_router_ref,
            reconnect_grace_secs,
            heartbeat_interval_secs: self.heartbeat_interval_secs,
            lease_ttl_secs: self.lease_ttl_secs,
            params: self.params.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::ChatMessage;
    use serde_json::json;

    #[test]
    fn config_tracks_target_locator_and_session_id() {
        let config = RemoteProviderClientConfig::new("provider_host::peer::abc", "demo", "m1")
            .with_params(Some(json!({"_remote_session_id": "s-123", "temperature": 0.7})));

        assert_eq!(config.target_locator(), "provider_host::peer::abc");
        assert_eq!(config.remote_session_id(), Some("s-123"));
    }

    #[test]
    fn stream_controls_are_clamped_and_renew_window_is_derived() {
        let config = RemoteProviderClientConfig::new("node", "demo", "m1").with_stream_controls(0, 0);

        assert_eq!(config.heartbeat_interval_secs, 1);
        assert_eq!(config.lease_ttl_secs, 1);
        assert_eq!(config.lease_renew_every(), std::time::Duration::from_secs(1));
    }

    #[test]
    fn build_requests_copy_provider_state() {
        let config = RemoteProviderClientConfig::new("node", "demo", "m1")
            .with_params(Some(json!({"temperature": 0.7})))
            .with_stream_controls(5, 9);
        let messages = vec![ChatMessage::user().text("hello").build()];

        let chat = config.build_chat_request(&messages, None);
        assert_eq!(chat.provider, "demo");
        assert_eq!(chat.model, "m1");
        assert_eq!(chat.params, Some(json!({"temperature": 0.7})));

        let stream = config.build_stream_request(
            &messages,
            None,
            "session-1".to_string(),
            "request-1".to_string(),
            "router-ref",
            42,
        );
        assert_eq!(stream.provider, "demo");
        assert_eq!(stream.model, "m1");
        assert_eq!(stream.session_id, "session-1");
        assert_eq!(stream.request_id, "request-1");
        assert_eq!(stream.reconnect_grace_secs, 42);
        assert_eq!(stream.heartbeat_interval_secs, 5);
        assert_eq!(stream.lease_ttl_secs, 9);
        assert_eq!(stream.params, Some(json!({"temperature": 0.7})));
    }
}
