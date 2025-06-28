mod interface;
pub use interface::{
    BinaryCodec, ExtismChatRequest, ExtismChatResponse, ExtismCompleteRequest, ExtismEmbedRequest,
};

#[cfg(feature = "extism_host")]
pub mod host;
