use super::config::MemorySettings;
use super::source::{Entry, Source};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "KeyData")]
pub(super) struct Key {
    pub start: i64,
    pub n: i64,
}

#[derive(Deserialize)]
struct KeyData {
    start: u64,
    n: u64,
}

impl TryFrom<KeyData> for Key {
    type Error = anyhow::Error;
    fn try_from(value: KeyData) -> Result<Self> {
        Self::checked(value.start, value.n)
    }
}

impl Key {
    pub fn checked(start: u64, n: u64) -> Result<Self> {
        ensure!(
            n.is_power_of_two() && n <= (1 << 62) && start.is_multiple_of(n),
            "expected aligned id and power-of-two n"
        );
        ensure!(
            start
                .checked_add(n)
                .is_some_and(|end| end <= i64::MAX as u64),
            "memory range is too large"
        );
        Ok(Self {
            start: i64::try_from(start)?,
            n: i64::try_from(n)?,
        })
    }
    pub fn end(self) -> i64 {
        self.start + self.n
    }
    fn parent(self) -> Self {
        Self {
            start: self.start / (self.n * 2) * (self.n * 2),
            n: self.n * 2,
        }
    }
    pub fn label(self) -> String {
        format!("{}+{}", self.start, self.n)
    }
}

#[derive(Clone)]
pub(super) struct Job {
    pub generation: i64,
    pub key: Key,
    pub input: String,
    pub context: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct View {
    keys: Vec<Key>,
    shrinking: bool,
}

pub(super) struct Store {
    conn: Connection,
    // One process owns each session's experimental database. OS releases on crash.
    _lock: File,
    settings: MemorySettings,
}

impl Store {
    pub fn open(path: &Path, session_id: &str, settings: MemorySettings) -> Result<Self> {
        if let Some(parent) = path.parent() {
            let mut directories = std::fs::DirBuilder::new();
            directories.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                directories.mode(0o700);
            }
            directories.create(parent)?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let lock = options.open(path.with_extension("lock"))?;
        lock.try_lock()
            .context("OptChat session is already open in another process")?;
        // Pre-create privately and reject symlink aliases that bypass the lock.
        options.open(path)?;
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // Fail before DDL or journal changes if the path belongs to another DB.
        let identified: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='identity')",
            [],
            |r| r.get(0),
        )?;
        if identified {
            let (id, version): (String, i64) =
                conn.query_row("SELECT session,version FROM identity", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
            ensure!(
                id == session_id && version == 1,
                "OptChat database identity or version mismatch"
            );
        } else {
            let tables: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |r|r.get(0))?;
            ensure!(
                tables == 0,
                "refusing to initialize OptChat in a non-OptChat database"
            );
        }
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
        let tx = conn.transaction()?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS identity(session TEXT NOT NULL, version INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS generations(id INTEGER PRIMARY KEY, settings TEXT NOT NULL,
                view TEXT NOT NULL DEFAULT '{\"keys\":[],\"shrinking\":false}',
                summary_view TEXT NOT NULL DEFAULT '{\"keys\":[],\"shrinking\":false}', count INTEGER NOT NULL DEFAULT 0, epoch INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS sources(g INTEGER NOT NULL, ordinal INTEGER NOT NULL, source_id TEXT NOT NULL,
                fingerprint TEXT NOT NULL, PRIMARY KEY(g,ordinal));
            CREATE TABLE IF NOT EXISTS entries(g INTEGER NOT NULL, id INTEGER NOT NULL, data TEXT NOT NULL, PRIMARY KEY(g,id));
            CREATE TABLE IF NOT EXISTS nodes(g INTEGER NOT NULL, start INTEGER NOT NULL, n INTEGER NOT NULL, text TEXT NOT NULL,
                PRIMARY KEY(g,start,n));
            CREATE TABLE IF NOT EXISTS jobs(g INTEGER NOT NULL, start INTEGER NOT NULL, n INTEGER NOT NULL,
                failed_at INTEGER NOT NULL DEFAULT -1, error TEXT, PRIMARY KEY(g,start,n));
            CREATE INDEX IF NOT EXISTS ready_jobs ON jobs(g,failed_at,n,start);")?;
        if !identified {
            tx.execute("INSERT INTO identity VALUES (?,1)", [session_id])?;
        }
        tx.commit()?;
        let mut store = Self {
            conn,
            _lock: lock,
            settings,
        };
        if store.generation()? == 0 {
            store.new_generation()?;
        }
        Ok(store)
    }

    fn generation(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id),0) FROM generations", [], |r| {
                r.get(0)
            })?)
    }

    fn shape(&self) -> String {
        format!(
            "{}:{}:{}",
            self.settings.summary_bytes, self.settings.view_min_bytes, self.settings.view_max_bytes
        )
    }

    fn new_generation(&mut self) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO generations(settings) VALUES (?)",
            [self.shape()],
        )?;
        self.generation()
    }

    /// Rewritten history gets a separate generation; old originals are never overwritten.
    pub fn sync(&mut self, sources: &[Source]) -> Result<()> {
        let mut g = self.generation()?;
        let stored = {
            let mut stmt = self
                .conn
                .prepare("SELECT source_id,fingerprint FROM sources WHERE g=? ORDER BY ordinal")?;
            stmt.query_map([g], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let shape: String =
            self.conn
                .query_row("SELECT settings FROM generations WHERE id=?", [g], |r| {
                    r.get(0)
                })?;
        let same_prefix = stored.len() <= sources.len()
            && stored
                .iter()
                .zip(sources)
                .all(|((id, hash), source)| id == &source.id && hash == &source.fingerprint);
        let skip = if same_prefix && shape == self.shape() {
            stored.len()
        } else {
            g = self.new_generation()?;
            0
        };
        let summary_bytes = self.settings.summary_bytes;
        let tx = self.conn.transaction()?;
        let mut count: i64 =
            tx.query_row("SELECT count FROM generations WHERE id=?", [g], |r| {
                r.get(0)
            })?;
        for (ordinal, source) in sources.iter().enumerate().skip(skip) {
            tx.execute(
                "INSERT INTO sources VALUES (?,?,?,?)",
                params![g, i64::try_from(ordinal)?, source.id, source.fingerprint],
            )?;
            for entry in &source.entries {
                tx.execute(
                    "INSERT INTO entries VALUES (?,?,?)",
                    params![g, count, serde_json::to_string(entry)?],
                )?;
                let key = Key::checked(u64::try_from(count)?, 1)?;
                let text = entry.line();
                if text.len() <= summary_bytes {
                    tx.execute(
                        "INSERT INTO nodes VALUES (?,?,1,?)",
                        params![g, count, text],
                    )?;
                    enqueue_parent(&tx, g, key)?;
                } else {
                    tx.execute(
                        "INSERT INTO jobs(g,start,n) VALUES (?,?,1)",
                        params![g, count],
                    )?;
                }
                count = count.checked_add(1).context("OptChat sequence exhausted")?;
            }
        }
        tx.execute(
            "UPDATE generations SET count=?,epoch=epoch+1 WHERE id=?",
            params![count, g],
        )?;
        tx.commit()?;
        self.update_views(g)
    }

    pub fn entry(&self, key: Key) -> Result<Entry> {
        ensure!(key.n == 1, "original retrieval requires n=1");
        let g = self.generation()?;
        let data: String = self
            .conn
            .query_row(
                "SELECT data FROM entries WHERE g=? AND id=?",
                params![g, key.start],
                |r| r.get(0),
            )
            .context("memory entry not found in this session/generation")?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn children(&self, key: Key) -> Result<Vec<(Key, String)>> {
        ensure!(key.n > 1, "a leaf has no children");
        let g = self.generation()?;
        let mut result = Vec::new();
        for start in [key.start, key.start + key.n / 2] {
            let child = Key {
                start,
                n: key.n / 2,
            };
            let text = node(&self.conn, g, child)?.context("memory node is not summarized yet")?;
            result.push((child, text));
        }
        Ok(result)
    }

    pub fn jobs(&self, limit: usize) -> Result<Vec<Job>> {
        let g = self.generation()?;
        let epoch: i64 =
            self.conn
                .query_row("SELECT epoch FROM generations WHERE id=?", [g], |r| {
                    r.get(0)
                })?;
        let keys = {
            let mut stmt = self.conn.prepare(
                "SELECT start,n FROM jobs WHERE g=? AND failed_at < ? ORDER BY n,start LIMIT ?",
            )?;
            stmt.query_map(params![g, epoch, i64::try_from(limit)?], |r| {
                Ok(Key {
                    start: r.get(0)?,
                    n: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut result = Vec::new();
        for key in keys {
            let input = if key.n == 1 {
                self.entry(key)?.line()
            } else {
                self.children(key)?
                    .into_iter()
                    .map(|(_, t)| t)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let view = self.load_view(g, true)?;
            let context = render(
                &self.conn,
                g,
                &view
                    .keys
                    .iter()
                    .copied()
                    .filter(|k| k.end() <= if key.n == 1 { key.start } else { key.end() })
                    .collect::<Vec<_>>(),
            )?;
            result.push(Job {
                generation: g,
                key,
                input,
                context,
            });
        }
        Ok(result)
    }

    pub fn finish(&mut self, job: &Job, result: Result<String>) -> Result<()> {
        ensure!(
            job.generation == self.generation()?,
            "history changed while summary was running"
        );
        match result {
            Ok(text) => {
                ensure!(
                    !text.trim().is_empty() && text.len() <= self.settings.summary_bytes,
                    "invalid memory summary"
                );
                let tx = self.conn.transaction()?;
                tx.execute(
                    "INSERT OR IGNORE INTO nodes VALUES (?,?,?,?)",
                    params![job.generation, job.key.start, job.key.n, text],
                )?;
                tx.execute(
                    "DELETE FROM jobs WHERE g=? AND start=? AND n=?",
                    params![job.generation, job.key.start, job.key.n],
                )?;
                enqueue_parent(&tx, job.generation, job.key)?;
                tx.commit()?;
            }
            Err(_) => {
                // Error bodies may echo credentials or source content. Persist only
                // a safe status, not arbitrary provider diagnostics.
                self.conn.execute("UPDATE jobs SET failed_at=(SELECT epoch FROM generations WHERE id=?),error=? WHERE g=? AND start=? AND n=?",
                    params![job.generation,"summary request failed; retry catch-up",job.generation,job.key.start,job.key.n])?;
            }
        }
        Ok(())
    }

    pub fn refresh_views(&mut self) -> Result<()> {
        self.update_views(self.generation()?)
    }

    pub fn ready(&self) -> Result<bool> {
        let g = self.generation()?;
        let count: i64 =
            self.conn
                .query_row("SELECT count FROM generations WHERE id=?", [g], |r| {
                    r.get(0)
                })?;
        let view = self.load_view(g, false)?;
        Ok(view.keys.last().map_or(0, |k| k.end()) == count && !view.shrinking)
    }

    pub fn last_error(&self) -> Result<Option<String>> {
        let g = self.generation()?;
        Ok(self
            .conn
            .query_row(
                "SELECT error FROM jobs WHERE g=? AND error IS NOT NULL ORDER BY start LIMIT 1",
                [g],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn view(&self) -> Result<Vec<(Key, String)>> {
        let g = self.generation()?;
        self.load_view(g, false)?
            .keys
            .into_iter()
            .map(|key| {
                Ok((
                    key,
                    node(&self.conn, g, key)?
                        .context("persisted OptChat view references missing node")?,
                ))
            })
            .collect()
    }

    fn load_view(&self, g: i64, summary: bool) -> Result<View> {
        let sql = if summary {
            "SELECT summary_view FROM generations WHERE id=?"
        } else {
            "SELECT view FROM generations WHERE id=?"
        };
        let json: String = self.conn.query_row(sql, [g], |r| r.get(0))?;
        let view: View = serde_json::from_str(&json)?;
        let count: i64 =
            self.conn
                .query_row("SELECT count FROM generations WHERE id=?", [g], |r| {
                    r.get(0)
                })?;
        let mut covered = 0;
        for key in &view.keys {
            ensure!(
                key.start == covered && key.end() <= count,
                "invalid persisted OptChat view coverage"
            );
            covered = key.end();
        }
        Ok(view)
    }

    fn update_views(&mut self, g: i64) -> Result<()> {
        let count: i64 =
            self.conn
                .query_row("SELECT count FROM generations WHERE id=?", [g], |r| {
                    r.get(0)
                })?;
        for summary in [false, true] {
            let mut view = self.load_view(g, summary)?;
            let mut covered = view.keys.last().map_or(0, |k| k.end());
            while covered < count
                && node(
                    &self.conn,
                    g,
                    Key {
                        start: covered,
                        n: 1,
                    },
                )?
                .is_some()
            {
                view.keys.push(Key {
                    start: covered,
                    n: 1,
                });
                covered += 1;
            }
            let (min, max) = if summary {
                (16_000, 32_000)
            } else {
                (self.settings.view_min_bytes, self.settings.view_max_bytes)
            };
            let mut bytes = view_size(&self.conn, g, &view.keys)?;
            if bytes > max {
                view.shrinking = true;
            }
            while view.shrinking && bytes > min {
                let Some(index) = merge_candidate(&self.conn, g, &view.keys, count)? else {
                    break;
                };
                let parent = view.keys[index].parent();
                view.keys.splice(index..index + 2, [parent]);
                bytes = view_size(&self.conn, g, &view.keys)?;
            }
            if bytes <= min {
                view.shrinking = false;
            }
            let sql = if summary {
                "UPDATE generations SET summary_view=? WHERE id=?"
            } else {
                "UPDATE generations SET view=? WHERE id=?"
            };
            self.conn
                .execute(sql, params![serde_json::to_string(&view)?, g])?;
        }
        Ok(())
    }
}

fn node(conn: &Connection, g: i64, key: Key) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT text FROM nodes WHERE g=? AND start=? AND n=?",
            params![g, key.start, key.n],
            |r| r.get(0),
        )
        .optional()?)
}

fn enqueue_parent(conn: &Connection, g: i64, key: Key) -> Result<()> {
    if key.n >= (1 << 61) {
        return Ok(());
    }
    let parent = key.parent();
    let left = Key {
        start: parent.start,
        n: key.n,
    };
    let right = Key {
        start: parent.start + key.n,
        n: key.n,
    };
    if node(conn, g, left)?.is_some()
        && node(conn, g, right)?.is_some()
        && node(conn, g, parent)?.is_none()
    {
        conn.execute(
            "INSERT OR IGNORE INTO jobs(g,start,n) VALUES (?,?,?)",
            params![g, parent.start, parent.n],
        )?;
    }
    Ok(())
}

fn render(conn: &Connection, g: i64, keys: &[Key]) -> Result<String> {
    let mut text = String::from("<chat>\n");
    for key in keys {
        let summary = node(conn, g, *key)?.context("missing summary in view")?;
        text.push_str(&format!(
            "{}|{}\n",
            key.label(),
            summary.replace(['\n', '\r'], " ")
        ));
    }
    text.push_str("</chat>");
    Ok(text)
}

fn view_size(conn: &Connection, g: i64, keys: &[Key]) -> Result<usize> {
    // Runtime emits independently cacheable four-line blocks, each with tags.
    Ok(render(conn, g, keys)?.len() + keys.len().div_ceil(4).saturating_sub(1) * 14)
}

/// Compare rational ages using pair END, not pair start; ties choose oldest.
fn merge_candidate(conn: &Connection, g: i64, keys: &[Key], total: i64) -> Result<Option<usize>> {
    let mut best: Option<usize> = None;
    for (index, pair) in keys.windows(2).enumerate() {
        let [left, right] = pair else {
            continue;
        };
        if left.n != right.n || left.end() != right.start || left.start % (left.n * 2) != 0 {
            continue;
        }
        if node(conn, g, left.parent())?.is_none() {
            continue;
        }
        if let Some(old) = best {
            let old_left = keys[old];
            let age = total.saturating_sub(right.end() - 1) as u128;
            let old_age = total.saturating_sub(keys[old + 1].end() - 1) as u128;
            if age * (old_left.n as u128) > old_age * (left.n as u128) {
                best = Some(index);
            }
        } else {
            best = Some(index);
        }
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn source(id: usize, text: String) -> Source {
        Source {
            id: id.to_string(),
            fingerprint: text.clone(),
            entries: vec![Entry {
                kind: "user".into(),
                text,
                date: 123,
                attachments: vec![],
            }],
        }
    }
    fn drain(store: &mut Store) {
        loop {
            let jobs = store.jobs(8).unwrap();
            if jobs.is_empty() {
                break;
            }
            for job in jobs {
                store
                    .finish(&job, Ok(format!("summary {}", job.key.label())))
                    .unwrap();
            }
            store.refresh_views().unwrap();
        }
    }

    #[test]
    fn view_is_saved_exactly_and_sources_are_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.db");
        let settings = MemorySettings {
            view_min_bytes: 1200,
            view_max_bytes: 1800,
            ..Default::default()
        };
        let input: Vec<_> = (0..200).map(|i| source(i, "x".repeat(500))).collect();
        let mut store = Store::open(&path, "session", settings.clone()).unwrap();
        store.sync(&input).unwrap();
        drain(&mut store);
        assert!(store.ready().unwrap());
        let before = store.view().unwrap();
        assert_eq!(before.first().unwrap().0.start, 0);
        assert_eq!(before.last().unwrap().0.end(), 200);
        for pair in before.windows(2) {
            assert_eq!(pair[0].0.end(), pair[1].0.start);
        }
        assert!(
            view_size(
                &store.conn,
                store.generation().unwrap(),
                &before.iter().map(|v| v.0).collect::<Vec<_>>()
            )
            .unwrap()
                <= 1800
        );
        store.sync(&input).unwrap();
        assert_eq!(before, store.view().unwrap());
        drop(store);
        let reopened = Store::open(&path, "session", settings).unwrap();
        assert_eq!(before, reopened.view().unwrap());
    }

    #[test]
    fn appending_does_not_rewrite_view_before_batch() {
        let dir = tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("m.db"), "s", Default::default()).unwrap();
        let mut input = vec![source(0, "first".into())];
        store.sync(&input).unwrap();
        let before = store.view().unwrap();
        input.push(source(1, "second".into()));
        store.sync(&input).unwrap();
        drain(&mut store);
        assert_eq!(&store.view().unwrap()[..1], before);
    }

    #[test]
    fn undo_creates_generation_and_cannot_zoom_abandoned_entries() {
        let dir = tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("m.db"), "s", Default::default()).unwrap();
        store
            .sync(&[source(0, "kept".into()), source(1, "abandoned".into())])
            .unwrap();
        drain(&mut store);
        store.sync(&[source(0, "kept".into())]).unwrap();
        assert_eq!(store.generation().unwrap(), 2);
        assert!(store.entry(Key { start: 1, n: 1 }).is_err());
        assert_eq!(store.entry(Key { start: 0, n: 1 }).unwrap().text, "kept");
    }

    #[test]
    fn failure_is_not_a_node_and_retries_after_new_source() {
        let dir = tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("m.db"), "s", Default::default()).unwrap();
        let input = vec![source(0, "long".repeat(200))];
        store.sync(&input).unwrap();
        let job = store.jobs(1).unwrap().pop().unwrap();
        store
            .finish(
                &job,
                Err(anyhow::anyhow!("SECRET_ERROR provider echoed a credential")),
            )
            .unwrap();
        assert!(!store.ready().unwrap());
        assert!(
            !store
                .last_error()
                .unwrap()
                .unwrap()
                .contains("SECRET_ERROR")
        );
        assert!(store.jobs(1).unwrap().is_empty());
        drop(store);
        let mut store = Store::open(&dir.path().join("m.db"), "s", Default::default()).unwrap();
        // A failed turn can be retried even without an additional source entry.
        store.sync(&input).unwrap();
        assert_eq!(store.jobs(1).unwrap().len(), 1);
        drain(&mut store);
        assert!(store.ready().unwrap());
    }

    #[test]
    fn due_order_uses_pair_end_and_oldest_tie() {
        let dir = tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("m.db"), "s", Default::default()).unwrap();
        store
            .sync(&(0..10).map(|i| source(i, "x".into())).collect::<Vec<_>>())
            .unwrap();
        drain(&mut store);
        let keys = [
            Key { start: 0, n: 4 },
            Key { start: 4, n: 4 },
            Key { start: 8, n: 1 },
            Key { start: 9, n: 1 },
        ];
        // Inclusive last-message index: at T=10, 1/1 beats 3/4.
        assert_eq!(merge_candidate(&store.conn, 1, &keys, 10).unwrap(), Some(2));
        assert_eq!(merge_candidate(&store.conn, 1, &keys, 11).unwrap(), Some(2));
        assert!(Key::checked(1, 2).is_err());
        assert!(Key::checked(0, 3).is_err());
    }

    #[test]
    fn foreign_database_is_rejected_without_adding_tables() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("foreign.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE unrelated(value TEXT); INSERT INTO unrelated VALUES ('keep');",
            )
            .unwrap();
        }
        assert!(Store::open(&path, "s", Default::default()).is_err());
        let conn = Connection::open(&path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn invalid_persisted_keys_are_rejected_before_arithmetic() {
        for json in [
            r#"{"start":0,"n":0}"#,
            r#"{"start":-1,"n":1}"#,
            r#"{"start":1,"n":2}"#,
            r#"{"start":9223372036854775807,"n":1}"#,
        ] {
            assert!(
                serde_json::from_str::<Key>(json).is_err(),
                "accepted invalid key {json}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_alias_cannot_bypass_session_writer_lock() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("owned.db");
        let _owner = Store::open(&path, "s", Default::default()).unwrap();
        let alias = directory.path().join("alias.db");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(Store::open(&alias, "s", Default::default()).is_err());
    }

    #[test]
    fn second_writer_is_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("m.db");
        let _owner = Store::open(&path, "s", Default::default()).unwrap();
        assert!(Store::open(&path, "s", Default::default()).is_err());
    }
}
