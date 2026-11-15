//! Where the run-lease files of a sessions database live.

use std::path::{Path, PathBuf};

/// Lease root for a sessions database: `<db_dir>/<db_stem>.run-leases`.
///
/// Returns `None` for databases without a stable on-disk location
/// (in-memory databases).
pub fn run_lease_root_for_db(db_path: &Path) -> Option<PathBuf> {
    if db_path.as_os_str().is_empty() || db_path == Path::new(":memory:") {
        return None;
    }
    let stem = db_path.file_stem()?;
    let mut dir_name = stem.to_os_string();
    dir_name.push(".run-leases");
    Some(match db_path.parent() {
        Some(parent) => parent.join(dir_name),
        None => PathBuf::from(dir_name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_root_follows_database_path() {
        assert_eq!(
            run_lease_root_for_db(Path::new("/home/u/.qmt/sessions.db")),
            Some(PathBuf::from("/home/u/.qmt/sessions.run-leases"))
        );
        assert_eq!(run_lease_root_for_db(Path::new(":memory:")), None);
    }

    #[cfg(windows)]
    #[test]
    fn lease_root_follows_windows_database_path() {
        assert_eq!(
            run_lease_root_for_db(Path::new(r"C:\Users\x\.qmt\sessions.db")),
            Some(PathBuf::from(r"C:\Users\x\.qmt\sessions.run-leases"))
        );
    }
}
