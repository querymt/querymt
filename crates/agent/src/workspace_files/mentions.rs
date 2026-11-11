//! Resolution of `file://` `resource_link` prompt blocks into persisted
//! [`MessagePart::MentionContext`] snapshots.
//!
//! Resolution runs on the node that owns the session, when an input is
//! accepted. Reads go through [`super::confined`], which keeps them inside the
//! workspace even if a path component is swapped after the check. Links that
//! cannot be read never fail the prompt; they become
//! [`MentionContent::Unresolved`].
//!
//! Mentions draw from the same text, image and attachment budgets as the
//! prompt blocks of their input (see [`PromptUsage`]).

use super::confined::{ReadError, read_confined};
use crate::acp::protocol::ContentBlock;
use crate::index::CanonicalWorkspace;
use crate::model::{
    MAX_ATTACHMENT_BYTES, MAX_IMAGES_PER_PROMPT, MentionContent, MessagePart, PromptUsage,
    parse_file_link, validate_attachment_size, validate_image,
};
use crate::tools::builtins::read_shared::DEFAULT_READ_LIMIT;
use base64::Engine as _;
use querymt::chat::{MediaSource, ToolResultPart};
use std::collections::HashSet;
use url::Url;

/// Text budget for one mention.
pub const MAX_MENTION_BYTES: usize = 64 * 1024;
/// Text budget across all mentions of one input.
pub const MAX_TOTAL_MENTION_BYTES: usize = 256 * 1024;

/// Why a link was not read. `Display` is the reason shown to the model.
#[derive(Debug, thiserror::Error)]
enum Refusal {
    #[error("non-local host")]
    NonLocalHost,
    #[error("not a local path")]
    NotLocalPath,
    #[error("no workspace for this session")]
    NoWorkspace,
    #[error("not found")]
    NotFound,
    #[error("outside workspace")]
    OutsideWorkspace,
    #[error("text budget exhausted")]
    BudgetExhausted,
    #[error("{0}")]
    Unreadable(String),
    #[error("image rejected: {0}")]
    InvalidImage(String),
}

/// Remaining text budget for one input.
///
/// Charges what the model will see for each mention, i.e. what
/// [`MentionContent::render_text`] produces, so the snapshots fit the
/// `max_prompt_bytes` budget that message projection applies.
#[derive(Debug, Clone, Copy)]
struct Budget {
    per_mention: usize,
    remaining_total: usize,
}

impl Budget {
    /// `prompt_text_bytes` is what the prompt blocks already take from
    /// `max_prompt_bytes`.
    fn new(max_prompt_bytes: Option<usize>, prompt_text_bytes: usize) -> Self {
        let total = max_prompt_bytes.map_or(MAX_TOTAL_MENTION_BYTES, |max| {
            max.saturating_sub(prompt_text_bytes)
                .min(MAX_TOTAL_MENTION_BYTES)
        });
        Self {
            per_mention: MAX_MENTION_BYTES.min(total),
            remaining_total: total,
        }
    }

    /// Content bytes a mention of `path` may still keep, after its header and
    /// a possible truncation note.
    fn allowance(&self, path: &str) -> usize {
        self.per_mention
            .min(self.remaining_total)
            .saturating_sub(MentionContent::text_header(path).len() + truncation_note(path).len())
    }

    /// Fit `text` into the budget, charging the header and what was kept.
    fn take(&mut self, text: String, path: &str) -> String {
        let header = MentionContent::text_header(path).len();
        let allowed = self.per_mention.min(self.remaining_total);
        if header + text.len() <= allowed {
            self.remaining_total -= header + text.len();
            return text;
        }
        let mut end = self.allowance(path).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let note = truncation_note(path);
        self.remaining_total = self
            .remaining_total
            .saturating_sub(header + end + note.len());
        format!("{}{note}", &text[..end])
    }

    /// Charge the rendered form of a reference that carries no content.
    fn charge(&mut self, content: &MentionContent) {
        self.remaining_total = self
            .remaining_total
            .saturating_sub(content.render_text().len());
    }
}

fn truncation_note(path: &str) -> String {
    format!("\n[truncated: content of {path} was cut; read it with tools for the rest]")
}

/// Image accounting shared by the mentions of one input.
#[derive(Debug, Default)]
struct ImageTally {
    count: usize,
    bytes: usize,
}

/// Resolve every `file://` link in `blocks` into a mention snapshot.
///
/// Links of other schemes are skipped (they keep their existing handling), and
/// repeated links to the same URI are resolved once. `workspace` is the
/// session's resolved workspace (`None` when it has none on this node), and
/// `usage` is what the prompt blocks already consume of the input's limits.
pub async fn resolve_mentions(
    blocks: &[ContentBlock],
    workspace: Option<&CanonicalWorkspace>,
    max_prompt_bytes: Option<usize>,
    usage: PromptUsage,
) -> Vec<MessagePart> {
    let mut budget = Budget::new(max_prompt_bytes, usage.text_bytes);
    let mut images = ImageTally {
        count: usage.image_count,
        bytes: usage.attachment_bytes,
    };
    let mut seen = HashSet::new();
    let mut parts = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let ContentBlock::ResourceLink(link) = block else {
            continue;
        };
        let Some(url) = parse_file_link(&link.uri) else {
            continue;
        };
        if !seen.insert(link.uri.clone()) {
            continue;
        }
        let content = match read_link(&url, workspace, index, &mut budget, &mut images).await {
            Ok(content) => content,
            Err(refusal) => {
                let content = MentionContent::Unresolved {
                    path: url
                        .to_file_path()
                        .map_or_else(|()| link.uri.clone(), |path| path.display().to_string()),
                    reason: refusal.to_string(),
                };
                budget.charge(&content);
                content
            }
        };
        parts.push(MessagePart::MentionContext {
            uri: link.uri.clone(),
            content,
        });
    }
    parts
}

async fn read_link(
    url: &Url,
    workspace: Option<&CanonicalWorkspace>,
    index: usize,
    budget: &mut Budget,
    images: &mut ImageTally,
) -> Result<MentionContent, Refusal> {
    if url
        .host_str()
        .is_some_and(|host| !host.is_empty() && !host.eq_ignore_ascii_case("localhost"))
    {
        return Err(Refusal::NonLocalHost);
    }
    let target = url.to_file_path().map_err(|()| Refusal::NotLocalPath)?;
    let workspace = workspace.ok_or(Refusal::NoWorkspace)?;

    let allowance = budget.allowance(&target.display().to_string());
    let read = read_confined(workspace, &target, 0, DEFAULT_READ_LIMIT, Some(allowance))
        .await
        .map_err(Refusal::from)?;
    let path = read.path.display().to_string();
    match read.parts.into_iter().next() {
        Some(ToolResultPart::Text { text }) => Ok(MentionContent::Text {
            text: budget.take(text, &path),
            path,
        }),
        Some(ToolResultPart::Attachment(media)) => {
            let MediaSource::Inline { data } = media.source() else {
                return Err(Refusal::InvalidImage("image data unavailable".into()));
            };
            let mime_type = media
                .media_type()
                .map(ToString::to_string)
                .unwrap_or_default();
            validate_attachment_size(index, data.len(), &mut images.bytes)
                .and_then(|()| validate_image(index, &mime_type, &mut images.count))
                .map_err(|error| Refusal::InvalidImage(error.to_string()))?;
            Ok(MentionContent::Image {
                path,
                mime_type,
                data: base64::engine::general_purpose::STANDARD.encode(data),
            })
        }
        _ => Err(Refusal::Unreadable("nothing to read".into())),
    }
}

impl From<ReadError> for Refusal {
    fn from(error: ReadError) -> Self {
        match error {
            ReadError::NotFound => Self::NotFound,
            ReadError::OutsideWorkspace(_) => Self::OutsideWorkspace,
            ReadError::BudgetExhausted => Self::BudgetExhausted,
            ReadError::Resolution(reason) | ReadError::Unreadable(reason) => {
                Self::Unreadable(reason)
            }
            other @ ReadError::NotRegular => Self::Unreadable(other.to_string()),
        }
    }
}

// The image caps are part of the public contract of this module's behaviour.
const _: () = assert!(MAX_IMAGES_PER_PROMPT > 0 && MAX_ATTACHMENT_BYTES > 0);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::protocol::ResourceLink;
    use std::path::Path;
    use tempfile::TempDir;

    fn ws(cwd: &Path) -> CanonicalWorkspace {
        crate::index::canonical_workspace(cwd).expect("workspace")
    }

    fn link(uri: impl Into<String>) -> ContentBlock {
        ContentBlock::ResourceLink(ResourceLink::new("n", uri))
    }

    fn file_uri(path: &Path) -> String {
        Url::from_file_path(path)
            .expect("absolute path")
            .to_string()
    }

    fn workspace() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        std::fs::create_dir(dir.path().join(".git")).expect("marker");
        dir
    }

    async fn one(blocks: &[ContentBlock], cwd: &Path) -> MentionContent {
        let mut parts =
            resolve_mentions(blocks, Some(&ws(cwd)), None, PromptUsage::default()).await;
        assert_eq!(parts.len(), 1, "{parts:?}");
        match parts.remove(0) {
            MessagePart::MentionContext { content, .. } => content,
            other => panic!("unexpected part {other:?}"),
        }
    }

    fn reason(content: &MentionContent) -> &str {
        match content {
            MentionContent::Unresolved { reason, .. } => reason,
            other => panic!("expected unresolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn text_file_is_captured_even_through_a_symlinked_root() {
        // On macOS the temp dir lives under the /var -> /private/var symlink.
        let dir = workspace();
        let file = dir.path().join("a.ts");
        std::fs::write(&file, "const x = 1;\n").unwrap();
        let content = one(&[link(file_uri(&file))], dir.path()).await;
        match content {
            MentionContent::Text { text, .. } => assert!(text.contains("const x = 1;")),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn directory_mention_lists_entries() {
        let dir = workspace();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        let content = one(&[link(file_uri(&dir.path().join("src")))], dir.path()).await;
        match content {
            MentionContent::Text { text, .. } => assert!(text.contains("lib.rs")),
            other => panic!("expected listing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn png_is_captured_as_image() {
        let dir = workspace();
        let png: [u8; 16] = [
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, b'I', b'H', b'D', b'R',
        ];
        let file = dir.path().join("p.png");
        std::fs::write(&file, png).unwrap();
        let content = one(&[link(file_uri(&file))], dir.path()).await;
        assert!(matches!(
            content,
            MentionContent::Image { ref mime_type, .. } if mime_type == "image/png"
        ));
    }

    #[tokio::test]
    async fn one_large_file_is_truncated_with_a_note_naming_the_path() {
        let dir = workspace();
        let file = dir.path().join("big.txt");
        let line = "x".repeat(99);
        std::fs::write(&file, format!("{line}\n").repeat(1500)).unwrap();
        let content = one(&[link(file_uri(&file))], dir.path()).await;
        match content {
            MentionContent::Text { text, .. } => {
                assert!(text.len() < MAX_MENTION_BYTES + 200);
                assert!(text.contains("[truncated: content of"));
                assert!(text.contains("big.txt"));
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn total_budget_truncates_across_files() {
        let dir = workspace();
        let body = "y".repeat(60 * 1024);
        let mut blocks = Vec::new();
        for i in 0..6 {
            let file = dir.path().join(format!("f{i}.txt"));
            std::fs::write(&file, &body).unwrap();
            blocks.push(link(file_uri(&file)));
        }
        let parts =
            resolve_mentions(&blocks, Some(&ws(dir.path())), None, PromptUsage::default()).await;
        assert_eq!(parts.len(), 6);
        let texts: Vec<&str> = parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::MentionContext {
                    content: MentionContent::Text { text, .. },
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        // Four files fit, the fifth is cut, and the sixth is not read at all
        // because no text budget remains.
        assert_eq!(texts.len(), 5);
        assert!(!texts[0].contains("[truncated"));
        assert!(texts[4].contains("[truncated: content of"));
        let kept: usize = texts.iter().map(|text| text.len()).sum();
        assert!(kept < MAX_TOTAL_MENTION_BYTES + 5 * 200);
        match &parts[5] {
            MessagePart::MentionContext { content, .. } => {
                assert_eq!(reason(content), "text budget exhausted")
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn parent_traversal_is_outside_workspace() {
        let outer = TempDir::new().unwrap();
        std::fs::write(outer.path().join("secret.txt"), "s").unwrap();
        let root = outer.path().join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let uri = format!("{}/../secret.txt", file_uri(&root));
        let content = one(&[link(uri)], &root).await;
        assert_eq!(reason(&content), "outside workspace");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_is_outside_workspace() {
        let outer = TempDir::new().unwrap();
        std::fs::write(outer.path().join("secret.txt"), "s").unwrap();
        let root = outer.path().join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::os::unix::fs::symlink(outer.path(), root.join("link")).unwrap();
        let uri = file_uri(&root.join("link/secret.txt"));
        let content = one(&[link(uri)], &root).await;
        assert_eq!(reason(&content), "outside workspace");
    }

    #[tokio::test]
    async fn missing_file_is_not_found() {
        let dir = workspace();
        let content = one(&[link(file_uri(&dir.path().join("gone.txt")))], dir.path()).await;
        assert_eq!(reason(&content), "not found");
    }

    #[tokio::test]
    async fn non_local_host_is_refused() {
        let dir = workspace();
        let content = one(&[link("file://server/share/a.txt")], dir.path()).await;
        assert_eq!(reason(&content), "non-local host");
    }

    #[tokio::test]
    async fn missing_workspace_is_unresolved_not_an_error() {
        let parts =
            resolve_mentions(&[link("file:///a.txt")], None, None, PromptUsage::default()).await;
        assert!(matches!(
            parts.as_slice(),
            [MessagePart::MentionContext {
                content: MentionContent::Unresolved { .. },
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn non_file_links_and_text_are_ignored_and_duplicates_collapse() {
        let dir = workspace();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "a").unwrap();
        let uri = file_uri(&file);
        let blocks = [
            link("https://example.com/a"),
            link("attachment:///x"),
            link(uri.clone()),
            link(uri),
        ];
        let parts =
            resolve_mentions(&blocks, Some(&ws(dir.path())), None, PromptUsage::default()).await;
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn budget_charges_what_the_model_sees() {
        let path = "/p";
        let overhead = MentionContent::text_header(path).len() + truncation_note(path).len();
        let mut budget = Budget::new(Some(overhead + 10), 0);
        // Longer than the whole budget, so it must be cut.
        let kept = budget.take(format!("0123456789{}", "a".repeat(200)), path);
        assert!(kept.starts_with("0123456789\n[truncated: content of /p"));
        let rendered = MentionContent::Text {
            path: path.into(),
            text: kept,
        }
        .render_text();
        assert_eq!(rendered.len(), overhead + 10);
        assert_eq!(budget.remaining_total, 0);
    }

    #[test]
    fn prompt_text_shrinks_the_mention_budget() {
        let budget = Budget::new(Some(1000), 990);
        assert_eq!(budget.allowance("/p"), 0);
        assert_eq!(Budget::new(Some(1000), 0).remaining_total, 1000);
    }

    #[tokio::test]
    async fn exhausted_text_budget_skips_the_read() {
        let dir = workspace();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        let usage = PromptUsage {
            text_bytes: 50,
            ..Default::default()
        };
        let parts = resolve_mentions(
            &[link(file_uri(&file))],
            Some(&ws(dir.path())),
            Some(50),
            usage,
        )
        .await;
        match &parts[..] {
            [MessagePart::MentionContext { content, .. }] => {
                assert_eq!(reason(content), "text budget exhausted")
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn prompt_images_count_against_the_image_limit() {
        let dir = workspace();
        let png: [u8; 16] = [
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, b'I', b'H', b'D', b'R',
        ];
        let file = dir.path().join("p.png");
        std::fs::write(&file, png).unwrap();
        let usage = PromptUsage {
            image_count: MAX_IMAGES_PER_PROMPT,
            ..Default::default()
        };
        let parts =
            resolve_mentions(&[link(file_uri(&file))], Some(&ws(dir.path())), None, usage).await;
        match &parts[..] {
            [MessagePart::MentionContext { content, .. }] => {
                assert!(reason(content).starts_with("image rejected"), "{content:?}")
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
