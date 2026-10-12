//! Shared gix-based repository fixtures for worktree unit tests.
//!
//! Only compiled for tests. All fixtures build repositories through `gix`
//! object APIs and materialize working-tree files directly; no Git CLI is
//! used for fixture setup (the CLI adapter tests exercise the adapter
//! itself).

#![cfg(test)]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Initialize a non-bare repository with a commit identity in its config
/// (mirroring the snapshot backend's config handling).
pub(crate) fn init_repo() -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().unwrap();
    gix::init(tmp.path()).unwrap();
    let config_path = tmp.path().join(".git/config");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("\n[user]\n\tname = test\n\temail = test@example.com\n");
    fs::write(&config_path, config).unwrap();
    tmp
}

/// Recursively write nested trees for `(relative_path, blob)` entries and
/// return the root tree id.
pub(crate) fn write_tree_recursive(
    repo: &gix::Repository,
    entries: Vec<(String, gix::ObjectId)>,
) -> gix::ObjectId {
    let mut blobs: Vec<(String, gix::ObjectId)> = Vec::new();
    let mut subdirs: BTreeMap<String, Vec<(String, gix::ObjectId)>> = BTreeMap::new();

    for (path, oid) in entries {
        if let Some((dir, rest)) = path.split_once('/') {
            subdirs
                .entry(dir.to_string())
                .or_default()
                .push((rest.to_string(), oid));
        } else {
            blobs.push((path, oid));
        }
    }

    let mut tree_entries: Vec<gix::objs::tree::Entry> = Vec::new();

    for (dir_name, sub_entries) in subdirs {
        let sub_tree_id = write_tree_recursive(repo, sub_entries);
        tree_entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: dir_name.as_str().into(),
            oid: sub_tree_id,
        });
    }

    for (name, oid) in blobs {
        tree_entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Blob.into(),
            filename: name.as_str().into(),
            oid,
        });
    }

    tree_entries.sort();
    repo.write_object(&gix::objs::Tree {
        entries: tree_entries,
    })
    .unwrap()
    .detach()
}

/// Write `files` as blobs/trees and commit them on the current HEAD,
/// returning the new commit id.
pub(crate) fn commit_files(root: &Path, message: &str, files: &[(&str, &str)]) -> gix::ObjectId {
    let repo = gix::open(root).unwrap();
    let entries: Vec<(String, gix::ObjectId)> = files
        .iter()
        .map(|(path, content)| {
            let oid = repo.write_blob(content.as_bytes()).unwrap().detach();
            (path.to_string(), oid)
        })
        .collect();

    let tree_id = write_tree_recursive(&repo, entries);

    let parent_ids: Vec<gix::ObjectId> = match repo.head_commit() {
        Ok(commit) => vec![commit.id().detach()],
        Err(_) => Vec::new(),
    };

    repo.commit("HEAD", message, tree_id, parent_ids)
        .unwrap()
        .detach()
}

/// Write `files` into the working tree at `root` (gix fixtures create
/// objects in the ODB only; the checkout must be materialized too).
pub(crate) fn materialize(root: &Path, files: &[(&str, &str)]) {
    for (path, content) in files {
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(target, content).unwrap();
    }
}

/// Standard content history: commit 1 has `file.txt`; commit 2 adds
/// `sub/file.txt`. Both checkouts are materialized on disk. Returns the
/// temp dir and both commit ids.
pub(crate) fn init_repo_with_subdir() -> (tempfile::TempDir, gix::ObjectId, gix::ObjectId) {
    let tmp = init_repo();
    let first = commit_files(tmp.path(), "first", &[("file.txt", "one")]);
    materialize(tmp.path(), &[("file.txt", "one")]);
    let second = commit_files(
        tmp.path(),
        "add sub",
        &[("file.txt", "one"), ("sub/file.txt", "sub")],
    );
    materialize(tmp.path(), &[("file.txt", "one"), ("sub/file.txt", "sub")]);
    (tmp, first, second)
}
