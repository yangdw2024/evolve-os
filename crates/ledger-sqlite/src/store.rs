use core_contracts::{ArtifactPayload, ContractError};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const INIT_SQL: &str = include_str!("../migrations/001_init.sql");

/// Thread-safe, cloneable SQLite WAL store implementing `BudgetLedger` and `EvolutionArchive`.
#[derive(Clone)]
pub struct SqliteLedgerStore {
    pub(crate) conn: Arc<Mutex<Connection>>,
}

impl SqliteLedgerStore {
    /// Opens or creates a persistent SQLite WAL database at `path` and applies migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ContractError> {
        let conn = Connection::open(path).map_err(map_sqlite_err)?;
        Self::init_connection(conn)
    }

    /// Opens an in-memory SQLite database (ideal for deterministic unit tests).
    pub fn open_in_memory() -> Result<Self, ContractError> {
        let conn = Connection::open_in_memory().map_err(map_sqlite_err)?;
        Self::init_connection(conn)
    }

    fn init_connection(conn: Connection) -> Result<Self, ContractError> {
        conn.execute_batch(INIT_SQL).map_err(map_sqlite_err)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Runs a synchronous SQLite closure on Tokio's blocking thread pool.
    pub(crate) async fn with_conn<F, T>(&self, f: F) -> Result<T, ContractError>
    where
        F: FnOnce(&mut Connection) -> Result<T, ContractError> + Send + 'static,
        T: Send + 'static,
    {
        let conn_arc = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn_arc
                .lock()
                .map_err(|_| ContractError::Internal("sqlite mutex poisoned".into()))?;
            f(&mut guard)
        })
        .await
        .map_err(|e| ContractError::Internal(format!("blocking task join error: {e}")))?
    }
}

pub(crate) fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) fn map_sqlite_err(err: rusqlite::Error) -> ContractError {
    ContractError::Internal(format!("sqlite error: {err}"))
}

pub(crate) fn encode_payload(
    payload: &ArtifactPayload,
) -> Result<(Vec<u8>, &'static str, String), ContractError> {
    let media_type = payload.media_type();
    let bytes = match payload {
        ArtifactPayload::SingleFile(text) => text.as_bytes().to_vec(),
        ArtifactPayload::FileMap(map) => serde_json::to_vec(map)
            .map_err(|e| ContractError::Internal(format!("serialize filemap: {e}")))?,
    };
    let mut hasher = Sha256::new();
    hasher.update(media_type.as_bytes());
    hasher.update([0u8]);
    hasher.update(&bytes);
    let sha256 = hex::encode(hasher.finalize());
    Ok((bytes, media_type, sha256))
}

pub(crate) fn decode_payload(
    media_type: &str,
    bytes: &[u8],
) -> Result<ArtifactPayload, ContractError> {
    match media_type {
        "text/plain" => {
            let text = String::from_utf8(bytes.to_vec())
                .map_err(|e| ContractError::Internal(format!("invalid utf8 blob: {e}")))?;
            Ok(ArtifactPayload::SingleFile(text))
        }
        "application/vnd.evolve.filemap+json" => {
            let map: BTreeMap<String, String> = serde_json::from_slice(bytes)
                .map_err(|e| ContractError::Internal(format!("deserialize filemap: {e}")))?;
            Ok(ArtifactPayload::FileMap(map))
        }
        other => Err(ContractError::Internal(format!(
            "unsupported blob media_type: {other}"
        ))),
    }
}
