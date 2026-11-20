//! SQLite access policy stored separately from session history.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use querymt_remote::NodeId;
use querymt_remote::peer_access::{
    GrantStore, GrantStoreError, PeerGrant, PeerGrantKey, PeerGrantState,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

// Distinguish the standalone store from existing user SQLite databases.
const APPLICATION_ID: i64 = 0x514d_5441;
const SCHEMA_VERSION: i64 = 1;

/// Host-local mesh grants. Each edit is a compare-and-swap SQLite transaction.
#[derive(Clone)]
pub struct SqliteMeshAccess {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteMeshAccess {
    /// Open a dedicated store. Existing databases with other schemas are refused.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, GrantStoreError> {
        let path = path.as_ref().to_path_buf();
        let conn = tokio::task::spawn_blocking(move || open_store(path))
            .await
            .map_err(backend)??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with_conn<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, GrantStoreError> + Send + 'static,
    ) -> Result<T, GrantStoreError> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || f(&mut conn.lock()))
            .await
            .map_err(backend)?
    }
}

fn open_store(path: PathBuf) -> Result<Connection, GrantStoreError> {
    let mut conn = Connection::open(path).map_err(backend)?;
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(backend)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(backend)?;
    let app: i64 = tx
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(backend)?;
    let version: i64 = tx
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(backend)?;
    match (app, version) {
        (APPLICATION_ID, SCHEMA_VERSION) => {}
        (0, 0) => {
            let populated: bool = tx
                .query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema)", [], |row| {
                    row.get(0)
                })
                .map_err(backend)?;
            if populated {
                return Err(backend(std::io::Error::other(
                    "mesh store path refers to a different database",
                )));
            }
            tx.execute_batch(
                "CREATE TABLE peer_grants (
                    host_id TEXT NOT NULL,
                    peer_id TEXT NOT NULL,
                    revision TEXT NOT NULL,
                    grant_json TEXT NOT NULL,
                    PRIMARY KEY (host_id, peer_id)
                ) STRICT;",
            )
            .map_err(backend)?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)
                .map_err(backend)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(backend)?;
        }
        _ => {
            return Err(backend(std::io::Error::other(
                "unrecognized mesh store schema",
            )));
        }
    }
    tx.commit().map_err(backend)?;
    Ok(conn)
}

fn backend(error: impl std::error::Error + Send + Sync + 'static) -> GrantStoreError {
    GrantStoreError::Backend(Box::new(error))
}

fn load_grant(conn: &Connection, key: &PeerGrantKey) -> Result<PeerGrantState, GrantStoreError> {
    let stored = conn
        .query_row(
            "SELECT revision, grant_json FROM peer_grants WHERE host_id = ?1 AND peer_id = ?2",
            params![key.host.to_string(), key.peer.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(backend)?;
    let Some((revision, grant)) = stored else {
        return Ok(PeerGrantState::default());
    };
    let grant: PeerGrant = serde_json::from_str(&grant).map_err(backend)?;
    grant.validate().map_err(backend)?;
    let revision = Uuid::parse_str(&revision).map_err(backend)?;
    if revision.is_nil() {
        return Err(backend(std::io::Error::other(
            "invalid revision stored in mesh grant",
        )));
    }
    Ok(PeerGrantState { revision, grant })
}

#[async_trait]
impl GrantStore for SqliteMeshAccess {
    async fn grant(&self, key: &PeerGrantKey) -> Result<PeerGrantState, GrantStoreError> {
        let key = key.clone();
        self.with_conn(move |conn| load_grant(conn, &key)).await
    }

    async fn replace_grant(
        &self,
        key: &PeerGrantKey,
        expected: Uuid,
        grant: PeerGrant,
    ) -> Result<PeerGrantState, GrantStoreError> {
        grant.validate()?;
        let key = key.clone();
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(backend)?;
            let current = load_grant(&tx, &key)?;
            if expected != current.revision {
                return Err(GrantStoreError::Conflict { expected, actual: current.revision });
            }
            if grant == current.grant {
                return Ok(current);
            }
            let revision = Uuid::now_v7();
            let grant_json = serde_json::to_string(&grant).map_err(backend)?;
            tx.execute(
                "INSERT INTO peer_grants (host_id, peer_id, revision, grant_json) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (host_id, peer_id) DO UPDATE SET revision = excluded.revision, grant_json = excluded.grant_json",
                params![key.host.to_string(), key.peer.to_string(), revision.to_string(), grant_json],
            ).map_err(backend)?;
            tx.commit().map_err(backend)?;
            Ok(PeerGrantState { revision, grant })
        }).await
    }

    async fn managed_peers(&self, host: &NodeId) -> Result<Vec<NodeId>, GrantStoreError> {
        let host = host.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare("SELECT peer_id FROM peer_grants WHERE host_id = ?1 ORDER BY peer_id")
                .map_err(backend)?;
            let rows = stmt
                .query_map([host], |row| row.get::<_, String>(0))
                .map_err(backend)?;
            rows.map(|row| {
                let peer = row.map_err(backend)?;
                NodeId::parse(&peer).map_err(|err| backend(std::io::Error::other(err)))
            })
            .collect()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant_key() -> PeerGrantKey {
        PeerGrantKey {
            host: NodeId::parse("12D3KooWJ5MP5JUR1c8pF37dNHGGrVhCGHEHwrCYyowX6dPJtV6t").unwrap(),
            peer: NodeId::parse("12D3KooWQKkiEPk7f8Sp2LXj7S2BJMAMePSz23RQPYGVBCGiyhJE").unwrap(),
        }
    }

    #[tokio::test]
    async fn stale_owner_edit_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mesh-access.db");
        let first = SqliteMeshAccess::open(&path).await.unwrap();
        let second = SqliteMeshAccess::open(&path).await.unwrap();
        let key = grant_key();
        let baseline = first.grant(&key).await.unwrap();
        assert_eq!(baseline, PeerGrantState::default());
        let edited = first
            .replace_grant(&key, baseline.revision, PeerGrant::Full)
            .await
            .unwrap();
        assert!(matches!(
            second.replace_grant(&key, baseline.revision, PeerGrant::None).await,
            Err(GrantStoreError::Conflict { actual, .. }) if actual == edited.revision
        ));
        drop((first, second));
        let reopened = SqliteMeshAccess::open(&path).await.unwrap();
        assert_eq!(reopened.grant(&key).await.unwrap(), edited);
        assert_eq!(
            reopened.managed_peers(&key.host).await.unwrap(),
            vec![key.peer.clone()]
        );
        let reverse = PeerGrantKey {
            host: key.peer,
            peer: key.host,
        };
        assert_eq!(
            reopened.grant(&reverse).await.unwrap(),
            PeerGrantState::default()
        );
    }

    #[tokio::test]
    async fn refuses_a_database_with_an_unrelated_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unrelated.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE sessions (id INTEGER PRIMARY KEY);")
            .unwrap();
        assert!(matches!(
            SqliteMeshAccess::open(path).await,
            Err(GrantStoreError::Backend(_))
        ));
        let tables: i64 = conn
            .query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))
            .unwrap();
        assert_eq!(tables, 1);
    }
}
