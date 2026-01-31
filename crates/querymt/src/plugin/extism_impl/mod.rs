mod interface;
pub use interface::{
    BinaryCodec, ExtismChatChunk, ExtismChatRequest, ExtismChatResponse, ExtismCompleteRequest,
    ExtismEmbedRequest, ExtismSttRequest, ExtismSttResponse, ExtismTtsRequest, ExtismTtsResponse,
    PluginError,
};

#[cfg(feature = "extism_host")]
pub mod host;

// Export HTTP wire types when either host (http-client) or plugin (extism_plugin) features are enabled
#[cfg(any(feature = "http-client", feature = "extism_plugin"))]
mod http_wire;
#[cfg(any(feature = "http-client", feature = "extism_plugin"))]
pub use http_wire::{SerializableHttpRequest, SerializableHttpResponse};
