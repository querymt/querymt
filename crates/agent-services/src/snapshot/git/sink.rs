//! The single route for every object write and object presence check of a
//! snapshot store.
//!
//! A store of a managed workspace is *linked* to its source repository: it
//! lists the source's object directory in `objects/info/alternates` and names
//! the workspace's retained baseline commit in [`BASELINE_FILE`]. Git's own
//! write path (`Repository::write_blob`, `write_object`, `has_object`) follows
//! alternates, so it would skip content the source merely happens to hold:
//! blobs the workspace staged or committed, or objects of a branch about to be
//! deleted. Nothing pins those. The sink therefore trusts only
//!
//! - objects in the store's own loose `objects/`, and
//! - the *baseline set*: every tree and blob reachable from the baseline
//!   commit's tree, which `refs/qmt/baselines/<id>` keeps alive,
//!
//! and writes everything else into the store's own loose `objects/`,
//! bypassing the alternates-aware existence check. An unlinked store behaves
//! as a plain repository.

use super::gc::mark_tree;
use crate::snapshot::backend::{SnapshotError, SnapshotResult};
use gix::hashtable::HashSet;
use gix::objs::Write as _;
use std::cell::OnceCell;
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

/// File in the snapshot git dir that names the linked baseline commit.
pub(super) const BASELINE_FILE: &str = "qmt-baseline";

type BaselineSet = HashSet<gix::ObjectId>;

/// How many (store, baseline commit) entries [`BASELINE_SETS`] keeps. A set
/// can hold hundreds of thousands of ids, so a long-running process with many
/// workspaces must not keep one per store forever.
const BASELINE_CACHE_ENTRIES: usize = 8;

/// Baseline sets by (store git dir, baseline commit), least recently used
/// first. A baseline commit is immutable, so a set never goes stale.
#[derive(Default)]
struct BaselineCache {
    entries: VecDeque<((PathBuf, gix::ObjectId), Arc<BaselineSet>)>,
}

impl BaselineCache {
    /// The set cached for `key`, marked most recently used. Never shared
    /// across stores: a set is only cached for a store that could read the
    /// whole baseline tree.
    fn get(&mut self, key: &(PathBuf, gix::ObjectId)) -> Option<Arc<BaselineSet>> {
        let pos = self.entries.iter().position(|(k, _)| k == key)?;
        let entry = self.entries.remove(pos)?;
        let set = entry.1.clone();
        self.entries.push_back(entry);
        Some(set)
    }

    /// Cache `set` for `key`. A store keeps only its latest baseline, and the
    /// least recently used entries go beyond [`BASELINE_CACHE_ENTRIES`].
    fn insert(&mut self, key: (PathBuf, gix::ObjectId), set: Arc<BaselineSet>) {
        self.entries.retain(|((store, _), _)| *store != key.0);
        self.entries.push_back((key, set));
        while self.entries.len() > BASELINE_CACHE_ENTRIES {
            self.entries.pop_front();
        }
    }
}

/// Only locked for short updates, never while a set is collected.
static BASELINE_SETS: LazyLock<Mutex<BaselineCache>> =
    LazyLock::new(|| Mutex::new(BaselineCache::default()));

/// What a linked store trusts besides its own loose objects.
struct Linked {
    /// `None` when [`BASELINE_FILE`] is missing or unreadable: nothing is
    /// trusted, so every object is written to the store.
    commit: Option<gix::ObjectId>,
    set: OnceCell<Arc<BaselineSet>>,
}

/// Writes objects to, and asks about objects in, one snapshot store.
pub(crate) struct ObjectSink<'r> {
    repo: &'r gix::Repository,
    /// Blobs of at least this many bytes are never taken from the source.
    stream_bytes: u64,
    linked: Option<Linked>,
}

impl<'r> ObjectSink<'r> {
    pub(super) fn new(repo: &'r gix::Repository, stream_bytes: u64) -> Self {
        let git_dir = repo.git_dir();
        let baseline = fs::read_to_string(git_dir.join(BASELINE_FILE));
        let has_alternates = git_dir.join("objects/info/alternates").is_file();
        let linked = match baseline {
            Ok(text) => Some(Linked {
                commit: gix::ObjectId::from_hex(text.trim().as_bytes()).ok(),
                set: OnceCell::new(),
            }),
            // An alternate without a baseline must still never be trusted.
            Err(_) if has_alternates => Some(Linked {
                commit: None,
                set: OnceCell::new(),
            }),
            Err(_) => None,
        };
        Self {
            repo,
            stream_bytes,
            linked,
        }
    }

    /// The store's own loose object file for `id`.
    fn own_loose_path(&self, id: &gix::oid) -> PathBuf {
        let hex = id.to_hex().to_string();
        self.repo
            .git_dir()
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..])
    }

    fn own_loose_contains(&self, id: &gix::oid) -> bool {
        self.own_loose_path(id).is_file()
    }

    fn baseline_set(&self, linked: &Linked) -> Arc<BaselineSet> {
        linked
            .set
            .get_or_init(|| match linked.commit {
                None => Arc::new(BaselineSet::default()),
                Some(commit) => self.load_baseline_set(commit),
            })
            .clone()
    }

    fn load_baseline_set(&self, commit: gix::ObjectId) -> Arc<BaselineSet> {
        let key = (self.repo.git_dir().to_path_buf(), commit);
        if let Some(set) = BASELINE_SETS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return set;
        }
        match self.collect_baseline(commit) {
            Ok(set) => {
                let set = Arc::new(set);
                BASELINE_SETS
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(key, set.clone());
                set
            }
            Err(e) => {
                // Not cached, so a later snapshot retries. Until then every
                // object is written to the store.
                tracing::debug!(%commit, error = %e, "baseline commit unreadable; snapshot store trusts no linked objects");
                Arc::new(BaselineSet::default())
            }
        }
    }

    /// Tree and blob ids below the baseline commit's tree, without loading blobs.
    fn collect_baseline(&self, commit: gix::ObjectId) -> SnapshotResult<BaselineSet> {
        let tree_id = self
            .repo
            .find_commit(commit)
            .map_err(|e| SnapshotError::NotFound(format!("baseline commit {commit}: {e}")))?
            .tree_id()
            .map_err(|e| SnapshotError::Repository(format!("Failed to get tree: {e}")))?
            .detach();
        let mut set = BaselineSet::default();
        mark_tree(self.repo, tree_id, &mut set)?;
        Ok(set)
    }

    pub(super) fn git_dir(&self) -> &Path {
        self.repo.git_dir()
    }

    /// Whether `id`, a blob of `size` bytes or a tree, is safely held: in an
    /// unlinked store anywhere in it, in a linked store only in its own loose
    /// `objects/` or, below the large-file threshold, in the baseline set.
    pub(super) fn contains(&self, id: gix::ObjectId, size: u64) -> bool {
        match &self.linked {
            None => self.repo.has_object(id),
            Some(linked) => {
                self.own_loose_contains(&id)
                    || (size < self.stream_bytes && self.baseline_set(linked).contains(&id))
            }
        }
    }

    /// Write `data` as an object of `kind` into the store's own loose
    /// `objects/` unless it is already trusted.
    fn write_linked(
        &self,
        linked: &Linked,
        kind: gix::objs::Kind,
        data: &[u8],
    ) -> SnapshotResult<gix::ObjectId> {
        let id = gix::objs::compute_hash(self.repo.object_hash(), kind, data)
            .map_err(|e| SnapshotError::Repository(format!("Failed to hash {kind}: {e}")))?;
        let baseline_eligible = match kind {
            gix::objs::Kind::Blob => (data.len() as u64) < self.stream_bytes,
            gix::objs::Kind::Tree => true,
            gix::objs::Kind::Commit | gix::objs::Kind::Tag => false,
        };
        if self.own_loose_contains(&id)
            || (baseline_eligible && self.baseline_set(linked).contains(&id))
        {
            return Ok(id);
        }
        let store = gix::odb::loose::Store::at_opts(
            self.repo.git_dir().join("objects"),
            self.repo.object_hash(),
            gix::odb::loose::Options::default(),
        );
        store
            .write_buf(kind, data)
            .map_err(|e| SnapshotError::Repository(format!("Failed to write {kind}: {e}")))
    }

    /// Write a blob held in memory.
    pub(super) fn write_blob(&self, data: &[u8]) -> SnapshotResult<gix::ObjectId> {
        match &self.linked {
            None => self
                .repo
                .write_blob(data)
                .map(gix::Id::detach)
                .map_err(|e| SnapshotError::Repository(format!("Failed to write blob: {e}"))),
            Some(linked) => self.write_linked(linked, gix::objs::Kind::Blob, data),
        }
    }

    /// Write a tree or commit.
    pub(super) fn write_object(
        &self,
        object: &impl gix::objs::WriteTo,
    ) -> SnapshotResult<gix::ObjectId> {
        let kind = object.kind();
        let Some(linked) = &self.linked else {
            return self
                .repo
                .write_object(object)
                .map(gix::Id::detach)
                .map_err(|e| SnapshotError::Repository(format!("Failed to write {kind}: {e}")));
        };
        let mut buf = Vec::new();
        object
            .write_to(&mut buf)
            .map_err(|e| SnapshotError::Repository(format!("Failed to encode {kind}: {e}")))?;
        self.write_linked(linked, kind, &buf)
    }

    /// Stream exactly `size` bytes from `source` into a blob in the store's own
    /// loose `objects/`. `Ok(None)` if the source ended early.
    ///
    /// Unlike `write_blob`, the loose-object writer compresses before checking
    /// whether the object already exists, so this is only used for large files:
    /// they are always written, which keeps restoring them a streamed read of a
    /// loose object, never a load of a packed source object.
    pub(super) fn write_stream_blob(
        &self,
        source: impl std::io::Read,
        size: u64,
    ) -> SnapshotResult<Option<gix::ObjectId>> {
        // Large blobs rarely compress; storing them as zlib "stored" blocks
        // keeps the object readable by git at the cost of a copy, not a deflate.
        let store = gix::odb::loose::Store::at_opts(
            self.repo.git_dir().join("objects"),
            self.repo.object_hash(),
            gix::odb::loose::Options {
                compression: gix::zlib::Compression::NONE,
                alloc_limit_bytes: None,
            },
        );
        let mut reader = ExactReader::new(source.take(size), size);
        match store.write_stream(gix::objs::Kind::Blob, size, &mut reader) {
            Ok(oid) => Ok(Some(oid)),
            Err(_) if reader.shrank() => Ok(None),
            Err(e) => Err(SnapshotError::Repository(format!(
                "Failed to write blob: {e}"
            ))),
        }
    }
}

/// Reader that fails with `UnexpectedEof` (and remembers it) if the source
/// ends before the declared number of bytes was produced.
struct ExactReader<R> {
    inner: R,
    remaining: u64,
    shrank: bool,
}

impl<R: std::io::Read> ExactReader<R> {
    fn new(inner: R, expected: u64) -> Self {
        Self {
            inner,
            remaining: expected,
            shrank: false,
        }
    }

    fn shrank(&self) -> bool {
        self.shrank
    }
}

impl<R: std::io::Read> std::io::Read for ExactReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 && self.remaining > 0 && !buf.is_empty() {
            self.shrank = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "file shrank while being snapshotted",
            ));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    fn set_of(byte: u8) -> Arc<BaselineSet> {
        Arc::new(BaselineSet::from_iter([
            gix::ObjectId::from_bytes_or_panic(&[byte; 20]),
        ]))
    }

    fn key(store: &str, byte: u8) -> (PathBuf, gix::ObjectId) {
        (
            PathBuf::from(store),
            gix::ObjectId::from_bytes_or_panic(&[byte; 20]),
        )
    }

    #[test]
    fn baseline_cache_keeps_the_latest_baseline_per_store() {
        let mut cache = BaselineCache::default();
        cache.insert(key("s", 1), set_of(1));
        cache.insert(key("s", 2), set_of(2));
        assert!(
            cache.get(&key("s", 1)).is_none(),
            "the old baseline is dropped"
        );
        assert_eq!(cache.get(&key("s", 2)), Some(set_of(2)));
        assert!(
            cache.get(&key("other", 2)).is_none(),
            "a set is never shared across stores"
        );
    }

    #[test]
    fn baseline_cache_is_bounded_and_evicts_the_least_recently_used() {
        let mut cache = BaselineCache::default();
        for i in 0..BASELINE_CACHE_ENTRIES as u8 {
            cache.insert(key(&format!("s{i}"), i), set_of(i));
        }
        // Touch the oldest, so the second oldest goes first.
        assert!(cache.get(&key("s0", 0)).is_some());
        cache.insert(key("new", 99), set_of(99));
        assert_eq!(cache.entries.len(), BASELINE_CACHE_ENTRIES);
        assert!(cache.get(&key("s0", 0)).is_some());
        assert!(cache.get(&key("s1", 1)).is_none());
        assert!(cache.get(&key("new", 99)).is_some());
    }
}
