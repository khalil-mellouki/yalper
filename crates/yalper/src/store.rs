//! The event log: one SQLite database per project, `.yalper/yalper.db`.
//!
//! Hook processes can run at the same time (parallel tool calls), so every write is made while holding the
//! [`WriterLock`], and the database is opened after taking it. SQLite runs in WAL mode, so readers such as
//! `yalper log` never take the lock and are not blocked by a writer.

mod lock;

use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::Value;

use crate::safe_fs::{Access, OwnedDir};

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
const MIGRATIONS: &[&str] = &[SCHEMA_V1];

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

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Sqlite(rusqlite::Error),
    /// The database was written by a newer Yalper, with a schema this version does not know.
    NewerSchema {
        found: u32,
        known: u32,
    },
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
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::NewerSchema { .. } => None,
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
}

impl Store {
    /// Opens the database in `dir`, creating it and bringing its schema up to date if needed. Writers must
    /// hold the [`WriterLock`] while calling this and while writing.
    pub fn open(dir: &OwnedDir) -> Result<Self> {
        // SQLite opens its files by path. Creating the database here checks it through a handle, and links
        // planted in place of its other files are refused (SQLite refuses them itself on Unix, but follows
        // them on Windows).
        dir.open_file(DATABASE_FILE, Access::ReadWrite)?;
        for name in DATABASE_SIDE_FILES {
            dir.check_regular_or_missing(name)?;
        }
        let mut conn = Connection::open_with_flags(
            dir.path().join(DATABASE_FILE),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // A database that is already in WAL mode stays so without taking a write lock. On a file system
        // without WAL support (network shares) SQLite keeps its default mode, which still works.
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Every hook is a new process, so each close is the last connection's close, where SQLite would
        // copy the WAL into the database and delete it: about 4 ms per hook on Windows, half of the
        // database time (measured). The WAL is kept instead and emptied only once it grows, see `drop`.
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
        migrate(&mut conn, MIGRATIONS)?;
        Ok(Self {
            conn,
            wal_path: dir.path().join(WAL_FILE),
        })
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

/// Runs the migrations the database has not had yet, each at most once, even when several processes open
/// a new database at the same time.
fn migrate(conn: &mut Connection, migrations: &[&str]) -> Result<()> {
    let known = u32::try_from(migrations.len()).expect("fewer than 4 billion migrations");
    let found = user_version(conn)?;
    if found == known {
        return Ok(());
    }
    if found > known {
        return Err(Error::NewerSchema { found, known });
    }
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Another process may have migrated between the first read and the write lock.
    let found = user_version(&transaction)?;
    for (version, sql) in (1..).zip(migrations).skip(found as usize) {
        transaction.execute_batch(sql)?;
        transaction.pragma_update(None, "user_version", version)?;
    }
    transaction.commit()?;
    Ok(())
}

fn user_version(conn: &Connection) -> rusqlite::Result<u32> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
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

    fn schema(store: &Store) -> Vec<(String, String)> {
        let mut statement = store
            .conn
            .prepare("SELECT name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name")
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn the_schema_is_created_on_first_open() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = Store::open(&owned).unwrap();

        assert_eq!(user_version(&store.conn).unwrap(), 1);
        let names: Vec<String> = schema(&store).into_iter().map(|(name, _)| name).collect();
        assert_eq!(
            names,
            ["events", "events_session_step", "file_cache", "sessions"]
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
    }

    #[test]
    fn reopening_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = Store::open(&owned).unwrap();
        store.upsert_session(&Session::new("s1", 1)).unwrap();
        store.insert_event(&event("s1", 1)).unwrap();
        let before = schema(&store);
        drop(store);

        let store = Store::open(&owned).unwrap();
        assert_eq!(schema(&store), before);
        assert_eq!(user_version(&store.conn).unwrap(), 1);
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

        migrate(&mut conn, &[first, second]).unwrap();
        migrate(&mut conn, &[first, second]).unwrap();
        assert_eq!(runs(&conn), [1, 2]);
        assert_eq!(user_version(&conn).unwrap(), 2);

        migrate(&mut conn, &[first, second, third]).unwrap();
        assert_eq!(runs(&conn), [1, 2, 3]);
        assert_eq!(user_version(&conn).unwrap(), 3);
    }

    #[test]
    fn a_failed_migration_leaves_the_database_unchanged() {
        let mut conn = Connection::open_in_memory().unwrap();
        let good = "CREATE TABLE runs (migration INTEGER);";
        let bad = "INSERT INTO runs VALUES (2); THIS IS NOT SQL;";
        assert!(migrate(&mut conn, &[good, bad]).is_err());
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
        let store = Store::open(&owned).unwrap();
        store.conn.pragma_update(None, "user_version", 99).unwrap();
        drop(store);

        let error = Store::open(&owned).unwrap_err();
        assert!(
            matches!(
                error,
                Error::NewerSchema {
                    found: 99,
                    known: 1
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn a_session_keeps_its_start_and_gains_fields() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = Store::open(&owned).unwrap();

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
    }

    #[test]
    fn sessions_are_listed_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = Store::open(&owned).unwrap();
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
        let store = Store::open(&owned).unwrap();
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
        let store = Store::open(&owned).unwrap();
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
            let _lock = WriterLock::acquire(&owned, LOCK_TIMEOUT).unwrap();
            let store = Store::open(&owned).unwrap();
            store.upsert_session(&Session::new("even", 1)).unwrap();
            store.upsert_session(&Session::new("odd", 2)).unwrap();
        }

        thread::scope(|scope| {
            for writer in 0..WRITERS {
                let path = dir.path();
                scope.spawn(move || {
                    let session = if writer % 2 == 0 { "even" } else { "odd" };
                    for _ in 0..EVENTS_PER_WRITER {
                        // What each hook process does: lock, open, number the step, insert.
                        let owned = OwnedDir::open(path).unwrap();
                        let _lock = WriterLock::acquire(&owned, LOCK_TIMEOUT).unwrap();
                        let store = Store::open(&owned).unwrap();
                        let step = store.next_step(session).unwrap();
                        store.insert_event(&event(session, step)).unwrap();
                    }
                });
            }
        });

        let owned = OwnedDir::open(dir.path()).unwrap();
        let store = Store::open(&owned).unwrap();
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

    #[test]
    fn a_reader_is_not_blocked_by_a_writer() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let _lock = WriterLock::acquire(&owned, LOCK_TIMEOUT).unwrap();
        let writer = Store::open(&owned).unwrap();
        writer.upsert_session(&Session::new("s1", 1)).unwrap();
        writer.insert_event(&event("s1", 1)).unwrap();
        // The writer holds the lock and an open write transaction with an uncommitted step.
        writer.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        writer.insert_event(&event("s1", 2)).unwrap();

        let start = Instant::now();
        let reader = Store::open(&owned).unwrap();
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
        assert!(Store::open(&owned).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
    }

    #[test]
    fn a_directory_in_place_of_the_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(DATABASE_FILE)).unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        assert!(matches!(Store::open(&owned), Err(Error::Io(_))));
    }

    #[test]
    fn the_wal_is_kept_between_connections_and_emptied_once_large() {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join(WAL_FILE);
        let wal_bytes = || fs::metadata(&wal).unwrap().len();
        let owned = OwnedDir::open(dir.path()).unwrap();

        let store = Store::open(&owned).unwrap();
        store.upsert_session(&Session::new("s1", 1)).unwrap();
        store.insert_event(&event("s1", 1)).unwrap();
        drop(store);
        assert!(wal_bytes() > 0);
        assert!(wal_bytes() <= WAL_TRUNCATE_BYTES);

        let store = Store::open(&owned).unwrap();
        let mut large = event("s1", 2);
        large.payload = json!({"tool_response": "x".repeat(2 * WAL_TRUNCATE_BYTES as usize)});
        store.insert_event(&large).unwrap();
        assert!(wal_bytes() > WAL_TRUNCATE_BYTES);
        drop(store);
        assert_eq!(wal_bytes(), 0);

        let store = Store::open(&owned).unwrap();
        assert_eq!(store.events("s1").unwrap(), [event("s1", 1), large]);
    }
}
