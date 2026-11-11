//! Workspace-confined reads, shared by `file://` mention snapshots and the
//! remote file proxy.
//!
//! The workspace is a [`CanonicalWorkspace`], the same identity the file index
//! is built for. The target is resolved once to find its location relative to
//! that root; the read then goes through a root directory handle
//! (`cap-primitives`), which refuses to follow any path component that leads
//! outside the root, so a component swapped for a symlink after the check
//! cannot redirect the read.
//!
//! Only regular files and directories are read. FIFOs, sockets and devices are
//! refused before they are opened, and files are opened non-blocking, so a
//! special file can never stall the caller.

use crate::index::CanonicalWorkspace;
use crate::tools::builtins::read_shared::{
    MAX_BINARY_READ_BYTES, detect_image_mime, render_dir_listing, render_file_bytes,
    too_large_output,
};
use cap_primitives::ambient_authority;
use cap_primitives::fs::{FollowSymlinks, OpenOptions};
use querymt::chat::ToolResultPart;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Bytes read before deciding whether a file is an image or text.
const HEADER_BYTES: u64 = 4096;
/// Source bytes read beyond the text budget. Line numbering only ever grows the
/// rendered text, so a cut file always renders past the budget and is reported
/// as truncated by the caller.
const TEXT_SLACK_BYTES: u64 = 4096;

/// Why a confined read was refused.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("not found")]
    NotFound,
    #[error("cannot resolve path: {0}")]
    Resolution(String),
    #[error("outside workspace")]
    OutsideWorkspace(String),
    #[error("not a regular file or directory")]
    NotRegular,
    #[error("{0}")]
    Unreadable(String),
    /// The target is text, but no text budget remains.
    #[error("text budget exhausted")]
    BudgetExhausted,
}

/// Result of a confined read.
#[derive(Debug)]
pub struct ConfinedRead {
    /// Canonical path of the target.
    pub path: PathBuf,
    pub parts: Vec<ToolResultPart>,
}

/// Read `target` (relative paths are joined to the workspace cwd) if it lies
/// inside `workspace`.
///
/// `max_text_bytes` bounds the text that will be retained: at most that many
/// bytes (plus a small slack) of a text file are read, and text is refused with
/// [`ReadError::BudgetExhausted`] when it is zero. Images are not text and are
/// read up to the usual attachment limit. `None` reads without a text budget.
pub async fn read_confined(
    workspace: &CanonicalWorkspace,
    target: &Path,
    offset: usize,
    limit: usize,
    max_text_bytes: Option<usize>,
) -> Result<ConfinedRead, ReadError> {
    let workspace = workspace.clone();
    let target = target.to_path_buf();
    tokio::task::spawn_blocking(move || {
        read_blocking(&workspace, &target, offset, limit, max_text_bytes)
    })
    .await
    .map_err(|error| ReadError::Unreadable(error.to_string()))?
}

fn read_blocking(
    workspace: &CanonicalWorkspace,
    target: &Path,
    offset: usize,
    limit: usize,
    max_text_bytes: Option<usize>,
) -> Result<ConfinedRead, ReadError> {
    let joined = if target.is_absolute() {
        target.to_path_buf()
    } else {
        workspace.cwd.join(target)
    };
    let canonical = dunce::canonicalize(&joined).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => ReadError::NotFound,
        _ => ReadError::Resolution(error.to_string()),
    })?;
    let relative = canonical
        .strip_prefix(&workspace.root)
        .map_err(|_| ReadError::OutsideWorkspace(canonical.display().to_string()))?;
    let relative = if relative.as_os_str().is_empty() {
        Path::new(".")
    } else {
        relative
    };

    let root = cap_primitives::fs::open_ambient_dir(&workspace.root, ambient_authority())
        .map_err(|error| ReadError::Unreadable(error.to_string()))?;
    let metadata =
        cap_primitives::fs::stat(&root, relative, FollowSymlinks::Yes).map_err(confined_error)?;
    let parts = if metadata.is_dir() {
        if max_text_bytes == Some(0) {
            return Err(ReadError::BudgetExhausted);
        }
        read_dir(&root, relative, &canonical, offset, limit)?
    } else if metadata.is_file() {
        read_file(&root, relative, &canonical, offset, limit, max_text_bytes)?
    } else {
        // Opening a FIFO for reading blocks until a writer appears.
        return Err(ReadError::NotRegular);
    };
    Ok(ConfinedRead {
        path: canonical,
        parts,
    })
}

/// Map an error from the confined root handle.
///
/// `cap-primitives` reports an escape attempt as `PermissionDenied` with a
/// fixed message and exposes no other way to tell it apart from an ordinary
/// permission error, so the message is what distinguishes the two.
fn confined_error(error: std::io::Error) -> ReadError {
    const ESCAPE_MESSAGE: &str = "a path led outside of the filesystem";
    match error.kind() {
        std::io::ErrorKind::NotFound => ReadError::NotFound,
        std::io::ErrorKind::PermissionDenied if error.to_string().contains(ESCAPE_MESSAGE) => {
            ReadError::OutsideWorkspace(error.to_string())
        }
        _ => ReadError::Unreadable(error.to_string()),
    }
}

fn read_dir(
    root: &std::fs::File,
    relative: &Path,
    display: &Path,
    offset: usize,
    limit: usize,
) -> Result<Vec<ToolResultPart>, ReadError> {
    let mut entries = Vec::new();
    for entry in cap_primitives::fs::read_dir(root, relative).map_err(confined_error)? {
        let entry = entry
            .map_err(|error| ReadError::Unreadable(format!("read dir entry failed: {error}")))?;
        let is_dir = entry
            .file_type()
            .map_err(|error| ReadError::Unreadable(format!("stat dir entry failed: {error}")))?
            .is_dir();
        entries.push((entry.file_name().to_string_lossy().to_string(), is_dir));
    }
    Ok(render_dir_listing(display, entries, offset, limit))
}

fn read_file(
    root: &std::fs::File,
    relative: &Path,
    display: &Path,
    offset: usize,
    limit: usize,
    max_text_bytes: Option<usize>,
) -> Result<Vec<ToolResultPart>, ReadError> {
    let mut options = OpenOptions::new();
    options.read(true);
    // A path swapped for a FIFO after the type check must not block the open.
    #[cfg(unix)]
    {
        use cap_primitives::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let mut file = cap_primitives::fs::open(root, relative, &options).map_err(confined_error)?;
    // The handle, not the path, is what gets inspected from here on.
    let metadata = file
        .metadata()
        .map_err(|error| ReadError::Unreadable(format!("stat failed: {error}")))?;
    if !metadata.is_file() {
        return Err(ReadError::NotRegular);
    }
    let len = metadata.len();
    if len > MAX_BINARY_READ_BYTES {
        return Ok(too_large_output(len));
    }

    let mut bytes = Vec::new();
    let read_error = |error: std::io::Error| ReadError::Unreadable(format!("read failed: {error}"));
    (&mut file)
        .take(HEADER_BYTES)
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    let is_image = detect_image_mime(&bytes).is_some();
    if !is_image && max_text_bytes == Some(0) {
        return Err(ReadError::BudgetExhausted);
    }
    let cap = match (is_image, max_text_bytes) {
        (false, Some(max)) => u64::try_from(max)
            .unwrap_or(u64::MAX)
            .saturating_add(TEXT_SLACK_BYTES)
            .min(MAX_BINARY_READ_BYTES),
        _ => MAX_BINARY_READ_BYTES,
    };
    let already = bytes.len() as u64;
    if already < cap {
        (&mut file)
            .take(cap - already)
            .read_to_end(&mut bytes)
            .map_err(read_error)?;
    }
    if !is_image && len > bytes.len() as u64 {
        // Cut mid-file: drop a trailing partial UTF-8 sequence, not the rest.
        if let Err(error) = std::str::from_utf8(&bytes)
            && error.error_len().is_none()
        {
            bytes.truncate(error.valid_up_to());
        }
    }
    render_file_bytes(bytes, display, offset, limit).map_err(ReadError::Unreadable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::canonical_workspace;
    use tempfile::TempDir;

    fn workspace() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        std::fs::create_dir(dir.path().join(".git")).expect("marker");
        dir
    }

    async fn read(
        cwd: &Path,
        target: &Path,
        max_text_bytes: Option<usize>,
    ) -> Result<ConfinedRead, ReadError> {
        let workspace = canonical_workspace(cwd).expect("workspace");
        read_confined(&workspace, target, 0, 10, max_text_bytes).await
    }

    fn text_of(read: &ConfinedRead) -> &str {
        match &read.parts[0] {
            ToolResultPart::Text { text } => text,
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reads_absolute_and_relative_paths_inside_the_workspace() {
        let dir = workspace();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let absolute = read(dir.path(), &dir.path().join("a.txt"), None)
            .await
            .unwrap();
        assert!(text_of(&absolute).contains("hello"));
        let relative = read(dir.path(), Path::new("a.txt"), None).await.unwrap();
        assert_eq!(relative.path, absolute.path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reads_through_a_symlinked_workspace_cwd() {
        let dir = workspace();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let outer = TempDir::new().unwrap();
        let alias = outer.path().join("alias");
        std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
        // Both the cwd and the target are spelled through the alias.
        let read = read(&alias, &alias.join("a.txt"), None).await.unwrap();
        assert!(text_of(&read).contains("hello"));
    }

    #[tokio::test]
    async fn refuses_paths_outside_the_workspace() {
        let dir = workspace();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "x").unwrap();
        let error = read(dir.path(), &outside.path().join("secret.txt"), None)
            .await
            .unwrap_err();
        assert!(matches!(error, ReadError::OutsideWorkspace(_)), "{error:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_symlinks_that_lead_outside() {
        let dir = workspace();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let error = read(dir.path(), Path::new("link/secret.txt"), None)
            .await
            .unwrap_err();
        assert!(matches!(error, ReadError::OutsideWorkspace(_)), "{error:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_fifos_without_blocking() {
        let dir = workspace();
        let fifo = dir.path().join("pipe");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path for the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read(dir.path(), Path::new("pipe"), None),
        )
        .await
        .expect("a FIFO must not block the read");
        assert!(matches!(result, Err(ReadError::NotRegular)), "{result:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn permission_errors_are_unreadable_not_outside() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = workspace();
        let file = dir.path().join("locked.txt");
        std::fs::write(&file, "x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&file).is_ok() {
            return; // Running as root: permissions are not enforced.
        }
        let error = read(dir.path(), Path::new("locked.txt"), None)
            .await
            .unwrap_err();
        assert!(matches!(error, ReadError::Unreadable(_)), "{error:?}");
    }

    #[tokio::test]
    async fn missing_target_is_not_found() {
        let dir = workspace();
        let error = read(dir.path(), Path::new("nope.txt"), None)
            .await
            .unwrap_err();
        assert!(matches!(error, ReadError::NotFound), "{error:?}");
    }

    #[tokio::test]
    async fn lists_directories_including_the_root() {
        let dir = workspace();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        let read = read(dir.path(), dir.path(), None).await.unwrap();
        assert!(text_of(&read).contains("a.txt"));
    }

    #[tokio::test]
    async fn exhausted_budget_refuses_text_without_reading() {
        let dir = workspace();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let error = read(dir.path(), Path::new("a.txt"), Some(0))
            .await
            .unwrap_err();
        assert!(matches!(error, ReadError::BudgetExhausted), "{error:?}");
        let error = read(dir.path(), dir.path(), Some(0)).await.unwrap_err();
        assert!(matches!(error, ReadError::BudgetExhausted), "{error:?}");
    }

    #[tokio::test]
    async fn text_reads_are_bounded_by_the_budget() {
        let dir = workspace();
        let content = "line of text\n".repeat(20_000);
        std::fs::write(dir.path().join("big.txt"), &content).unwrap();
        let workspace = canonical_workspace(dir.path()).unwrap();
        let read = read_confined(
            &workspace,
            Path::new("big.txt"),
            0,
            usize::MAX / 2,
            Some(100),
        )
        .await
        .unwrap();
        // Far less than the file was read, yet more than the budget keeps, so
        // the caller still sees that it must truncate.
        let text = text_of(&read);
        assert!(text.len() > 100, "{}", text.len());
        assert!(text.len() < content.len() / 2, "{}", text.len());
    }

    #[tokio::test]
    async fn images_are_read_even_without_text_budget() {
        let dir = workspace();
        // Smallest PNG signature + IHDR is enough for type detection.
        let png: [u8; 24] = [
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, b'I', b'H', b'D', b'R',
            0, 0, 0, 1, 0, 0, 0, 1,
        ];
        std::fs::write(dir.path().join("p.png"), png).unwrap();
        match read(dir.path(), Path::new("p.png"), Some(0)).await {
            Ok(read) => assert!(matches!(read.parts[0], ToolResultPart::Attachment(_))),
            Err(error) => panic!("image must not need text budget: {error:?}"),
        }
    }
}
