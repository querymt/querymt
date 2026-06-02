use super::utils::ext_json_response;
use super::*;

impl LocalAgentHandle {
    pub(super) async fn handle_ext_update_plugins(&self) -> Result<ExtResponse, Error> {
        #[cfg(feature = "plugin-loaders")]
        {
            let registry = self.config.provider.plugin_registry();
            let results = crate::plugin_update::update_all_plugins(&registry, None).await;
            ext_json_response(&serde_json::json!({ "results": results }))
        }

        #[cfg(not(feature = "plugin-loaders"))]
        {
            Err(Error::method_not_found())
        }
    }
}
