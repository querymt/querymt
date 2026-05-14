// QueryMT — ACP CWD Compatibility Helpers
//
// ACP's wire protocol requires `cwd: PathBuf` (absolute path) in session
// requests. QueryMT's domain model treats cwd as `Option<PathBuf>`. This
// module bridges the gap by providing a cross-platform "no cwd" sentinel
// that satisfies ACP deserialization but maps to `None` internally.
//
// Embedded/mobile hosts that cannot provide a meaningful cwd should omit
// the field in JSON; the FFI adapter normalizes it to `no_cwd_path()`.
// Non-mobile FFI hosts that have a real cwd should pass it through normally.
// ─────────────────────────────────────────────────────────────────────────────

use std::path::{Path, PathBuf};

/// Platform-specific sentinel path used to represent "no cwd" in ACP requests.
///
/// The sentinel is absolute (required by ACP schema) and intentionally
/// non-existent. It is recognized explicitly by [`is_no_cwd_path`] and
/// [`acp_cwd_to_optional`] so that accidental creation of the directory
/// does not silently trigger workspace indexing.
pub fn no_cwd_path() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(r"C:\querymt-no-cwd-sentinel")
    }

    #[cfg(not(windows))]
    {
        PathBuf::from("/tmp/querymt-no-cwd-sentinel")
    }
}

/// Returns `true` if `path` is the "no cwd" sentinel.
///
/// Both the exact sentinel and an empty path are treated as "no cwd".
pub fn is_no_cwd_path(path: &Path) -> bool {
    path.as_os_str().is_empty() || path == no_cwd_path()
}

/// Convert an ACP `cwd` field to QueryMT's optional-cwd semantics.
///
/// - Empty path -> `Ok(None)`
/// - Sentinel path -> `Ok(None)`
/// - Non-absolute path -> `Err(invalid_params)`
/// - Valid absolute path -> `Ok(Some(path))`
pub fn acp_cwd_to_optional(
    cwd: &Path,
) -> Result<Option<PathBuf>, agent_client_protocol::schema::Error> {
    if cwd.as_os_str().is_empty() || cwd == no_cwd_path() {
        return Ok(None);
    }

    if !cwd.is_absolute() {
        return Err(agent_client_protocol::schema::Error::invalid_params().data(
            serde_json::json!({
                "message": "cwd must be an absolute path",
                "cwd": cwd.display().to_string(),
            }),
        ));
    }

    Ok(Some(cwd.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_cwd_path_is_absolute() {
        let path = no_cwd_path();
        assert!(path.is_absolute(), "sentinel must be absolute: {:?}", path);
    }

    #[test]
    fn empty_path_is_no_cwd() {
        assert!(is_no_cwd_path(Path::new("")));
    }

    #[test]
    fn sentinel_is_no_cwd() {
        assert!(is_no_cwd_path(&no_cwd_path()));
    }

    #[test]
    fn empty_path_maps_to_none() {
        let result = acp_cwd_to_optional(Path::new("")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn sentinel_maps_to_none() {
        let result = acp_cwd_to_optional(&no_cwd_path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn real_absolute_path_maps_to_some() {
        let cwd = if cfg!(windows) {
            PathBuf::from(r"C:\Users\test\project")
        } else {
            PathBuf::from("/home/user/project")
        };
        let result = acp_cwd_to_optional(&cwd).unwrap();
        assert_eq!(result, Some(cwd));
    }

    #[test]
    fn relative_path_errors() {
        let result = acp_cwd_to_optional(Path::new("relative/path"));
        assert!(result.is_err());
    }
}
