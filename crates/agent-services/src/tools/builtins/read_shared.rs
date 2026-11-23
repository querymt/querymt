use querymt::chat::{MediaKind, MediaPart, MediaSource, ToolResultPart};
use std::path::Path;

pub const DEFAULT_READ_LIMIT: usize = 2000;

/// Maximum file size (bytes) we will read into memory for binary content.
/// Files larger than this get a descriptive error instead.
const MAX_BINARY_READ_BYTES: u64 = 20 * 1024 * 1024; // 20 MiB

/// Detect if bytes represent a supported image format.
pub(crate) fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    let kind = infer::get(bytes)?;
    match kind.mime_type() {
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" => Some(kind.mime_type()),
        _ => None,
    }
}

/// Render read output for the given path.
///
/// - Text files   → canonical text tool-result part with line-numbered XML-like format
/// - Image files  → validated inline image attachment tool-result part
/// - Other binary → text part with a descriptive error message
/// - Directories  → text part with entry listing
pub async fn render_read_output(
    target: &Path,
    offset: usize,
    limit: usize,
) -> Result<Vec<ToolResultPart>, String> {
    let metadata = tokio::fs::metadata(target)
        .await
        .map_err(|e| format!("stat failed: {e}"))?;

    if metadata.is_file() {
        let file = tokio::fs::File::open(target)
            .await
            .map_err(|e| format!("open failed: {e}"))?;
        return read_open_file(file, target, offset, limit).await;
    }

    if metadata.is_dir() {
        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(target)
            .await
            .map_err(|e| format!("read dir failed: {e}"))?;
        while let Some(entry) = dir
            .next_entry()
            .await
            .map_err(|e| format!("read dir entry failed: {e}"))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry
                .file_type()
                .await
                .map_err(|e| format!("stat dir entry failed: {e}"))?
                .is_dir();
            entries.push((name, is_dir));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let total = entries.len();
        let end = offset.saturating_add(limit).min(total);
        let mut body = String::new();
        for (name, is_dir) in entries.iter().take(end).skip(offset) {
            body.push_str(name);
            if *is_dir {
                body.push('/');
            }
            body.push('\n');
        }
        let shown = end.saturating_sub(offset);
        body.push_str(&format!("({shown} entries)\n"));
        if end < total {
            body.push_str("(More entries available. Use a higher offset.)\n");
        }
        return Ok(vec![ToolResultPart::text(format!(
            "<path>{}</path>\n<type>directory</type>\n<entries>\n{body}</entries>",
            target.display(),
        ))]);
    }

    Err("Target is neither a file nor a directory".into())
}

async fn read_open_file(
    file: tokio::fs::File,
    label: &Path,
    offset: usize,
    limit: usize,
) -> Result<Vec<ToolResultPart>, String> {
    use std::fmt::Write as _;
    use tokio::io::AsyncReadExt as _;

    let too_large = || {
        vec![ToolResultPart::text(format!(
            "File too large to read inline (limit {MAX_BINARY_READ_BYTES} bytes). Use an external tool to process this file."
        ))]
    };
    let metadata = file
        .metadata()
        .await
        .map_err(|e| format!("stat failed: {e}"))?;
    if !metadata.is_file() {
        return Err("Target is not a regular file".into());
    }
    if metadata.len() > MAX_BINARY_READ_BYTES {
        return Ok(too_large());
    }
    let mut bytes = Vec::new();
    let count = file
        .take(MAX_BINARY_READ_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    if count as u64 > MAX_BINARY_READ_BYTES {
        return Ok(too_large());
    }
    if let Some(mime_type) = detect_image_mime(&bytes) {
        let media = MediaPart::new(
            MediaKind::Image,
            mime_type.parse().ok(),
            MediaSource::Inline { data: bytes },
        )
        .map_err(|e| format!("invalid image media: {e}"))?;
        return Ok(vec![ToolResultPart::Attachment(Box::new(media))]);
    }
    let Ok(content) = String::from_utf8(bytes) else {
        return Ok(vec![ToolResultPart::text(format!(
            "Binary file '{}'; not a supported format (image/text). Use an external tool to process this file.",
            label.display(),
        ))]);
    };
    let total = content.lines().count();
    let end = offset.saturating_add(limit).min(total);
    let mut body = String::new();
    for (index, line) in content.lines().enumerate().take(end).skip(offset) {
        let _ = writeln!(body, "{:05}| {line}", index + 1);
    }
    if end < total {
        let _ = writeln!(
            body,
            "\n(File has more lines. Use 'offset' parameter to read beyond line {end})"
        );
    } else {
        let _ = writeln!(body, "\n(End of file - total {total} lines)");
    }
    Ok(vec![ToolResultPart::text(format!(
        "<path>{}</path>\n<type>file</type>\n<content>\n{body}</content>",
        label.display(),
    ))])
}

/// Read a regular file from an already selected workspace root. The kernel
/// confines path resolution; rendering never opens the requested path again.
/// Platforms without the required confinement primitive refuse the operation.
#[cfg(target_os = "linux")]
pub async fn render_confined_file(
    root: &Path,
    relative: &Path,
    offset: usize,
    limit: usize,
) -> Result<Vec<ToolResultPart>, String> {
    use rustix::fs::{self, FileType, Mode, OFlags, ResolveFlags};
    use std::os::fd::AsFd;
    use std::path::Component;

    if !root.is_absolute()
        || !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err("workspace path is not confined".into());
    }
    let root = root.to_owned();
    let relative = relative.to_owned();
    let label = relative.clone();
    let file = tokio::task::spawn_blocking(move || {
        // Only the host selects this root. Pin its resolved directory before
        // handling the peer-relative path, which still cannot follow links.
        let root = fs::openat2(
            fs::CWD,
            &root,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_MAGICLINKS,
        )?;
        let target = fs::openat2(
            &root,
            &relative,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )?;
        let stat = fs::fstat(&target)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink != 1 {
            return Err(std::io::Error::other(
                "workspace reads require a regular, single-link file",
            ));
        }
        // Reopen the verified inode via our own descriptor, not the user's path.
        use std::os::fd::AsRawFd as _;
        std::fs::File::open(format!("/proc/self/fd/{}", target.as_fd().as_raw_fd()))
    })
    .await
    .map_err(|e| format!("workspace read task failed: {e}"))?
    .map_err(|e| format!("workspace file unavailable: {e}"))?;
    read_open_file(tokio::fs::File::from_std(file), &label, offset, limit).await
}

#[cfg(not(target_os = "linux"))]
pub async fn render_confined_file(
    _root: &Path,
    _relative: &Path,
    _offset: usize,
    _limit: usize,
) -> Result<Vec<ToolResultPart>, String> {
    Err("confined workspace reads are not supported on this host".into())
}

/// List visible entries below an owner-selected root without following links,
/// crossing mounts, opening file contents, or initializing a workspace watcher.
#[cfg(target_os = "linux")]
pub async fn list_confined_workspace(
    root: &Path,
) -> Result<Vec<querymt_agent_index::FileIndexEntry>, String> {
    use querymt_agent_index::FileIndexEntry;
    use rustix::fs::{self, Dir, FileType, Mode, OFlags, ResolveFlags};

    const MAX_ENTRIES: usize = 10_000;
    const MAX_DEPTH: usize = 20;
    if !root.is_absolute() {
        return Err("workspace path is not absolute".into());
    }
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || -> std::io::Result<Vec<FileIndexEntry>> {
        let root = fs::openat2(
            fs::CWD,
            &root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_MAGICLINKS,
        )?;
        let mut stack = vec![(std::path::PathBuf::new(), Dir::read_from(&root)?, 0)];
        let mut entries = Vec::new();
        let mut visited = 0;
        while let Some((parent, dir, depth)) = stack.last_mut() {
            let Some(entry) = dir.read() else {
                stack.pop();
                continue;
            };
            visited += 1;
            if visited > MAX_ENTRIES {
                break;
            }
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name.starts_with(b".") {
                continue;
            }
            let Ok(name) = std::str::from_utf8(name) else {
                continue;
            };
            let relative = parent.join(name);
            let Ok(target) = fs::openat2(
                &root,
                &relative,
                OFlags::PATH | OFlags::CLOEXEC,
                Mode::empty(),
                ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
            ) else {
                continue;
            };
            let stat = fs::fstat(&target)?;
            let is_dir = match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => true,
                FileType::RegularFile if stat.st_nlink == 1 => false,
                _ => continue,
            };
            let path = relative.to_string_lossy().into_owned();
            if is_dir
                && *depth < MAX_DEPTH
                && let Ok(fd) = fs::openat(
                    &target,
                    ".",
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                && let Ok(dir) = Dir::new(fd)
            {
                let child_depth = *depth + 1;
                stack.push((relative, dir, child_depth));
            }
            entries.push(FileIndexEntry { path, is_dir });
        }
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.path.cmp(&b.path)));
        Ok(entries)
    })
    .await
    .map_err(|error| format!("workspace listing task failed: {error}"))?
    .map_err(|error| format!("workspace listing unavailable: {error}"))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn confined_read_returns_pages_and_refuses_to_inline_large_or_unsupported_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("notes.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(root.join("binary"), [0x80, 0x81]).unwrap();
        std::fs::File::create(root.join("large.txt"))
            .unwrap()
            .set_len(MAX_BINARY_READ_BYTES + 1)
            .unwrap();
        let read = |name, offset, limit| {
            let root = &root;
            async move {
                let output = render_confined_file(root, Path::new(name), offset, limit)
                    .await
                    .unwrap();
                let [ToolResultPart::Text { text }] = output.as_slice() else {
                    panic!("expected rendered text for {name}");
                };
                text.clone()
            }
        };
        let page = read("notes.txt", 1, 1).await;
        assert!(page.contains("00002| two"));
        assert!(page.contains("more lines"));
        assert!(!page.contains("00001| one") && !page.contains("00003| three"));
        let tail = read("notes.txt", 2, 1).await;
        assert!(tail.contains("00003| three") && tail.contains("End of file"));
        assert!(
            read("binary", 0, 1)
                .await
                .contains("not a supported format")
        );
        assert!(read("large.txt", 0, 1).await.contains("File too large"));
    }

    #[tokio::test]
    async fn confined_read_accepts_a_host_selected_root_alias() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkout");
        let alias = dir.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("notes.txt"), "workspace fixture\n").unwrap();
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let blocks = render_confined_file(&alias, Path::new("notes.txt"), 0, 1)
            .await
            .unwrap();
        assert!(
            matches!(&blocks[..], [ToolResultPart::Text { text }] if text.contains("workspace fixture"))
        );
    }

    #[tokio::test]
    async fn confined_read_requires_a_regular_single_link_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("original"), "shared fixture").unwrap();
        std::fs::hard_link(root.join("original"), root.join("second-name")).unwrap();
        std::fs::create_dir(root.join("directory")).unwrap();
        for name in ["second-name", "directory"] {
            let error = render_confined_file(&root, Path::new(name), 0, 1)
                .await
                .unwrap_err();
            assert!(error.contains("regular, single-link file"), "{error}");
        }
        std::fs::remove_file(root.join("second-name")).unwrap();
        assert!(
            render_confined_file(&root, Path::new("original"), 0, 1)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn confined_listing_skips_symlinks_hard_links_and_nested_hidden_entries() {
        use std::{fs, os::unix::fs::symlink};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        let outside = dir.path().join("outside");
        fs::create_dir_all(root.join("src/.hidden")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root.join("src/visible.txt"), "visible").unwrap();
        fs::write(root.join("src/.hidden/file.txt"), "hidden").unwrap();
        fs::write(outside.join("file.txt"), "outside").unwrap();
        fs::write(dir.path().join("shared.txt"), "hard-linked").unwrap();

        symlink(outside.join("file.txt"), root.join("file-link")).unwrap();
        symlink(&outside, root.join("dir-link")).unwrap();
        // An in-root link also exercises the symlink ban, not just containment.
        symlink("visible.txt", root.join("src/file-alias")).unwrap();
        fs::hard_link(dir.path().join("shared.txt"), root.join("hard-link")).unwrap();

        let entries = list_confined_workspace(&root).await.unwrap();
        let mut listed: Vec<_> = entries
            .iter()
            .map(|e| (e.path.as_str(), e.is_dir))
            .collect();
        listed.sort_unstable();
        assert_eq!(listed, [("src", true), ("src/visible.txt", false)]);
    }
}
