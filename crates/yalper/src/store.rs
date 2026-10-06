//! The event log: one SQLite database per project, `.yalper/yalper.db`.
//!
//! Hook processes can run at the same time (parallel tool calls), so every write is made while holding the
//! [`WriterLock`], taken after opening the store. SQLite runs in WAL mode, so readers such as `yalper log`
//! never take the lock (except for a moment if the database still has to be created) and are not blocked by
//! a writer.

mod lock;

use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::Value;

use crate::repo::Token;
use crate::safe_fs::{OwnedDir, PathGuard};

pub use lock::{LOCK_FILE, LOCK_TIMEOUT, WriterLock};

/// The database file inside `.yalper/`.
pub const DATABASE_FILE: &str = "yalper.db";

/// SQLite's write-ahead log, next to the database.
const WAL_FILE: &str = "yalper.db-wal";

/// Files SQLite creates next to the database while it works.
const DATABASE_SIDE_FILES: [&str; 3] = [WAL_FILE, "yalper.db-shm", "yalper.db-journal"];

/// When the WAL is larger than this as a connection closes, it is copied into the database and emptied.
const WAL_TRUNCATE_BYTES: u64 = 256 * 1024;

/// Schema migrations, oldest first. `PRAGMA user_version` holds how many have run. A released entry is
/// never edited: changes go in a new entry at the end.
const MIGRATIONS: &[&str] = &[SCHEMA_V1, SCHEMA_V2, SCHEMA_V3];

const SCHEMA_V1: &str = "
CREATE TABLE sessions (
    id              TEXT PRIMARY KEY,
    started_at_ms   INTEGER NOT NULL,
    ended_at_ms     INTEGER,
    end_reason      TEXT,
    source          TEXT,
    model           TEXT,
    cwd             TEXT,
    transcript_path TEXT
);

CREATE TABLE events (
    id            INTEGER PRIMARY KEY,
    session_id    TEXT NOT NULL REFERENCES sessions (id),
    step          INTEGER NOT NULL,
    ts_ms         INTEGER NOT NULL,
    kind          TEXT NOT NULL,
    tool_name     TEXT,
    tool_use_id   TEXT,
    agent_id      TEXT,
    success       INTEGER,
    tree_id       TEXT,
    files_changed INTEGER,
    payload       TEXT NOT NULL
);

CREATE UNIQUE INDEX events_session_step ON events (session_id, step);

CREATE TABLE file_cache (
    path     TEXT PRIMARY KEY,
    size     INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    mode     INTEGER NOT NULL,
    oid      TEXT NOT NULL,
    racy     INTEGER NOT NULL
) WITHOUT ROWID;
";

/// The tree of the latest snapshot. It is saved in the same transaction as the `file_cache` rows, which
/// describe the files of exactly this tree, so the next snapshot can be built from it.
const SCHEMA_V2: &str = "
CREATE TABLE latest_snapshot (
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    tree_id TEXT NOT NULL
);
";

/// Facts about the database itself. `init_token` holds the init token of the `.yalper/` it was created in (see
/// `crate::repo`), written in the transaction that creates the schema.
const SCHEMA_V3: &str = "
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
";

/// The `meta` key of the init token.
const INIT_TOKEN_KEY: &str = "init_token";

/// One Claude Code session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub end_reason: Option<String>,
    pub source: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub transcript_path: Option<String>,
}

impl Session {
    /// A session with only its id and start time known.
    pub fn new(id: impl Into<String>, started_at_ms: i64) -> Self {
        Self {
            id: id.into(),
            started_at_ms,
            ended_at_ms: None,
            end_reason: None,
            source: None,
            model: None,
            cwd: None,
            transcript_path: None,
        }
    }
}

/// One recorded step of a session.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub session_id: String,
    /// 1 for the first step of the session, then counting up with no gaps.
    pub step: u32,
    pub ts_ms: i64,
    pub kind: String,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub agent_id: Option<String>,
    pub success: Option<bool>,
    /// The snapshot of the working tree after this step.
    pub tree_id: Option<String>,
    pub files_changed: Option<u32>,
    /// The hook payload, already redacted.
    pub payload: Value,
}

/// What the stat cache knew about one file of the latest snapshot when the file was last read.
///
/// Rows come from a file on disk, so callers must check them before use (see `snapshot::snapshot`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedFile {
    /// Relative to the project root, with `/` as separator.
    pub path: String,
    pub size: i64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// The git file mode: 0o100644, 0o100755 or 0o120000.
    pub mode: i64,
    /// The blob id, in hex.
    pub oid: String,
    /// The file was read so soon after its last change that a later change in the same timestamp tick could
    /// go unnoticed, so it is read again next time whatever its size and mtime.
    pub racy: bool,
}

/// The tree of the latest snapshot and the stat cache rows that describe its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCache {
    pub tree_id: String,
    pub files: Vec<CachedFile>,
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Sqlite(rusqlite::Error),
    /// The database was written by a newer Yalper, with a schema this version does not know.
    NewerSchema {
        found: u32,
        known: u32,
    },
    /// The database holds tables, indexes, triggers or views that Yalper did not create, for example a
    /// crafted `yalper.db` committed to a repository.
    UnexpectedSchema,
    /// The database was not created for this `.yalper/`: it has no init token or another one, for example a
    /// `yalper.db` that a pulled commit wrote over the local one.
    ForeignDatabase,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Sqlite(error) => write!(f, "database error: {error}"),
            Self::NewerSchema { found, known } => write!(
                f,
                "the database has schema version {found}, but this Yalper only knows up to version \
                 {known}: update Yalper"
            ),
            Self::UnexpectedSchema => write!(
                f,
                "the database contains tables, indexes, triggers or views Yalper did not create, so \
                 it is not used"
            ),
            Self::ForeignDatabase => write!(
                f,
                "the database was not created by `yalper init` for this .yalper folder (its init token \
                 does not match), so it is not used"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::NewerSchema { .. } | Self::UnexpectedSchema | Self::ForeignDatabase => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// An open event log.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
    wal_path: PathBuf,
    token: Token,
    /// See [`OwnedDir::guard_path`]: on Windows it keeps the database file from being renamed or deleted.
    _database: PathGuard,
}

impl Store {
    /// Opens the database in `dir`, creating it and bringing its schema up to date if needed. `token` is the
    /// init token of `dir` (see [`crate::repo::open_yalper_dir`]): a new database stores it, and a database
    /// that holds no token or another one is refused with [`Error::ForeignDatabase`].
    ///
    /// Creating or migrating takes the [`WriterLock`] for a moment, so a writer opens the store first and
    /// takes the lock after: opening while this process already holds the lock would wait for itself.
    pub fn open(dir: &OwnedDir, token: &Token) -> Result<Self> {
        // SQLite opens its files by path, so they are checked by name first: anything but a regular file
        // with a single link is refused. SQLite then refuses a symlink anywhere in the path (Unix;
        // `dir.path()` is canonical), and the file it reached is compared with the one checked (Unix: device
        // and inode; Windows: a handle kept open stops renames and deletes).
        // Remaining gap, outside the threat model: another process of the same user could swap a file
        // between these checks and SQLite's own opens (and swap it back), or swap the journal and WAL files.
        let database = dir.guard_path(DATABASE_FILE)?;
        for name in DATABASE_SIDE_FILES {
            dir.check_regular_or_missing(name)?;
        }
        let mut conn = Connection::open_with_flags(
            dir.path().join(DATABASE_FILE),
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        dir.check_guarded(DATABASE_FILE, &database)?;

        // A repository could commit a crafted database: no schema changes outside SQL, and no functions
        // with side effects from the schema.
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
        // Every hook is a new process, so each close is the last connection's close, where SQLite would
        // copy the WAL into the database and delete it: about 4 ms per hook on Windows, half of the
        // database time (measured). The WAL is kept instead and emptied only once it grows, see `drop`.
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
        conn.busy_timeout(LOCK_TIMEOUT)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        if user_version(&conn)? != schema_version(MIGRATIONS) {
            // Switching a new database to WAL fails at once (SQLITE_BUSY) when several processes try it
            // together, so creation and migrations happen one process at a time.
            let _lock = WriterLock::acquire(dir, LOCK_TIMEOUT)?;
            // On a file system without WAL support (network shares) SQLite keeps its default mode, which
            // still works.
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
            migrate(&mut conn, MIGRATIONS, Some(token))?;
        }
        check_schema(&conn)?;
        if stored_token(&conn)?.as_deref() != Some(token.as_str()) {
            return Err(Error::ForeignDatabase);
        }
        Ok(Self {
            conn,
            wal_path: dir.path().join(WAL_FILE),
            token: token.clone(),
            _database: database,
        })
    }

    /// The init token this database belongs to.
    pub fn token(&self) -> &Token {
        &self.token
    }

    /// Whether a snapshot was saved: the latest tree is known.
    pub fn has_snapshot(&self) -> Result<bool> {
        let found = self
            .conn
            .prepare_cached("SELECT 1 FROM latest_snapshot WHERE id = 1")?
            .exists([])?;
        Ok(found)
    }

    /// Creates the session, or updates it: a field given as `None` keeps its stored value, and the start
    /// time of an existing session never changes.
    pub fn upsert_session(&self, session: &Session) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO sessions
                     (id, started_at_ms, ended_at_ms, end_reason, source, model, cwd, transcript_path)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (id) DO UPDATE SET
                     ended_at_ms = COALESCE(excluded.ended_at_ms, ended_at_ms),
                     end_reason = COALESCE(excluded.end_reason, end_reason),
                     source = COALESCE(excluded.source, source),
                     model = COALESCE(excluded.model, model),
                     cwd = COALESCE(excluded.cwd, cwd),
                     transcript_path = COALESCE(excluded.transcript_path, transcript_path)",
            )?
            .execute(params![
                session.id,
                session.started_at_ms,
                session.ended_at_ms,
                session.end_reason,
                session.source,
                session.model,
                session.cwd,
                session.transcript_path,
            ])?;
        Ok(())
    }

    /// Marks a session as running again, for a session that is resumed after it ended: its end time and
    /// reason are cleared.
    pub fn reopen_session(&self, id: &str) -> Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE sessions SET ended_at_ms = NULL, end_reason = NULL
                 WHERE id = ?1 AND ended_at_ms IS NOT NULL",
            )?
            .execute([id])?;
        Ok(())
    }

    /// The number the next step of `session_id` gets. Only meaningful while holding the [`WriterLock`].
    pub fn next_step(&self, session_id: &str) -> Result<u32> {
        let step = self
            .conn
            .prepare_cached("SELECT COALESCE(MAX(step), 0) + 1 FROM events WHERE session_id = ?1")?
            .query_row([session_id], |row| row.get(0))?;
        Ok(step)
    }

    /// Adds a step. Its session must exist, and its step number must not be taken.
    pub fn insert_event(&self, event: &Event) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO events (session_id, step, ts_ms, kind, tool_name, tool_use_id, agent_id,
                                     success, tree_id, files_changed, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?
            .execute(params![
                event.session_id,
                event.step,
                event.ts_ms,
                event.kind,
                event.tool_name,
                event.tool_use_id,
                event.agent_id,
                event.success,
                event.tree_id,
                event.files_changed,
                event.payload,
            ])?;
        Ok(())
    }

    /// Every session, newest first.
    pub fn sessions(&self) -> Result<Vec<Session>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions ORDER BY started_at_ms DESC, id"
        ))?;
        let sessions = statement
            .query_map([], session_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(sessions)
    }

    /// The steps of a session, in order.
    pub fn events(&self, session_id: &str) -> Result<Vec<Event>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE session_id = ?1 ORDER BY step"
        ))?;
        let events = statement
            .query_map([session_id], event_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(events)
    }

    /// One step of a session, if it exists.
    pub fn event(&self, session_id: &str, step: u32) -> Result<Option<Event>> {
        let event = self
            .conn
            .query_row(
                &format!("SELECT {EVENT_COLUMNS} FROM events WHERE session_id = ?1 AND step = ?2"),
                params![session_id, step],
                event_from_row,
            )
            .optional()?;
        Ok(event)
    }

    /// The latest snapshot and its stat cache, or `None` if there is no snapshot yet or a stored value has a
    /// type Yalper never writes (a damaged or crafted database). Either way the next snapshot starts over.
    pub fn file_cache(&self) -> Result<Option<FileCache>> {
        // `as_str` fails on a value that is not valid UTF-8 text.
        let tree_id = self
            .conn
            .prepare_cached("SELECT tree_id FROM latest_snapshot WHERE id = 1")?
            .query_row([], |row| {
                Ok(row.get_ref(0)?.as_str().ok().map(str::to_owned))
            })
            .optional()?;
        let Some(Some(tree_id)) = tree_id else {
            return Ok(None);
        };
        let files: Option<Vec<CachedFile>> = self
            .conn
            .prepare_cached("SELECT path, size, mtime_ns, mode, oid, racy FROM file_cache")?
            .query_map([], cached_file_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(files.map(|files| FileCache { tree_id, files }))
    }

    /// Saves a new latest snapshot and brings the stat cache in line with it, in one transaction: `changed`
    /// rows are added or replaced and `removed` paths deleted. With `replace_all`, every other row is deleted
    /// first. Taking the [`WriterLock`] makes sure no other process saves a snapshot at the same time.
    pub fn save_snapshot(
        &self,
        _lock: &WriterLock,
        tree_id: &str,
        changed: &[CachedFile],
        removed: &[String],
        replace_all: bool,
    ) -> Result<()> {
        let transaction = self.conn.unchecked_transaction()?;
        if replace_all {
            transaction.execute("DELETE FROM file_cache", [])?;
        }
        {
            let mut upsert = transaction.prepare_cached(
                "INSERT OR REPLACE INTO file_cache (path, size, mtime_ns, mode, oid, racy)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for file in changed {
                upsert.execute(params![
                    file.path,
                    file.size,
                    file.mtime_ns,
                    file.mode,
                    file.oid,
                    file.racy
                ])?;
            }
            let mut delete =
                transaction.prepare_cached("DELETE FROM file_cache WHERE path = ?1")?;
            for path in removed {
                delete.execute([path])?;
            }
            transaction
                .prepare_cached(
                    "INSERT OR REPLACE INTO latest_snapshot (id, tree_id) VALUES (1, ?1)",
                )?
                .execute([tree_id])?;
        }
        transaction.commit()?;
        Ok(())
    }
}

/// A stat cache row, or `None` if a value has an unexpected type.
fn cached_file_from_row(row: &Row<'_>) -> rusqlite::Result<Option<CachedFile>> {
    use rusqlite::types::ValueRef::{Integer, Text};
    let values = (
        row.get_ref(0)?,
        row.get_ref(1)?,
        row.get_ref(2)?,
        row.get_ref(3)?,
        row.get_ref(4)?,
        row.get_ref(5)?,
    );
    let (
        Text(path),
        Integer(size),
        Integer(mtime_ns),
        Integer(mode),
        Text(oid),
        Integer(racy @ (0 | 1)),
    ) = values
    else {
        return Ok(None);
    };
    let (Ok(path), Ok(oid)) = (std::str::from_utf8(path), std::str::from_utf8(oid)) else {
        return Ok(None);
    };
    Ok(Some(CachedFile {
        path: path.to_owned(),
        size,
        mtime_ns,
        mode,
        oid: oid.to_owned(),
        racy: racy == 1,
    }))
}

impl Drop for Store {
    fn drop(&mut self) {
        // When no other connection is open, as for almost every hook, opening the database reads the whole
        // WAL, so a WAL that only grew would make every hook slower (measured: about 2 ms more per MiB on
        // Windows). Once it is large, it is copied into the database and emptied. If another process is
        // using the database right now, this is left for a later close instead of waiting.
        let wal_bytes = fs::symlink_metadata(&self.wal_path).map_or(0, |metadata| metadata.len());
        if wal_bytes > WAL_TRUNCATE_BYTES {
            let _ = self.conn.busy_timeout(Duration::ZERO);
            let _ = self
                .conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        }
    }
}

const SESSION_COLUMNS: &str =
    "id, started_at_ms, ended_at_ms, end_reason, source, model, cwd, transcript_path";

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        started_at_ms: row.get(1)?,
        ended_at_ms: row.get(2)?,
        end_reason: row.get(3)?,
        source: row.get(4)?,
        model: row.get(5)?,
        cwd: row.get(6)?,
        transcript_path: row.get(7)?,
    })
}

const EVENT_COLUMNS: &str = "session_id, step, ts_ms, kind, tool_name, tool_use_id, agent_id, \
                             success, tree_id, files_changed, payload";

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        session_id: row.get(0)?,
        step: row.get(1)?,
        ts_ms: row.get(2)?,
        kind: row.get(3)?,
        tool_name: row.get(4)?,
        tool_use_id: row.get(5)?,
        agent_id: row.get(6)?,
        success: row.get(7)?,
        tree_id: row.get(8)?,
        files_changed: row.get(9)?,
        payload: row.get(10)?,
    })
}

/// Runs the migrations the database has not had yet, all in one transaction. The caller holds the
/// [`WriterLock`], so no other process migrates at the same time.
///
/// A database created by this call (it had no schema yet) also gets `token` in `meta`, in the same
/// transaction. An existing database never does: a token is only ever written into a database Yalper
/// creates.
fn migrate(conn: &mut Connection, migrations: &[&str], token: Option<&Token>) -> Result<()> {
    let known = schema_version(migrations);
    let found = user_version(conn)?;
    if found == known {
        return Ok(());
    }
    if found > known {
        return Err(Error::NewerSchema { found, known });
    }
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (version, sql) in (1..).zip(migrations).skip(found as usize) {
        transaction.execute_batch(sql)?;
        transaction.pragma_update(None, "user_version", version)?;
    }
    if let (0, Some(token)) = (found, token) {
        transaction.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)",
            [INIT_TOKEN_KEY, token.as_str()],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

/// The init token stored in the database, if any. A value that is not text counts as none.
fn stored_token(conn: &Connection) -> rusqlite::Result<Option<String>> {
    let token = conn
        .prepare_cached("SELECT value FROM meta WHERE key = ?1")?
        .query_row([INIT_TOKEN_KEY], |row| {
            Ok(row.get_ref(0)?.as_str().ok().map(str::to_owned))
        })
        .optional()?;
    Ok(token.flatten())
}

fn schema_version(migrations: &[&str]) -> u32 {
    u32::try_from(migrations.len()).expect("fewer than 4 billion migrations")
}

fn user_version(conn: &Connection) -> rusqlite::Result<u32> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

/// Refuses a database whose tables, indexes, triggers and views differ in any way from the ones
/// [`MIGRATIONS`] create, compared with a fresh in-memory copy.
fn check_schema(conn: &Connection) -> Result<()> {
    let mut expected = Connection::open_in_memory()?;
    migrate(&mut expected, MIGRATIONS, None)?;
    if schema(conn)? == schema(&expected)? {
        Ok(())
    } else {
        Err(Error::UnexpectedSchema)
    }
}

type SchemaRow = (String, String, String, Option<String>);

fn schema(conn: &Connection) -> rusqlite::Result<Vec<SchemaRow>> {
    conn.prepare("SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name")?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::thread;
    use std::time::{Duration, Instant};

    fn event(session_id: &str, step: u32) -> Event {
        Event {
            session_id: session_id.to_owned(),
            step,
            ts_ms: 1_000 + i64::from(step),
            kind: "tool".to_owned(),
            tool_name: Some("Bash".to_owned()),
            tool_use_id: Some(format!("toolu_{step}")),
            agent_id: None,
            success: Some(true),
            tree_id: Some("4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_owned()),
            files_changed: Some(2),
            payload: json!({"tool_input": {"command": "cargo test"}, "step": step}),
        }
    }

    fn token() -> Token {
        Token::parse("0123456789abcdef0123456789abcdef").unwrap()
    }

    #[test]
    fn the_schema_is_created_on_first_open() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = Store::open(&owned, &token()).unwrap();

        assert_eq!(user_version(&store.conn).unwrap(), 3);
        let names: Vec<String> = schema(&store.conn)
            .unwrap()
            .into_iter()
            .filter(|(_, _, _, sql)| sql.is_some())
            .map(|(_, name, _, _)| name)
            .collect();
        assert_eq!(
            names,
            [
                "events_session_step",
                "events",
                "file_cache",
                "latest_snapshot",
                "meta",
                "sessions"
            ]
        );
        let mode: String = store
            .conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let foreign_keys: bool = store
            .conn
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert!(foreign_keys);
        let synchronous: u32 = store
            .conn
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 1, "NORMAL");
        let busy_timeout_ms: u32 = store
            .conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        assert_eq!(busy_timeout_ms, 3000);
        let config = |option| store.conn.db_config(option).unwrap();
        assert!(config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE));
        assert!(!config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA));
        assert!(config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE));
    }

    #[test]
    fn many_processes_can_create_the_database_at_once() {
        const OPENERS: usize = 8;
        for _ in 0..10 {
            let dir = tempfile::tempdir().unwrap();
            let start = std::sync::Barrier::new(OPENERS);
            thread::scope(|scope| {
                let openers: Vec<_> = (0..OPENERS)
                    .map(|_| {
                        scope.spawn(|| {
                            let owned = OwnedDir::open(dir.path()).unwrap();
                            start.wait();
                            Store::open(&owned, &token())
                                .map(|store| store.sessions().unwrap().len())
                        })
                    })
                    .collect();
                for opener in openers {
                    assert_eq!(opener.join().unwrap().unwrap(), 0);
                }
            });
        }
    }

    /// Opens the database behind Yalper's back, the way a crafted file in a repository could be made.
    fn tamper(dir: &tempfile::TempDir, sql: &str) {
        let owned = OwnedDir::open(dir.path()).unwrap();
        drop(Store::open(&owned, &token()).unwrap());
        Connection::open(dir.path().join(DATABASE_FILE))
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }

    #[test]
    fn a_database_with_a_trigger_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        tamper(
            &dir,
            "CREATE TRIGGER wipe AFTER INSERT ON events BEGIN DELETE FROM sessions; END;",
        );
        let owned = OwnedDir::open(dir.path()).unwrap();
        assert!(matches!(
            Store::open(&owned, &token()),
            Err(Error::UnexpectedSchema)
        ));
    }

    #[test]
    fn a_database_with_an_extra_view_or_changed_table_is_refused() {
        for sql in [
            "CREATE VIEW extra AS SELECT 1;",
            "CREATE TABLE extra (x);",
            "ALTER TABLE events ADD COLUMN extra TEXT DEFAULT 'x';",
            "DROP INDEX events_session_step;",
        ] {
            let dir = tempfile::tempdir().unwrap();
            tamper(&dir, sql);
            let owned = OwnedDir::open(dir.path()).unwrap();
            assert!(
                matches!(Store::open(&owned, &token()), Err(Error::UnexpectedSchema)),
                "{sql}"
            );
        }
    }

    #[test]
    fn reopening_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = Store::open(&owned, &token()).unwrap();
        store.upsert_session(&Session::new("s1", 1)).unwrap();
        store.insert_event(&event("s1", 1)).unwrap();
        let before = schema(&store.conn).unwrap();
        drop(store);

        let store = Store::open(&owned, &token()).unwrap();
        assert_eq!(schema(&store.conn).unwrap(), before);
        assert_eq!(user_version(&store.conn).unwrap(), 3);
        assert_eq!(store.events("s1").unwrap(), [event("s1", 1)]);
    }

    #[test]
    fn migrations_run_once_and_only_new_ones_run_later() {
        let mut conn = Connection::open_in_memory().unwrap();
        let first = "CREATE TABLE runs (migration INTEGER); INSERT INTO runs VALUES (1);";
        let second = "INSERT INTO runs VALUES (2);";
        let third = "INSERT INTO runs VALUES (3);";
        let runs = |conn: &Connection| -> Vec<u32> {
            let mut statement = conn
                .prepare("SELECT migration FROM runs ORDER BY rowid")
                .unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };

        migrate(&mut conn, &[first, second], None).unwrap();
        migrate(&mut conn, &[first, second], None).unwrap();
        assert_eq!(runs(&conn), [1, 2]);
        assert_eq!(user_version(&conn).unwrap(), 2);

        migrate(&mut conn, &[first, second, third], None).unwrap();
        assert_eq!(runs(&conn), [1, 2, 3]);
        assert_eq!(user_version(&conn).unwrap(), 3);
    }

    #[test]
    fn a_failed_migration_leaves_the_database_unchanged() {
        let mut conn = Connection::open_in_memory().unwrap();
        let good = "CREATE TABLE runs (migration INTEGER);";
        let bad = "INSERT INTO runs VALUES (2); THIS IS NOT SQL;";
        assert!(migrate(&mut conn, &[good, bad], None).is_err());
        assert_eq!(user_version(&conn).unwrap(), 0);
        let tables: u32 = conn
            .query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| row.get(0))
            .unwrap();
        assert_eq!(tables, 0);
    }

    #[test]
    fn a_database_from_a_newer_yalper_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = Store::open(&owned, &token()).unwrap();
        store.conn.pragma_update(None, "user_version", 99).unwrap();
        drop(store);

        let error = Store::open(&owned, &token()).unwrap_err();
        assert!(
            matches!(
                error,
                Error::NewerSchema {
                    found: 99,
                    known: 3
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn a_database_belongs_to_the_token_it_was_created_with() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = Store::open(&owned, &token()).unwrap();
        assert_eq!(store.token(), &token());
        assert_eq!(
            stored_token(&store.conn).unwrap().as_deref(),
            Some(token().as_str())
        );
        drop(store);

        let other = Token::parse("fedcba9876543210fedcba9876543210").unwrap();
        assert!(matches!(
            Store::open(&owned, &other),
            Err(Error::ForeignDatabase)
        ));
        Store::open(&owned, &token()).unwrap();
    }

    #[test]
    fn a_database_without_a_token_is_refused_and_not_given_one() {
        for sql in [
            "DELETE FROM meta",
            "UPDATE meta SET value = X'30'",
            // A database from before the token: migrating it must not adopt it.
            "DROP TABLE meta; PRAGMA user_version = 2;",
        ] {
            let dir = tempfile::tempdir().unwrap();
            tamper(&dir, sql);
            let owned = OwnedDir::open(dir.path()).unwrap();
            assert!(
                matches!(Store::open(&owned, &token()), Err(Error::ForeignDatabase)),
                "{sql}"
            );
            let conn = Connection::open(dir.path().join(DATABASE_FILE)).unwrap();
            assert_eq!(stored_token(&conn).unwrap(), None, "{sql}");
        }
    }

    #[test]
    fn has_snapshot_tells_whether_a_tree_was_saved() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = Store::open(&owned, &token()).unwrap();
        assert!(!store.has_snapshot().unwrap());
        let lock = WriterLock::acquire(&owned, LOCK_TIMEOUT).unwrap();
        store.save_snapshot(&lock, "tree", &[], &[], true).unwrap();
        assert!(store.has_snapshot().unwrap());
    }

    #[test]
    fn a_session_keeps_its_start_and_gains_fields() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = Store::open(&owned, &token()).unwrap();

        let mut start = Session::new("s1", 100);
        start.source = Some("startup".to_owned());
        start.model = Some("model-a".to_owned());
        start.cwd = Some("/repo".to_owned());
        start.transcript_path = Some("/t.jsonl".to_owned());
        store.upsert_session(&start).unwrap();

        let mut end = Session::new("s1", 900);
        end.ended_at_ms = Some(950);
        end.end_reason = Some("prompt_input_exit".to_owned());
        store.upsert_session(&end).unwrap();

        let mut expected = start.clone();
        expected.ended_at_ms = Some(950);
        expected.end_reason = Some("prompt_input_exit".to_owned());
        assert_eq!(store.sessions().unwrap(), [expected]);

        store.reopen_session("s1").unwrap();
        assert_eq!(store.sessions().unwrap(), [start]);
    }

    #[test]
    fn sessions_are_listed_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = Store::open(&owned, &token()).unwrap();
        for (id, start) in [("old", 1), ("new", 3), ("middle", 2)] {
            store.upsert_session(&Session::new(id, start)).unwrap();
        }
        let ids: Vec<String> = store
            .sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, ["new", "middle", "old"]);
    }

    #[test]
    fn events_round_trip_and_steps_count_per_session() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = Store::open(&owned, &token()).unwrap();
        store.upsert_session(&Session::new("a", 1)).unwrap();
        store.upsert_session(&Session::new("b", 2)).unwrap();
        assert_eq!(store.next_step("a").unwrap(), 1);

        store.insert_event(&event("a", 1)).unwrap();
        store.insert_event(&event("a", 2)).unwrap();
        let mut sparse = event("b", 1);
        sparse.tool_name = None;
        sparse.tool_use_id = None;
        sparse.success = None;
        sparse.tree_id = None;
        sparse.files_changed = None;
        sparse.kind = "prompt".to_owned();
        sparse.payload = json!({"prompt": "hello"});
        store.insert_event(&sparse).unwrap();

        assert_eq!(store.next_step("a").unwrap(), 3);
        assert_eq!(store.next_step("b").unwrap(), 2);
        assert_eq!(store.events("a").unwrap(), [event("a", 1), event("a", 2)]);
        assert_eq!(store.event("b", 1).unwrap(), Some(sparse));
        assert_eq!(store.event("b", 2).unwrap(), None);
    }

    #[test]
    fn a_taken_step_or_an_unknown_session_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = Store::open(&owned, &token()).unwrap();
        store.upsert_session(&Session::new("a", 1)).unwrap();
        store.insert_event(&event("a", 1)).unwrap();
        assert!(store.insert_event(&event("a", 1)).is_err());
        assert!(store.insert_event(&event("unknown", 1)).is_err());
    }

    #[test]
    fn concurrent_writers_get_unique_gap_free_steps() {
        const WRITERS: usize = 8;
        const EVENTS_PER_WRITER: usize = 50;
        let dir = tempfile::tempdir().unwrap();
        {
            let owned = OwnedDir::open(dir.path()).unwrap();
            let store = Store::open(&owned, &token()).unwrap();
            store.upsert_session(&Session::new("even", 1)).unwrap();
            store.upsert_session(&Session::new("odd", 2)).unwrap();
        }

        thread::scope(|scope| {
            for writer in 0..WRITERS {
                let path = dir.path();
                scope.spawn(move || {
                    let session = if writer % 2 == 0 { "even" } else { "odd" };
                    for _ in 0..EVENTS_PER_WRITER {
                        // What each hook process does: open, lock, number the step, insert.
                        let owned = OwnedDir::open(path).unwrap();
                        let store = Store::open(&owned, &token()).unwrap();
                        let _lock = WriterLock::acquire(&owned, LOCK_TIMEOUT).unwrap();
                        let step = store.next_step(session).unwrap();
                        store.insert_event(&event(session, step)).unwrap();
                    }
                });
            }
        });

        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = Store::open(&owned, &token()).unwrap();
        let mut total = 0;
        let mut steps_per_session = HashMap::new();
        for session in ["even", "odd"] {
            let steps: Vec<u32> = store
                .events(session)
                .unwrap()
                .iter()
                .map(|event| event.step)
                .collect();
            total += steps.len();
            steps_per_session.insert(session, steps);
        }
        assert_eq!(total, WRITERS * EVENTS_PER_WRITER);
        let per_session = u32::try_from(WRITERS * EVENTS_PER_WRITER / 2).unwrap();
        for steps in steps_per_session.values() {
            assert_eq!(*steps, (1..=per_session).collect::<Vec<_>>());
        }
    }

    /// Runs [`other_process`] on `database` in a new process of this test binary and returns whether its
    /// checks passed.
    fn run_other_process(database: &std::path::Path) -> bool {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "store::tests::other_process", "--ignored"])
            .env(OTHER_PROCESS_DATABASE, database)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    const OTHER_PROCESS_DATABASE: &str = "YALPER_TEST_OTHER_PROCESS_DATABASE";

    /// Another process using the database the way any SQLite client would (the `sqlite3` shell, a
    /// viewer): while a store holds a write transaction, it must not get the write lock.
    #[test]
    #[ignore = "run in a child process by closing_one_store_keeps_the_locks_of_another"]
    fn other_process() {
        let Some(path) = std::env::var_os(OTHER_PROCESS_DATABASE) else {
            return;
        };
        let conn = Connection::open(path).unwrap();
        conn.busy_timeout(Duration::ZERO).unwrap();
        let error = conn.execute_batch("BEGIN IMMEDIATE").unwrap_err();
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy),
            "{error}"
        );
        let rows: u32 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1, "the other process sees only committed steps");
    }

    #[test]
    fn closing_one_store_keeps_the_locks_of_another() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let database = owned.path().join(DATABASE_FILE);
        let first = Store::open(&owned, &token()).unwrap();
        first.upsert_session(&Session::new("s1", 1)).unwrap();
        drop(Store::open(&owned, &token()).unwrap());

        // On Unix, closing any descriptor of a file drops every POSIX lock the process holds on it. The
        // first store's shared lock on the database (it tells other processes the database is in use) must
        // survive the second store. Linux lists the locks of every process in /proc/locks.
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            let inode = format!(":{} ", fs::metadata(&database).unwrap().ino());
            let pid = format!(" {} ", std::process::id());
            let held = fs::read_to_string("/proc/locks")
                .unwrap()
                .lines()
                .any(|line| {
                    line.contains(" POSIX ") && line.contains(&pid) && line.contains(&inode)
                });
            assert!(held, "the first store lost its lock on the database");
        }

        first.insert_event(&event("s1", 1)).unwrap();
        first.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        first.insert_event(&event("s1", 2)).unwrap();
        assert!(run_other_process(&database));
        first.conn.execute_batch("COMMIT").unwrap();
        assert_eq!(first.events("s1").unwrap().len(), 2);
    }

    #[test]
    fn a_reader_is_not_blocked_by_a_writer() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let writer = Store::open(&owned, &token()).unwrap();
        let _lock = WriterLock::acquire(&owned, LOCK_TIMEOUT).unwrap();
        writer.upsert_session(&Session::new("s1", 1)).unwrap();
        writer.insert_event(&event("s1", 1)).unwrap();
        // The writer holds the lock and an open write transaction with an uncommitted step.
        writer.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        writer.insert_event(&event("s1", 2)).unwrap();

        let start = Instant::now();
        let reader = Store::open(&owned, &token()).unwrap();
        assert_eq!(reader.events("s1").unwrap(), [event("s1", 1)]);
        assert_eq!(reader.sessions().unwrap().len(), 1);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );

        writer.conn.execute_batch("COMMIT").unwrap();
        assert_eq!(reader.events("s1").unwrap().len(), 2);
    }

    #[test]
    fn a_link_in_place_of_a_database_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("victim");
        fs::write(&target, "keep me").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, dir.path().join(WAL_FILE)).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(&target, dir.path().join(WAL_FILE)).is_err() {
            // Creating file symlinks needs Developer Mode or admin rights on Windows.
            return;
        }
        let owned = OwnedDir::open(dir.path()).unwrap();
        assert!(Store::open(&owned, &token()).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
    }

    #[test]
    fn a_directory_in_place_of_the_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(DATABASE_FILE)).unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        assert!(matches!(Store::open(&owned, &token()), Err(Error::Io(_))));
    }

    #[cfg(unix)]
    #[test]
    fn a_project_reached_through_a_symlink_works() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir_all(real.join(".yalper")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let owned = OwnedDir::open(&link.join(".yalper")).unwrap();
        let store = Store::open(&owned, &token()).unwrap();
        store.upsert_session(&Session::new("s1", 1)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_swapped_into_the_path_is_refused() {
        let project = tempfile::tempdir().unwrap();
        let yalper = project.path().join(".yalper");
        fs::create_dir(&yalper).unwrap();
        let owned = OwnedDir::open(&yalper).unwrap();
        // After the directory was checked, it is moved away and a symlink to it takes its place.
        let moved = project.path().join("moved");
        fs::rename(&yalper, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &yalper).unwrap();
        assert!(Store::open(&owned, &token()).is_err());
    }

    #[test]
    fn the_wal_is_kept_between_connections_and_emptied_once_large() {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join(WAL_FILE);
        let wal_bytes = || fs::metadata(&wal).unwrap().len();
        let owned = OwnedDir::open(dir.path()).unwrap();

        let store = Store::open(&owned, &token()).unwrap();
        store.upsert_session(&Session::new("s1", 1)).unwrap();
        store.insert_event(&event("s1", 1)).unwrap();
        drop(store);
        assert!(wal_bytes() > 0);
        assert!(wal_bytes() <= WAL_TRUNCATE_BYTES);

        let store = Store::open(&owned, &token()).unwrap();
        let mut large = event("s1", 2);
        large.payload = json!({"tool_response": "x".repeat(2 * WAL_TRUNCATE_BYTES as usize)});
        store.insert_event(&large).unwrap();
        assert!(wal_bytes() > WAL_TRUNCATE_BYTES);
        drop(store);
        assert_eq!(wal_bytes(), 0);

        let store = Store::open(&owned, &token()).unwrap();
        assert_eq!(store.events("s1").unwrap(), [event("s1", 1), large]);
    }

    fn cached(path: &str, oid: &str) -> CachedFile {
        CachedFile {
            path: path.to_owned(),
            size: 12,
            mtime_ns: 1_700_000_000_123_456_789,
            mode: 0o100644,
            oid: oid.to_owned(),
            racy: false,
        }
    }

    fn sorted(cache: Option<FileCache>) -> Option<FileCache> {
        cache.map(|mut cache| {
            cache.files.sort_by(|a, b| a.path.cmp(&b.path));
            cache
        })
    }

    #[test]
    fn the_file_cache_is_saved_with_its_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = Store::open(&owned, &token()).unwrap();
        assert_eq!(store.file_cache().unwrap(), None);
        let lock = WriterLock::acquire(&owned, LOCK_TIMEOUT).unwrap();

        let (a, b) = (cached("a.rs", "aa"), cached("dir/b.rs", "bb"));
        store
            .save_snapshot(&lock, "tree1", &[a.clone(), b.clone()], &[], false)
            .unwrap();
        let expected = FileCache {
            tree_id: "tree1".to_owned(),
            files: vec![a.clone(), b.clone()],
        };
        assert_eq!(sorted(store.file_cache().unwrap()), Some(expected));

        let mut racy_a = cached("a.rs", "a2");
        racy_a.racy = true;
        let c = cached("c.rs", "cc");
        store
            .save_snapshot(
                &lock,
                "tree2",
                &[racy_a.clone(), c.clone()],
                &["dir/b.rs".to_owned()],
                false,
            )
            .unwrap();
        let expected = FileCache {
            tree_id: "tree2".to_owned(),
            files: vec![racy_a, c.clone()],
        };
        assert_eq!(sorted(store.file_cache().unwrap()), Some(expected));

        store
            .save_snapshot(&lock, "tree3", std::slice::from_ref(&b), &[], true)
            .unwrap();
        let expected = FileCache {
            tree_id: "tree3".to_owned(),
            files: vec![b],
        };
        assert_eq!(store.file_cache().unwrap(), Some(expected));
    }

    #[test]
    fn a_file_cache_with_unexpected_values_is_not_used() {
        for sql in [
            "UPDATE file_cache SET size = 'big'",
            "UPDATE file_cache SET racy = 2",
            "UPDATE file_cache SET oid = X'0102'",
            "UPDATE file_cache SET mtime_ns = 1.5",
            "UPDATE file_cache SET path = CAST(X'FF' AS TEXT)",
            "UPDATE latest_snapshot SET tree_id = X'07'",
            "DELETE FROM latest_snapshot",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let owned = OwnedDir::open(dir.path()).unwrap();
            let store = Store::open(&owned, &token()).unwrap();
            let lock = WriterLock::acquire(&owned, LOCK_TIMEOUT).unwrap();
            store
                .save_snapshot(&lock, "tree", &[cached("a.rs", "aa")], &[], false)
                .unwrap();
            store.conn.execute_batch(sql).unwrap();
            assert_eq!(store.file_cache().unwrap(), None, "{sql}");
        }
    }
}
