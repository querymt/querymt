pub mod apply_patch;
pub mod browse;
pub mod create_task;
pub mod delegate;
pub mod delete_file;
pub mod edit;
pub mod glob;
pub mod helpers;
pub mod ls;
pub mod mdq;
pub mod multiedit;
pub mod patch_utils;
pub mod patch_validator;
pub mod question;
pub mod read_shared;
pub mod read_tool;
pub mod search_text;
pub mod semantic_edit;
pub mod shell;
pub mod todo;
pub mod web_fetch;
pub mod write_file;

pub use apply_patch::ApplyPatchTool;
pub use browse::BrowseTool;
pub use create_task::CreateTaskTool;
pub use delegate::DelegateTool;
pub use delete_file::DeleteFileTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use ls::ListTool;
pub use mdq::MdqTool;
pub use multiedit::MultiEditTool;
pub use question::QuestionTool;
pub use read_tool::ReadTool;
pub use search_text::SearchTextTool;
pub use semantic_edit::SemanticEditTool;
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
        Arc::new(ApplyPatchTool::new()),
        Arc::new(BrowseTool::new()),
        Arc::new(SemanticEditTool::new()),
        Arc::new(CreateTaskTool::new()),
        Arc::new(DelegateTool::new()),
        Arc::new(DeleteFileTool::new()),
        Arc::new(EditTool::new()),
        Arc::new(GlobTool::new()),
        Arc::new(ListTool::new()),
        Arc::new(MdqTool::new()),
        Arc::new(MultiEditTool::new()),
        Arc::new(QuestionTool::new()),
        Arc::new(ReadTool::new()),
        Arc::new(SearchTextTool::new()),
        Arc::new(ShellTool::new()),
        Arc::new(TodoReadTool::new()),
        Arc::new(TodoWriteTool::new()),
        Arc::new(WebFetchTool::new()),
        Arc::new(WriteFileTool::new()),
    ]
}
