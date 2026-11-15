//! Protection of the user's own work during an explicit result acceptance
//! (step 4 of design decision 9).
//!
//! Acceptance holds the checkout's real `index.lock` for the whole
//! transaction ([`IndexLock`]), so Git and other QueryMT processes cannot
//! change the index underneath it. While the lock is held, the checkout's
//! local state is read through gix's status platform and any path the
//! acceptance would write that carries staged, unstaged, untracked, or ignored
//! state is refused: the user's own work is never swept into the commit or
//! overwritten. Everything here is read-only apart from creating and, on
//! drop, removing the lock file QueryMT itself created.

use crate::worktree::domain::ChangedPath;
use crate::worktree::error::WorktreeResult;
use crate::worktree::integrate::{Refusal, Stop, refuse};
use crate::worktree::repo_ops::{FlatTree, flatten_tree, native_err};
use gix::bstr::ByteSlice;
use gix::hash::ObjectId;
use std::collections::BTreeSet;
use std::ops::Bound;
use std::path::{Path, PathBuf};

/// Exclusive hold on a checkout's `index.lock`.
///
/// The lock is created with `O_EXCL` semantics, so an existing lock (another
/// Git process, or an interrupted operation) is never taken over or removed
/// here: acquisition simply refuses with `index_locked`. Dropping the value
/// removes the lock file this instance created.
pub struct IndexLock {
    file: gix::lock::File,
}

impl IndexLock {
    /// Take the lock for the index at `index_path` without waiting.
    pub(crate) fn acquire(index_path: &Path) -> Result<Self, Stop> {
        use gix::lock::{File, acquire::Fail};
        match File::acquire_to_update_resource(index_path, Fail::Immediately, None, 0) {
            Ok(file) => Ok(Self { file }),
            Err(err) => {
                let mut lock_path = index_path.as_os_str().to_owned();
                lock_path.push(".lock");
                let lock_path = PathBuf::from(lock_path);
                if lock_path.exists() {
                    Err(refuse(
                        "index_locked",
                        format!(
                            "{} exists: another Git process or an interrupted operation holds \
                             the index; QueryMT never removes a lock it does not own",
                            lock_path.display()
                        ),
                    ))
                } else {
                    Err(native_err(err).into())
                }
            }
        }
    }

    /// Path of the lock file.
    pub fn path(&self) -> &Path {
        self.file.lock_path()
    }

    /// Abandon the lock *without* removing the file, as a killed process
    /// would. Fault injection uses this to leave a stale lock behind.
    pub(crate) fn leak(self) {
        std::mem::forget(self);
    }

    /// Write `index` through the lock and atomically replace the real index
    /// with it, releasing the lock. The lock was taken before anything else
    /// in the transaction, so nothing can have changed the index since.
    pub(crate) fn commit_index(mut self, index: &gix::index::File) -> WorktreeResult<()> {
        let mut bytes = Vec::new();
        index
            .write_to(&mut bytes, gix::index::write::Options::default())
            .map_err(native_err)?;
        self.file.with_mut(|file| {
            use std::io::Write;
            file.write_all(&bytes)?;
            file.sync_all()
        })?;
        self.file
            .commit()
            .map(|_| ())
            .map_err(|err| native_err(err.error))
    }
}

/// Stable identity of the lock file at `path`: device and inode where the
/// platform has them, plus the creation time when available. Persisted in
/// the integration intent so recovery can prove that a surviving
/// `index.lock` is the one QueryMT created rather than a lock another
/// process took later. `None` when nothing stable can be read; such a lock
/// is never treated as ours.
pub(crate) fn lock_identity(path: &Path) -> Option<String> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let birth = metadata
        .created()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_nanos().to_string());
    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt;
        Some(format!("{}-{}", metadata.dev(), metadata.ino()))
    };
    #[cfg(not(unix))]
    let inode: Option<String> = None;
    if inode.is_none() && birth.is_none() {
        return None;
    }
    Some(format!(
        "{}:{}",
        inode.as_deref().unwrap_or("-"),
        birth.as_deref().unwrap_or("-")
    ))
}

impl std::fmt::Debug for IndexLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("IndexLock").field(&self.path()).finish()
    }
}

impl PartialEq for IndexLock {
    fn eq(&self, other: &Self) -> bool {
        self.path() == other.path()
    }
}

impl Eq for IndexLock {}

/// Everything in the checkout that is not a clean copy of HEAD.
#[derive(Debug, Default)]
pub(crate) struct LocalState {
    /// Every path with staged, unstaged, untracked, or ignored state. Ignored
    /// and untracked directories that the status walk reports as a whole
    /// appear as the directory path.
    pub occupied: BTreeSet<String>,
    /// Paths with unmerged (stage > 0) index entries; sorted, unique.
    pub unmerged: Vec<String>,
}

/// Read the checkout's local state with the status platform.
///
/// `head_tree` is the tree of the checkout's current HEAD; the staged part is
/// the difference between it and the real index. The handle must have been
/// opened so untracked files are never hidden (see
/// [`open_acceptance_checkout`](crate::worktree::repo_ops::open_acceptance_checkout)).
pub(crate) fn scan_local_state(
    repo: &gix::Repository,
    head_tree: ObjectId,
) -> WorktreeResult<LocalState> {
    use gix::status::index_worktree::Item;

    let mut state = LocalState::default();

    // Index vs working tree, plus untracked and ignored discovery.
    let platform = repo
        .status(gix::progress::Discard)
        .map_err(native_err)?
        .untracked_files(gix::status::UntrackedFiles::Files)
        .dirwalk_options(|mut options| {
            options.set_emit_ignored(Some(gix::dir::walk::EmissionMode::Matching));
            options
        });
    for item in platform
        .into_index_worktree_iter(Vec::new())
        .map_err(native_err)?
    {
        let item = item.map_err(native_err)?;
        match &item {
            // Entries whose only news is a refreshable stat are clean.
            Item::Modification { rela_path, .. } if item.summary().is_some() => {
                state.occupied.insert(rela_path.to_str_lossy().into_owned());
            }
            Item::Modification { .. } => {}
            Item::DirectoryContents { entry, .. }
            | Item::Rewrite {
                dirwalk_entry: entry,
                ..
            } => {
                if matches!(
                    entry.status,
                    gix::dir::entry::Status::Untracked | gix::dir::entry::Status::Ignored(_)
                ) {
                    state
                        .occupied
                        .insert(entry.rela_path.to_str_lossy().into_owned());
                }
            }
        }
    }

    // HEAD tree vs real index: staged additions, modifications, deletions,
    // mode changes, and unmerged entries.
    let mut head_entries = FlatTree::new();
    flatten_tree(repo, head_tree, &mut head_entries, String::new())?;
    if repo.index_path().exists() {
        let index = repo.open_index().map_err(native_err)?;
        for entry in index.entries() {
            let path = entry.path(&index);
            if entry.stage() != gix::index::entry::Stage::Unconflicted {
                state.unmerged.push(path.to_str_lossy().into_owned());
                continue;
            }
            match head_entries.remove(path) {
                Some((mode, id)) if mode == entry.mode && id == entry.id => {}
                _ => {
                    state.occupied.insert(path.to_str_lossy().into_owned());
                }
            }
        }
    }
    // Whatever is left was removed from the index.
    state.occupied.extend(
        head_entries
            .keys()
            .map(|path| path.to_str_lossy().into_owned()),
    );
    state.unmerged.sort();
    state.unmerged.dedup();
    Ok(state)
}

/// Whether `path` is occupied: equal to an occupied path, inside an occupied
/// directory (or below an occupied file that would have to become a
/// directory), or a directory that contains an occupied path.
fn overlaps(occupied: &BTreeSet<String>, path: &str) -> bool {
    if occupied.contains(path) {
        return true;
    }
    let mut end = path.len();
    while let Some(slash) = path[..end].rfind('/') {
        if occupied.contains(&path[..slash]) {
            return true;
        }
        end = slash;
    }
    let prefix = format!("{path}/");
    occupied
        .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
        .next()
        .is_some_and(|candidate| candidate.starts_with(&prefix))
}

/// Refuse when the checkout has unresolved merge entries, or when any path in
/// `touched` carries local state. Both refusals name the offending paths.
pub(crate) fn protect_local_work(
    repo: &gix::Repository,
    head_tree: ObjectId,
    touched: &[ChangedPath],
) -> Result<(), Stop> {
    let state = scan_local_state(repo, head_tree)?;
    if !state.unmerged.is_empty() {
        return Err(Refusal::new(
            "unmerged_entries",
            "the checkout has unresolved merge conflicts",
        )
        .with_paths(state.unmerged)
        .into());
    }
    let colliding: Vec<String> = touched
        .iter()
        .filter(|change| overlaps(&state.occupied, &change.path))
        .map(|change| change.path.clone())
        .collect();
    if colliding.is_empty() {
        return Ok(());
    }
    Err(Refusal::new(
        "touches_local_changes",
        "the result touches paths that have local staged, unstaged, untracked or ignored \
         state; commit, stash or move that work first",
    )
    .with_paths(colliding)
    .into())
}

/// Delta paths on which the delegate's baseline already differed from the
/// HEAD it was captured on. Such paths carry the parent's uncommitted edits
/// at capture time, which the result commit also contains, so they cannot be
/// separated from the delegate's own work. Sorted by path.
pub(crate) fn inherited_baseline_edits(
    source: &FlatTree,
    baseline: &FlatTree,
    touched: &[ChangedPath],
) -> Vec<String> {
    let inherited: BTreeSet<String> = crate::worktree::repo_ops::diff_flattened(source, baseline)
        .into_iter()
        .map(|change| change.path)
        .collect();
    touched
        .iter()
        .filter(|change| inherited.contains(&change.path))
        .map(|change| change.path.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::domain::ChangeKind;

    fn set(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    #[test]
    fn equal_paths_overlap() {
        assert!(overlaps(&set(&["a/b.txt"]), "a/b.txt"));
        assert!(!overlaps(&set(&["a/b.txt"]), "a/c.txt"));
    }

    /// A file replacing a directory, a directory replacing a file, and a path
    /// inside an ignored directory reported as a whole all collide.
    #[test]
    fn directory_and_file_containment_overlaps() {
        // Occupied directory (or file) is an ancestor of the touched path.
        assert!(overlaps(&set(&["build"]), "build/out/x.o"));
        assert!(overlaps(&set(&["lib"]), "lib/mod.rs"));
        // Touched path is an ancestor of occupied paths.
        assert!(overlaps(&set(&["docs/notes/a.md"]), "docs"));
        assert!(overlaps(&set(&["docs/notes/a.md"]), "docs/notes"));
    }

    #[test]
    fn siblings_with_a_shared_name_prefix_do_not_overlap() {
        let occupied = set(&["src/lib.rs", "docs-old/a.md", "build"]);
        assert!(!overlaps(&occupied, "src/lib.rs.bak"));
        assert!(!overlaps(&occupied, "docs"));
        assert!(!overlaps(&occupied, "builder/x"));
        assert!(!overlaps(&occupied, "src/lib"));
        assert!(!overlaps(&BTreeSet::new(), "anything"));
    }

    fn change(path: &str) -> ChangedPath {
        ChangedPath {
            path: path.into(),
            kind: ChangeKind::Modified,
        }
    }

    #[test]
    fn inherited_edits_are_the_baseline_differences_the_result_touches() {
        let blob = |byte: u8| {
            (
                gix::index::entry::Mode::FILE,
                ObjectId::from_bytes_or_panic(&[byte; 20]),
            )
        };
        let key = |path: &str| gix::bstr::BString::from(path);
        let source: FlatTree = [
            (key("a"), blob(1)),
            (key("b"), blob(1)),
            (key("c"), blob(1)),
        ]
        .into_iter()
        .collect();
        // The parent had uncommitted edits to `a` and `c` when it captured.
        let baseline: FlatTree = [
            (key("a"), blob(2)),
            (key("b"), blob(1)),
            (key("c"), blob(2)),
        ]
        .into_iter()
        .collect();

        assert_eq!(
            inherited_baseline_edits(&source, &baseline, &[change("b"), change("c")]),
            vec!["c".to_string()]
        );
        assert!(inherited_baseline_edits(&source, &baseline, &[change("b")]).is_empty());
    }
}
