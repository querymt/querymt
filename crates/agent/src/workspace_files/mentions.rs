//! Resolution of `file://` `resource_link` prompt blocks into persisted
//! [`MessagePart::MentionContext`] snapshots.
//!
//! Resolution runs on the node that owns the session, when an input is
//! accepted. Every link is canonicalized (together with the workspace root)
//! before the sandbox check, and the *canonical* path is what gets read, so a
//! symlink swapped after the check cannot redirect the read. Links that cannot
//! be read never fail the prompt; they become [`MentionContent::Unresolved`].

use crate::acp::protocol::ContentBlock;
use crate::model::{
    MAX_ATTACHMENT_BYTES, MAX_IMAGES_PER_PROMPT, MAX_TOTAL_ATTACHMENT_BYTES, MentionContent,
    MessagePart, validate_attachment_size, validate_image,
};
use crate::tools::builtins::read_shared::{DEFAULT_READ_LIMIT, render_read_output};
use base64::Engine as _;
use querymt::chat::{MediaSource, ToolResultPart};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
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
    #[error("{0}")]
    Unreadable(String),
    #[error("image rejected: {0}")]
    InvalidImage(String),
}

/// Remaining text budget for one input.
#[derive(Debug, Clone, Copy)]
struct Budget {
    per_mention: usize,
    remaining_total: usize,
}

impl Budget {
    fn new(max_prompt_bytes: Option<usize>) -> Self {
        let total = max_prompt_bytes.map_or(MAX_TOTAL_MENTION_BYTES, |max| {
            max.min(MAX_TOTAL_MENTION_BYTES)
        });
        Self {
            per_mention: MAX_MENTION_BYTES.min(total),
            remaining_total: total,
        }
    }

    /// Fit `text` into the budget, charging what was kept.
    fn take(&mut self, text: String, path: &str) -> String {
        let allowed = self.per_mention.min(self.remaining_total);
        if text.len() <= allowed {
            self.remaining_total -= text.len();
            return text;
        }
        let mut end = allowed;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.remaining_total -= end;
        format!(
            "{}\n[truncated: content of {path} was cut; read it with tools for the rest]",
            &text[..end]
        )
    }
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
/// repeated links to the same URI are resolved once.
pub async fn resolve_mentions(
    blocks: &[ContentBlock],
    cwd: Option<&Path>,
    max_prompt_bytes: Option<usize>,
) -> Vec<MessagePart> {
    let mut budget = Budget::new(max_prompt_bytes);
    let mut images = ImageTally::default();
    let mut seen = HashSet::new();
    let mut parts = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let ContentBlock::ResourceLink(link) = block else {
            continue;
        };
        let Ok(url) = Url::parse(&link.uri) else {
            continue;
        };
        if url.scheme() != "file" || !seen.insert(link.uri.clone()) {
            continue;
        }
        let content = match read_link(&url, cwd, index, &mut budget, &mut images).await {
            Ok(content) => content,
            Err(refusal) => MentionContent::Unresolved {
                path: url
                    .to_file_path()
                    .map_or_else(|()| link.uri.clone(), |path| path.display().to_string()),
                reason: refusal.to_string(),
            },
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
    cwd: Option<&Path>,
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
    let cwd = cwd.ok_or(Refusal::NoWorkspace)?;
    let root = crate::index::resolve_workspace_root(cwd);
    let canonical = canonical_inside(root, target).await?;
    let path = canonical.display().to_string();

    let parts = render_read_output(&canonical, 0, DEFAULT_READ_LIMIT)
        .await
        .map_err(Refusal::Unreadable)?;
    match parts.into_iter().next() {
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
            images.bytes = images.bytes.saturating_add(data.len());
            if images.bytes > MAX_TOTAL_ATTACHMENT_BYTES {
                return Err(Refusal::InvalidImage("attachments too large".into()));
            }
            validate_attachment_size(index, data.len(), &mut 0)
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

/// Canonicalize `target` and `root`, requiring the target to lie inside.
async fn canonical_inside(root: PathBuf, target: PathBuf) -> Result<PathBuf, Refusal> {
    let (root, target) = tokio::task::spawn_blocking(move || {
        (dunce::canonicalize(root), dunce::canonicalize(target))
    })
    .await
    .map_err(|error| Refusal::Unreadable(error.to_string()))?;
    let target = target.map_err(|_| Refusal::NotFound)?;
    let root = root.map_err(|error| Refusal::Unreadable(error.to_string()))?;
    if target.starts_with(&root) {
        Ok(target)
    } else {
        Err(Refusal::OutsideWorkspace)
    }
}

// The image caps are part of the public contract of this module's behaviour.
const _: () = assert!(MAX_IMAGES_PER_PROMPT > 0 && MAX_ATTACHMENT_BYTES > 0);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::protocol::ResourceLink;
    use tempfile::TempDir;

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
        let mut parts = resolve_mentions(blocks, Some(cwd), None).await;
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
        let parts = resolve_mentions(&blocks, Some(dir.path()), None).await;
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
        assert_eq!(texts.len(), 6);
        assert!(!texts[0].contains("[truncated"));
        assert!(texts[5].contains("[truncated: content of"));
        let kept: usize = texts.iter().map(|text| text.len()).sum();
        assert!(kept < MAX_TOTAL_MENTION_BYTES + 6 * 200);
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
        let parts = resolve_mentions(&[link("file:///a.txt")], None, None).await;
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
        let parts = resolve_mentions(&blocks, Some(dir.path()), None).await;
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn budget_is_clamped_by_max_prompt_bytes() {
        let mut budget = Budget::new(Some(10));
        let out = budget.take("0123456789abcdef".into(), "/p");
        assert!(out.starts_with("0123456789\n[truncated: content of /p"));
    }
}
