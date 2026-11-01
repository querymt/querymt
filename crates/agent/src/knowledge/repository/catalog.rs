//! Directory catalogs: progressive disclosure over the selected corpus.
//!
//! A catalog answers "what is in this directory?" with bounded, cited
//! pointers. It has two origins, always labelled:
//!
//! * **Authored** — the directory holds a selected `index.md` or `README.md`
//!   (first match wins, in that order) whose body links to selected corpus
//!   entries. The catalog follows the author's link order, cites the listing's
//!   own lines, and leaves the document untouched; it stays fetchable with
//!   `repo_knowledge_get`.
//! * **Derived** — there is no usable authored listing, so the catalog is
//!   synthesized *in memory* from the titles and descriptions of selected
//!   documents. Each entry cites the document it came from. Missing
//!   descriptions stay missing; nothing is generated and nothing is written.
//!
//! Only selected, authorized sources ever appear. A link that leaves the
//! corpus is counted but never named, so a catalog cannot reveal what exists
//! outside it. Producing a catalog changes no file, enables no delivery stage
//! and is never used as an orientation fallback.

use super::browse::{
    DEFAULT_PAGE, EntryKind, GetError, ListEntry, MAX_PAGE, invalid, load_entries, parse_cursor,
};
use super::query::normalize_relative_path;
use super::reconcile::ReconcileStatus;
use super::references::{Reference, extract_links, resolve_reference};
use super::service::RepositoryService;
use super::store::{StoredDocument, load_document};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// File names recognised as an authored directory listing, in precedence order.
pub const LISTING_NAMES: [&str; 2] = ["index.md", "README.md"];
/// Upper bound on links read from one listing.
const MAX_LINKS: usize = 500;

/// What a catalog entry points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogKind {
    /// A directory containing selected documents or files.
    Directory,
    /// A selected authored document.
    Document,
    /// A path-level record of a selected code file.
    File,
}

/// Where an entry is cited from. Always repository-relative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    pub path: String,
    /// 1-based line in `path`.
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub kind: CatalogKind,
    pub path: String,
    /// Document ID or `file:<path>`; directories have none.
    pub id: Option<String>,
    pub title: String,
    /// Authored description only; never synthesized.
    pub description: Option<String>,
    pub status: Option<String>,
    /// The listing line that links here, or the document itself when derived.
    pub citation: Option<Citation>,
    /// Selected documents below a directory entry.
    pub documents: usize,
}

/// How a catalog was produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogOrigin {
    /// Read from an authored listing document, which is unchanged.
    Authored { path: String, id: String },
    /// Synthesized in memory from selected documents.
    Derived,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogRequest {
    /// Repository-relative directory; `None` is the repository root.
    pub path: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DirectoryCatalog {
    pub generation: u64,
    pub status: ReconcileStatus,
    /// Empty for the repository root.
    pub directory: String,
    pub origin: CatalogOrigin,
    /// An `index.md`/`README.md` that was present but linked to nothing
    /// selected, so it was not used as the listing.
    pub ignored_listing: Option<String>,
    pub entries: Vec<CatalogEntry>,
    pub total: usize,
    pub offset: usize,
    pub next_cursor: Option<String>,
    /// The cursor belonged to an older generation; the catalog restarted.
    pub generation_changed: bool,
    /// Links in the listing that did not resolve to selected sources.
    pub omitted_links: usize,
    /// The listing had more links than were read.
    pub scan_truncated: bool,
    /// Selected code files below the directory (they are not catalog entries).
    pub code_files: usize,
}

/// The catalog before pagination.
struct Built {
    origin: CatalogOrigin,
    ignored_listing: Option<String>,
    entries: Vec<CatalogEntry>,
    omitted_links: usize,
    scan_truncated: bool,
    code_files: usize,
}

impl RepositoryService {
    /// A page of the catalog for one directory. See the module documentation.
    pub fn catalog(&self, request: &CatalogRequest) -> Result<DirectoryCatalog, GetError> {
        let directory = match request.path.as_deref().map(str::trim) {
            None | Some("" | ".") => String::new(),
            Some(raw) => normalize_relative_path(raw)
                .map_err(|reason| invalid(format!("invalid path `{raw}`: {reason}")))?,
        };
        let cursor = request.cursor.as_deref().map(parse_cursor).transpose()?;
        let limit = request.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);

        let report = self.refresh()?;
        let (generation, (entries, listing)) = self.store().read(|conn| {
            let entries = load_entries(conn)?;
            let listing = match listing_path(&directory, &entries) {
                Some(path) => load_document(conn, "path", path)?,
                None => None,
            };
            Ok((entries, listing))
        })?;

        let mut built = build(&directory, &entries, listing.as_ref())?;
        let (offset, generation_changed) = match cursor {
            Some((issued, offset)) if issued == generation => (offset, false),
            Some(_) => (0, true),
            None => (0, false),
        };
        let total = built.entries.len();
        let mut page: Vec<CatalogEntry> =
            built.entries.drain(..).skip(offset).take(limit).collect();
        for entry in page.iter_mut().filter(|e| e.kind == CatalogKind::Directory) {
            let prefix = format!("{}/", entry.path);
            entry.documents = entries
                .iter()
                .filter(|e| e.kind == EntryKind::Document && e.path.starts_with(&prefix))
                .count();
        }
        let next = offset + page.len();
        Ok(DirectoryCatalog {
            generation,
            status: report.status,
            directory,
            origin: built.origin,
            ignored_listing: built.ignored_listing,
            entries: page,
            total,
            offset,
            next_cursor: (next < total).then(|| format!("{generation}:{next}")),
            generation_changed,
            omitted_links: built.omitted_links,
            scan_truncated: built.scan_truncated,
            code_files: built.code_files,
        })
    }
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

fn under(path: &str, directory: &str) -> bool {
    directory.is_empty()
        || path
            .strip_prefix(directory)
            .is_some_and(|r| r.starts_with('/'))
}

/// The authored listing document for `directory`, if one is selected.
fn listing_path<'a>(directory: &str, entries: &'a [ListEntry]) -> Option<&'a str> {
    LISTING_NAMES.iter().find_map(|name| {
        entries
            .iter()
            .find(|e| {
                e.kind == EntryKind::Document
                    && parent_of(&e.path) == directory
                    && file_name(&e.path).eq_ignore_ascii_case(name)
            })
            .map(|e| e.path.as_str())
    })
}

fn build(
    directory: &str,
    entries: &[ListEntry],
    listing: Option<&StoredDocument>,
) -> Result<Built, GetError> {
    let inside = || entries.iter().filter(|e| under(&e.path, directory));
    if inside().next().is_none() {
        if entries
            .iter()
            .any(|e| e.kind == EntryKind::Document && e.path == directory)
        {
            return Err(invalid(format!(
                "`{directory}` is a document, not a directory; fetch it with repo_knowledge_get"
            )));
        }
        return Err(GetError::NotFound {
            what: directory.to_string(),
        });
    }
    let code_files = inside().filter(|e| e.kind == EntryKind::File).count();

    let mut ignored_listing = None;
    if let Some(document) = listing {
        let authored = authored_entries(document, entries);
        if authored.entries.is_empty() {
            ignored_listing = Some(document.path.clone());
        } else {
            return Ok(Built {
                origin: CatalogOrigin::Authored {
                    path: document.path.clone(),
                    id: document.id.clone(),
                },
                ignored_listing: None,
                entries: authored.entries,
                omitted_links: authored.omitted,
                scan_truncated: authored.truncated,
                code_files,
            });
        }
    }

    Ok(Built {
        origin: CatalogOrigin::Derived,
        ignored_listing,
        entries: derived_entries(directory, entries),
        omitted_links: 0,
        scan_truncated: false,
        code_files,
    })
}

struct Authored {
    entries: Vec<CatalogEntry>,
    omitted: usize,
    truncated: bool,
}

/// Entries in the author's link order, resolved against the selected corpus.
fn authored_entries(listing: &StoredDocument, corpus: &[ListEntry]) -> Authored {
    let by_path: BTreeMap<&str, &ListEntry> = corpus.iter().map(|e| (e.path.as_str(), e)).collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Authored {
        entries: Vec::new(),
        omitted: 0,
        truncated: false,
    };
    let mut scanned = 0usize;

    'sections: for section in listing.sections.iter().filter(|s| !s.from_metadata) {
        // A heading section's body starts on the line after the heading.
        let first_line = section.start_line + usize::from(section.level > 0);
        for link in extract_links(&section.body, first_line) {
            if scanned == MAX_LINKS {
                out.truncated = true;
                break 'sections;
            }
            scanned += 1;
            let path = match resolve_reference(&listing.path, &link.target) {
                Reference::Local { path, .. } => path,
                Reference::Rejected(_) => {
                    out.omitted += 1;
                    continue;
                }
                // URLs and same-document fragments are inert, not omissions.
                Reference::External { .. } | Reference::SameDocument { .. } => continue,
            };
            if path == listing.path {
                continue;
            }
            let citation = Some(Citation {
                path: listing.path.clone(),
                line: link.line,
            });
            let entry = match by_path.get(path.as_str()) {
                Some(found) => entry_of(found, citation),
                None if is_directory(&by_path, &path) => directory_entry(&path, citation),
                None => {
                    out.omitted += 1;
                    continue;
                }
            };
            if seen.insert(entry.path.clone()) {
                out.entries.push(entry);
            }
        }
    }
    out
}

fn is_directory(by_path: &BTreeMap<&str, &ListEntry>, path: &str) -> bool {
    let prefix = format!("{path}/");
    by_path
        .range(prefix.as_str()..)
        .next()
        .is_some_and(|(key, _)| key.starts_with(&prefix))
}

fn directory_entry(path: &str, citation: Option<Citation>) -> CatalogEntry {
    CatalogEntry {
        kind: CatalogKind::Directory,
        path: path.to_string(),
        id: None,
        title: format!("{}/", file_name(path)),
        description: None,
        status: None,
        citation,
        documents: 0,
    }
}

fn entry_of(entry: &ListEntry, citation: Option<Citation>) -> CatalogEntry {
    CatalogEntry {
        kind: match entry.kind {
            EntryKind::Document => CatalogKind::Document,
            EntryKind::File => CatalogKind::File,
        },
        path: entry.path.clone(),
        id: Some(entry.id.clone()),
        title: entry.title.clone(),
        description: entry.description.clone(),
        status: entry.status.clone(),
        citation,
        documents: 0,
    }
}

/// Immediate subdirectories holding selected documents, then the directory's
/// own documents, each sorted by path. Code files are counted, not listed.
fn derived_entries(directory: &str, corpus: &[ListEntry]) -> Vec<CatalogEntry> {
    let mut subdirectories: BTreeSet<String> = BTreeSet::new();
    let mut documents: Vec<CatalogEntry> = Vec::new();
    for entry in corpus
        .iter()
        .filter(|e| e.kind == EntryKind::Document && under(&e.path, directory))
    {
        let relative = if directory.is_empty() {
            entry.path.as_str()
        } else {
            &entry.path[directory.len() + 1..]
        };
        match relative.split_once('/') {
            Some((child, _)) => {
                subdirectories.insert(if directory.is_empty() {
                    child.to_string()
                } else {
                    format!("{directory}/{child}")
                });
            }
            None => documents.push(entry_of(
                entry,
                Some(Citation {
                    path: entry.path.clone(),
                    line: 1,
                }),
            )),
        }
    }
    documents.sort_by(|a, b| a.path.cmp(&b.path));
    subdirectories
        .iter()
        .map(|path| directory_entry(path, None))
        .chain(documents)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::repository::browse::tests::Fx;
    use std::fs;
    use std::path::Path;

    fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    out.push((rel, fs::read(&path).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    fn catalog(fx: &Fx, path: Option<&str>) -> DirectoryCatalog {
        fx.service
            .catalog(&CatalogRequest {
                path: path.map(str::to_string),
                ..Default::default()
            })
            .unwrap()
    }

    fn paths(catalog: &DirectoryCatalog) -> Vec<&str> {
        catalog.entries.iter().map(|e| e.path.as_str()).collect()
    }

    #[test]
    fn an_authored_listing_is_followed_in_order_cited_and_left_unchanged() {
        let fx = Fx::new();
        fx.put(
            "docs/index.md",
            "# Docs\n\nSee [b](sub/b.md \"the b\") first.\n\n- [a](a.md)\n- [code](../src/x.rs)\n\
             - [again](a.md)\n- [outside](../secret.md)\n- [site](https://example.com/x)\n\
             - [dir](sub/)\n- [self](index.md)\n",
        );
        fx.put("docs/a.md", "---\ndescription: about a\n---\n# A\nx\n");
        fx.put("docs/sub/b.md", "# B\nx\n");
        fx.put("src/x.rs", "");
        fx.put("secret.md", "# Not selected\n");
        let before = tree(&fx.repo);

        let found = catalog(&fx, Some("docs"));

        assert_eq!(
            found.origin,
            CatalogOrigin::Authored {
                path: "docs/index.md".into(),
                id: "path:docs/index.md".into()
            }
        );
        assert_eq!(
            paths(&found),
            vec!["docs/sub/b.md", "docs/a.md", "src/x.rs", "docs/sub"]
        );
        assert_eq!(
            found.entries[0].citation,
            Some(Citation {
                path: "docs/index.md".into(),
                line: 3
            })
        );
        assert_eq!(found.entries[1].citation.as_ref().unwrap().line, 5);
        assert_eq!(found.entries[1].description.as_deref(), Some("about a"));
        assert_eq!(found.entries[2].kind, CatalogKind::File);
        assert_eq!(found.entries[3].kind, CatalogKind::Directory);
        assert_eq!(found.entries[3].documents, 1);
        // Only the unselected local file is counted; the URL is inert.
        assert_eq!(found.omitted_links, 1);
        assert!(!found.entries.iter().any(|e| e.path.contains("secret")));
        assert_eq!(tree(&fx.repo), before);
    }

    #[test]
    fn a_directory_without_a_listing_gets_a_derived_catalog_that_writes_nothing() {
        let fx = Fx::new();
        fx.put("docs/a.md", "---\ndescription: about a\n---\n# A\nx\n");
        fx.put("docs/z.md", "# Zed\nx\n");
        fx.put("docs/sub/b.md", "# B\nx\n");
        fx.put("docs/sub/c.md", "# C\nx\n");
        fx.put("docs/sub/deep/d.md", "# D\nx\n");
        fx.put("docs/ignored.txt", "not selected");
        fx.put("src/x.rs", "");
        let before = tree(&fx.repo);

        let found = catalog(&fx, Some("docs"));

        assert_eq!(found.origin, CatalogOrigin::Derived);
        assert_eq!(paths(&found), vec!["docs/sub", "docs/a.md", "docs/z.md"]);
        assert_eq!(found.entries[0].documents, 3);
        assert_eq!(found.entries[0].citation, None);
        // Authored descriptions are reused; missing ones stay missing.
        assert_eq!(found.entries[1].description.as_deref(), Some("about a"));
        assert_eq!(found.entries[2].description, None);
        // Each derived entry cites the document it came from.
        assert_eq!(
            found.entries[2].citation,
            Some(Citation {
                path: "docs/z.md".into(),
                line: 1
            })
        );
        assert_eq!(found.omitted_links, 0);
        assert_eq!(tree(&fx.repo), before);

        let root = catalog(&fx, None);
        assert_eq!(root.origin, CatalogOrigin::Derived);
        assert_eq!(paths(&root), vec!["docs"]);
        assert_eq!(root.code_files, 1);
        assert_eq!(tree(&fx.repo), before);
    }

    #[test]
    fn a_listing_that_links_to_nothing_selected_is_reported_and_not_used() {
        let fx = Fx::new();
        fx.put(
            "docs/README.md",
            "# Readme\n\nJust prose, and [web](https://example.com).\n",
        );
        fx.put("docs/a.md", "# A\nx\n");

        let found = catalog(&fx, Some("docs"));

        assert_eq!(found.origin, CatalogOrigin::Derived);
        assert_eq!(found.ignored_listing.as_deref(), Some("docs/README.md"));
        assert_eq!(paths(&found), vec!["docs/README.md", "docs/a.md"]);
    }

    #[test]
    fn index_md_takes_precedence_over_readme() {
        let fx = Fx::new();
        fx.put("docs/README.md", "# R\n\n[a](a.md)\n");
        fx.put("docs/index.md", "# I\n\n[b](b.md)\n");
        fx.put("docs/a.md", "# A\nx\n");
        fx.put("docs/b.md", "# B\nx\n");

        let found = catalog(&fx, Some("docs"));

        assert!(matches!(
            &found.origin,
            CatalogOrigin::Authored { path, .. } if path == "docs/index.md"
        ));
        assert_eq!(paths(&found), vec!["docs/b.md"]);
    }

    #[test]
    fn catalogs_continue_with_cursors_and_disclose_generation_changes() {
        let fx = Fx::new();
        for n in 0..5 {
            fx.put(&format!("docs/d{n}.md"), &format!("# D{n}\nx\n"));
        }
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = fx
                .service
                .catalog(&CatalogRequest {
                    path: Some("docs".into()),
                    cursor: cursor.clone(),
                    limit: Some(2),
                })
                .unwrap();
            assert_eq!(page.total, 5);
            assert!(!page.generation_changed);
            seen.extend(page.entries.iter().map(|e| e.path.clone()));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(
            seen,
            vec![
                "docs/d0.md",
                "docs/d1.md",
                "docs/d2.md",
                "docs/d3.md",
                "docs/d4.md"
            ]
        );

        let first = fx
            .service
            .catalog(&CatalogRequest {
                path: Some("docs".into()),
                limit: Some(2),
                ..Default::default()
            })
            .unwrap();
        fx.put("docs/d9.md", "# D9\nx\n");
        let resumed = fx
            .service
            .catalog(&CatalogRequest {
                path: Some("docs".into()),
                cursor: first.next_cursor,
                limit: Some(2),
            })
            .unwrap();
        assert!(resumed.generation_changed);
        assert_eq!(resumed.offset, 0);
        assert_eq!(resumed.total, 6);
    }

    #[test]
    fn long_listings_are_bounded() {
        let fx = Fx::new();
        let mut index = String::from("# Index\n\n");
        for n in 0..(MAX_LINKS + 20) {
            index.push_str(&format!("- [d](d{n}.md)\n"));
        }
        fx.put("docs/index.md", &index);
        fx.put("docs/d0.md", "# D\nx\n");

        let found = catalog(&fx, Some("docs"));

        assert!(found.scan_truncated);
        assert_eq!(found.entries.len(), 1);
        assert_eq!(found.omitted_links, MAX_LINKS - 1);
    }

    #[test]
    fn bad_paths_documents_and_unknown_directories_are_rejected() {
        let fx = Fx::new();
        fx.put("docs/a.md", "# A\nx\n");
        let request = |path: &str| {
            fx.service.catalog(&CatalogRequest {
                path: Some(path.into()),
                ..Default::default()
            })
        };
        assert!(matches!(request("../etc"), Err(GetError::Invalid(_))));
        assert!(matches!(request("/etc"), Err(GetError::Invalid(_))));
        assert!(matches!(request("docs/a.md"), Err(GetError::Invalid(_))));
        assert!(matches!(request("nope"), Err(GetError::NotFound { .. })));
        // `.` is the root.
        assert_eq!(request(".").unwrap().directory, "");
    }
}
