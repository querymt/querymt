//! Portable repository knowledge.
//!
//! This module is deliberately separate from the legacy session-oriented
//! [`KnowledgeStore`](super::KnowledgeStore): it owns its own schema, lifecycle
//! and tools, and never reads or writes the protocol/session tables.
//!
//! Repository content is authoritative Markdown selected by the QueryMT-owned
//! `.qmt/knowledge.toml` manifest. Nothing here enables itself: the host must
//! opt in through [`RepositoryKnowledgeSettings`](crate::config::RepositoryKnowledgeSettings).

pub mod discovery;
pub mod document;
pub mod identity;
pub mod manifest;
pub mod reconcile;
pub mod references;
pub mod registry;
pub mod root;
pub mod service;
pub mod store;

pub use store::{
    GenerationUpdate, IndexDiagnostic, OpenOutcome, RebuildReason, RepositoryStore, SourceRecord,
    SourceState, SourceTouch, StoreCounts, StoreError, StoreState, StoredDocument, StoredSection,
    StoredSource,
};

pub use discovery::{
    DiscoveredFile, Discovery, DiscoveryDiagnostic, DiscoveryOptions, FileStamp, discover,
};
pub use document::{
    DiagnosticSeverity, DocumentDiagnostic, DocumentKind, Lifecycle, ParseOutcome, ParsedBatch,
    ParsedDocument, RECOGNIZED_KEYS, classify, parse_document, parse_documents,
};
pub use identity::{
    DocumentId, IdOrigin, KeyParseError, NormalizedBatch, NormalizedDocument, Section, SectionKey,
    normalize_documents,
};
pub use manifest::{
    Entrypoint, KnowledgeManifest, MANIFEST_RELATIVE_PATH, MANIFEST_VERSION, ManifestError,
    ManifestOutcome, SourceSelection, load_manifest,
};
pub use reconcile::{
    ReconcileError, ReconcileOptions, ReconcileReport, ReconcileStatus, reconcile,
};
pub use references::{
    DocumentReference, LinkKind, RawLink, Reference, RejectReason, TargetStatus,
    document_references, extract_links, reference_diagnostics, resolve_reference, verify_local,
};
pub use registry::{BindingError, RepositoryKnowledgeRegistry};
pub use root::{
    CACHE_KEY_VERSION, ResolvedRoot, RootError, RootRequest, RootSource, default_config_dir,
    find_git_root, resolve_root,
};
pub use service::{CurrentDocument, RefreshPolicy, RepositoryService, ServiceError};
