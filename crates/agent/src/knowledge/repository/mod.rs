//! Portable repository knowledge.
//!
//! This module is deliberately separate from the legacy session-oriented
//! [`KnowledgeStore`](super::KnowledgeStore): it owns its own schema, lifecycle
//! and tools, and never reads or writes the protocol/session tables.
//!
//! Repository content is authoritative Markdown selected by the QueryMT-owned
//! `.qmt/knowledge.toml` manifest. Nothing here enables itself: the host must
//! opt in through [`RepositoryKnowledgeSettings`](crate::config::RepositoryKnowledgeSettings).

pub mod acknowledge;
pub mod adapters;
pub mod browse;
pub mod budget;
pub mod catalog;
pub mod check;
pub mod code;
pub mod delivery;
pub mod diagnostics;
pub mod discovery;
pub mod document;
/// Offline pilot-evaluation harness for the `repository_knowledge` example.
/// Public only so the example can use it; not part of the supported API.
#[doc(hidden)]
pub mod evaluation;
pub mod evidence;
pub mod export;
pub mod gate;
pub mod identity;
pub mod maintenance;
pub mod manifest;
pub mod map;
pub mod orientation;
pub mod path_knowledge;
pub mod policy;
pub mod purpose;
pub mod query;
pub mod reconcile;
pub mod references;
pub mod registry;
pub mod root;
pub mod scaffold;
pub mod service;
pub mod signals;
pub mod store;

pub use store::{
    CorpusEntry, FileRecord, GenerationUpdate, IndexDiagnostic, OpenOutcome, RebuildReason,
    RepositoryStore, SearchPlan, SearchSnapshot, SectionMatch, SourceRecord, SourceState,
    SourceTouch, StoreCounts, StoreError, StoreState, StoredDocument, StoredSection, StoredSource,
};

pub use acknowledge::{
    AcknowledgePlan, AcknowledgeReceipt, AcknowledgeRequest, Selection as AcknowledgeSelection,
};
pub use browse::{
    DocumentCatalog, EntryKind, Fetched, FileInfo, GetError, GetRequest, ListEntry, ListRequest,
    ListResponse, PurposeInfo, SectionContent, SectionRef, StatsReport, SymbolInfo,
};
pub use catalog::{
    CatalogEntry, CatalogKind, CatalogOrigin, CatalogRequest, Citation, DirectoryCatalog,
    LISTING_NAMES,
};
pub use check::{CheckReport, CheckRequest};
pub use code::{
    CODE_EXTRACTOR_VERSION, CodeExtraction, CodeRecord, CodeStatus, SourceDoc, SymbolRecord,
    extract_code, symbol_key,
};
pub use delivery::{eligible as delivery_eligible, evidence_caution, withholds_text};
pub use diagnostics::{DeliveryDiagnostics, DiagnosticsSnapshot, Stage as DeliveryStage};
pub use discovery::{
    DiscoveredFile, Discovery, DiscoveryDiagnostic, DiscoveryOptions, FileStamp, discover,
};
pub use document::{
    DiagnosticSeverity, DocumentDiagnostic, DocumentKind, Lifecycle, ParseOutcome, ParsedBatch,
    ParsedDocument, RECOGNIZED_KEYS, classify, parse_document, parse_documents,
};
pub use evidence::{
    DependencyCache, DependencyOutcome, DocumentEvidence, EVIDENCE_DIR, EVIDENCE_VERSION,
    EvidenceEntry, EvidenceFile, RecordEvidence, evaluate_document, evidence_path, load_evidence,
    normalize_text, text_hash,
};
pub use export::{
    ExportAction, ExportError, ExportPlan, ExportReceipt, ExportRequest, MAP_BEGIN, MAP_END,
};
pub use gate::{
    GATE_VERSION, GateOutcome, GateReason, GateSection, gate_fingerprint, gate_tokens,
    normalize_prompt,
};
pub use identity::{
    DocumentId, IdOrigin, KeyParseError, NormalizedBatch, NormalizedDocument, Section, SectionKey,
    normalize_documents,
};
pub use maintenance::{Finding, MaintenanceError, PlannedWrite};
pub use manifest::{
    Entrypoint, KnowledgeManifest, MANIFEST_RELATIVE_PATH, MANIFEST_VERSION, ManifestError,
    ManifestOutcome, SourceSelection, load_manifest,
};
pub use map::{
    DEFAULT_MAP_BYTES, MAX_MAP_BYTES, MIN_MAP_BYTES, MapError, MapOutput, MapRequest, RenderedMap,
    render_map,
};
pub use orientation::{
    CardOrigin, OrientationCard, OrientationOutcome, OrientationRequest, SummarySource,
};
pub use path_knowledge::{
    DEFAULT_MAX_PATH_CARDS, PathCard, PathCardKind, PathKnowledge, PathKnowledgeRequest,
};
pub use purpose::{INVENTORY_BEGIN, INVENTORY_END, PurposeRow, parse_purpose_rows, purpose_key};
pub use query::{
    DEFAULT_LIMIT, HitKind, Lane, MAX_LIMIT, Provenance, QueryError, QueryHit, QueryRequest,
    QueryResponse,
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
pub use scaffold::{
    DEFAULT_FILE_NAME, ScaffoldAction, ScaffoldPlan, ScaffoldReceipt, ScaffoldRequest,
};
pub use service::{CurrentDocument, RefreshPolicy, RepositoryService, ServiceError};
pub use signals::{DEFAULT_LIFECYCLE, EvidenceState, IndexState, StateSignals};
