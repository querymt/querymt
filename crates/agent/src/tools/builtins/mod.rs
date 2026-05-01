pub mod browse;
pub mod create_task;
pub mod delegate;
pub mod delete_file;
pub mod edit;
pub mod edit_output;
pub mod find_references;
pub mod get_function;
pub mod get_symbol;
pub mod glob;
pub mod helpers;
pub mod index;
pub mod knowledge_consolidate;
pub mod knowledge_ingest;
pub mod knowledge_list;
pub mod knowledge_query;
pub mod knowledge_stats;
pub mod language_query;
pub mod ls;
pub mod mdq;
pub mod multiedit;
pub mod patch_utils;
pub mod question;
pub mod read_shared;
pub mod read_tool;
pub mod replace_symbol;
pub mod search_text;
pub mod shell;
pub mod todo;
pub mod web_fetch;
pub mod write_file;

pub mod route_delegation_to_peer;
pub mod use_remote_provider;

pub use route_delegation_to_peer::RouteDelegationToPeerTool;
pub use use_remote_provider::UseRemoteProviderTool;

pub use browse::BrowseTool;
pub use create_task::CreateTaskTool;
pub use delegate::DelegateTool;
pub use delete_file::DeleteFileTool;
pub use edit::EditTool;
pub use find_references::FindSymbolReferencesTool;
pub use get_function::GetFunctionTool;
pub use get_symbol::GetSymbolTool;
pub use glob::GlobTool;
pub use index::IndexTool;
pub use knowledge_consolidate::KnowledgeConsolidateTool;
pub use knowledge_ingest::KnowledgeIngestTool;
pub use knowledge_list::KnowledgeListTool;
pub use knowledge_query::KnowledgeQueryTool;
pub use knowledge_stats::KnowledgeStatsTool;
pub use language_query::LanguageQueryTool;
pub use ls::ListTool;
pub use mdq::MdqTool;
pub use multiedit::MultiEditTool;
pub use question::QuestionTool;
pub use read_tool::ReadTool;
pub use replace_symbol::ReplaceSymbolTool;
pub use search_text::SearchTextTool;
pub use shell::ShellTool;
pub use todo::{TodoReadTool, TodoWriteTool};
pub use web_fetch::WebFetchTool;
pub use write_file::WriteFileTool;

use crate::tools::Tool;
use std::sync::Arc;

/// Returns all builtin tools.
///
/// This is the canonical source of truth for which tools are built-in.
/// Used for capability inference and tool registration.
pub fn all_builtin_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(BrowseTool::new()),
        Arc::new(CreateTaskTool::new()),
        Arc::new(DelegateTool::new()),
        Arc::new(DeleteFileTool::new()),
        Arc::new(EditTool::new()),
        Arc::new(GetFunctionTool::new()),
        Arc::new(GetSymbolTool::new()),
        Arc::new(GlobTool::new()),
        Arc::new(FindSymbolReferencesTool::new()),
        Arc::new(IndexTool::new()),
        Arc::new(LanguageQueryTool::new()),
        Arc::new(KnowledgeConsolidateTool::new()),
        Arc::new(KnowledgeIngestTool::new()),
        Arc::new(KnowledgeListTool::new()),
        Arc::new(KnowledgeQueryTool::new()),
        Arc::new(KnowledgeStatsTool::new()),
        Arc::new(ListTool::new()),
        Arc::new(MdqTool::new()),
        Arc::new(MultiEditTool::new()),
        Arc::new(QuestionTool::new()),
        Arc::new(ReadTool::new()),
        Arc::new(ReplaceSymbolTool::new()),
        Arc::new(SearchTextTool::new()),
        Arc::new(ShellTool::new()),
        Arc::new(TodoReadTool::new()),
        Arc::new(TodoWriteTool::new()),
        Arc::new(WebFetchTool::new()),
        Arc::new(WriteFileTool::new()),
    ]
}
