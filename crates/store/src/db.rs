//! The database: opened encrypted, its schema brought up to date, and a
//! small statement runner over the engine's step loop.

use std::path::Path;
use std::sync::Arc;

use turso_core::{Connection, Database, StepResult, Value};

use crate::StoreError;

/// The cipher the whole database file is encrypted with (plan D03 as
/// changed 2026-11-14: the engine's own encryption).
const CIPHER: &str = "aegis256";
/// The key's length in bytes.
const KEY_LEN: usize = 32;
/// The schema version this code writes.
const SCHEMA: i64 = 1;

fn db_err(e: impl std::fmt::Display) -> StoreError {
    StoreError::Db(e.to_string())
}

/// An open profile database.
#[allow(missing_debug_implementations)]
pub struct Store {
    _db: Arc<Database>,
    conn: Arc<Connection>,
    io: Arc<dyn turso_core::IO>,
}

impl Store {
    /// Open the profile in `dir`, creating the folder, its key and its
    /// database when they are missing.
    pub fn open(dir: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(dir)?;
        let key = load_or_create_key(&dir.join("key"))?;
        let path = dir.join("browser.db");
        let path = path.to_str().ok_or(StoreError::Db("the profile path is not UTF-8".into()))?;
        let io: Arc<dyn turso_core::IO> = Arc::new(turso_core::PlatformIO::new().map_err(db_err)?);
        let encryption = turso_core::EncryptionOpts {
            cipher: CIPHER.to_owned(),
            hexkey: key,
        };
        let opts = turso_core::DatabaseOpts::new().with_encryption(true);
        #[allow(deprecated)]
        let db = Database::open_file_with_flags(io.clone(), path, turso_core::OpenFlags::default(), opts, Some(encryption.clone()), Arc::new(turso_core::SqliteDialect))
            .map_err(db_err)?;
        let conn = db.connect().map_err(db_err)?;
        let store = Store { _db: db, conn, io };
        // The connection's cipher and key, before it reads or writes a
        // page (a new file is created encrypted).
        store.execute(&format!("PRAGMA cipher = '{CIPHER}'"), &[])?;
        store.execute(&format!("PRAGMA hexkey = '{}'", encryption.hexkey), &[])?;
        store.migrate()?;
        Ok(store)
    }

    /// Create the tables of this schema version.
    fn migrate(&self) -> Result<(), StoreError> {
        self.execute("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL)", &[])?;
        let version = self.query("SELECT value FROM meta WHERE key = 'schema'", &[])?.first().and_then(|r| r.first()).and_then(as_int);
        if version.unwrap_or(0) < 1 {
            self.execute(
                "CREATE TABLE IF NOT EXISTS history (url TEXT PRIMARY KEY, title TEXT, visit_count INTEGER NOT NULL, typed_count INTEGER NOT NULL, last_visit INTEGER NOT NULL)",
                &[],
            )?;
        }
        match version {
            None => self.execute("INSERT INTO meta (key, value) VALUES ('schema', ?1)", &[Value::from_i64(SCHEMA)])?,
            Some(v) if v < SCHEMA => self.execute("UPDATE meta SET value = ?1 WHERE key = 'schema'", &[Value::from_i64(SCHEMA)])?,
            Some(_) => {}
        }
        Ok(())
    }

    /// Run a statement that returns no rows.
    pub(crate) fn execute(&self, sql: &str, params: &[Value]) -> Result<(), StoreError> {
        self.query(sql, params).map(|_| ())
    }

    /// Run a statement and collect its rows.
    pub(crate) fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Vec<Value>>, StoreError> {
        let mut stmt = self.conn.prepare(sql).map_err(db_err)?;
        for (i, p) in params.iter().enumerate() {
            stmt.bind_at(std::num::NonZero::new(i + 1).expect("1-based"), p.clone()).map_err(db_err)?;
        }
        let mut rows = Vec::new();
        loop {
            match stmt.step().map_err(db_err)? {
                StepResult::Row => {
                    let row = stmt.row().ok_or(StoreError::Db("a row without values".into()))?;
                    rows.push(row.get_values().cloned().collect());
                }
                StepResult::IO => self.io.step().map_err(db_err)?,
                StepResult::Done => return Ok(rows),
                other => return Err(StoreError::Db(format!("statement stopped: {other:?}"))),
            }
        }
    }
}

pub(crate) fn as_int(v: &Value) -> Option<i64> {
    v.as_int()
}

pub(crate) fn as_text(v: &Value) -> Option<String> {
    match v {
        Value::Text(t) => Some(t.as_str().to_owned()),
        _ => None,
    }
}

/// The key as hex: read from `path`, or made at random and written there.
fn load_or_create_key(path: &Path) -> Result<String, StoreError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let key = text.trim().to_owned();
            if key.len() != KEY_LEN * 2 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(StoreError::BadKey);
            }
            Ok(key)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let bytes: [u8; KEY_LEN] = rand::random();
            let key: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            std::fs::write(path, &key)?;
            Ok(key)
        }
        Err(e) => Err(e.into()),
    }
}
