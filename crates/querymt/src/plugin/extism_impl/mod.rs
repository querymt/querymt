mod interface;
pub use interface::{
    BinaryCodec, ExtismChatRequest, ExtismChatResponse, ExtismCompleteRequest, ExtismEmbedRequest,
};

#[cfg(feature = "extism_plugin")]
pub mod wrapper;

#[cfg(feature = "extism_host")]
pub mod host;
#[cfg(feature = "extism_host")]
pub use host::{ExtismConfig, ExtismProviderRegistry};
