//! SQLite storage. A single writer connection behind a mutex: at a handful of
//! nodes reporting every few seconds, every statement on it is sub-millisecond.
//! The history charts, whose scans are not, read through a second connection.

mod backup;
mod metrics;
mod migrate;
mod nodes;
#[cfg(test)]
mod tests;
#[cfg(test)]
#[path = "../testdata/traffic-period.rs"]
mod traffic_period_tests;

use self::migrate::*;
pub use self::{backup::*, metrics::*, nodes::*};

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, TryLockError};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate, Utc};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use tokio::runtime::RuntimeFlavor;
use tracing::{info, warn};

use crate::traffic_period;

/// A string an agent reported, cut down to what may be shown. Control
/// characters break the panel's rows, and the characters markup is made of are
/// dropped because the value reaches a theme from a machine nobody has vouched
/// for: a theme that sets it as HTML would otherwise run a node's script in the
/// hub's origin, beside the administrator's session. No real OS name, kernel,
/// CPU model or host name contains any of them. `chars()` rather than bytes, so
/// the cut falls on a character boundary.
pub fn reported_text(raw: &str, max: usize) -> String {
    raw.chars().filter(|c| !c.is_control() && !matches!(c, '<' | '>' | '"' | '\'')).take(max).collect()
}

pub struct Db {
    /// A read-only connection to the same file, for the history charts. A week
    /// of one node's probe results is a scan of 98 ms at four 60-second probes
    /// and 486 ms at eight 10-second ones. With four of the latter in flight on
    /// `conn`, every agent report and panel request behind them would wait 1.1 s
    /// at the median; with the scans here they wait 1.8 ms, as WAL lets this
    /// connection read while `conn` commits. `None` for `:memory:`, which a second
    /// connection cannot open.
    ///
    /// Declared before `conn` so that it closes first. The last connection to
    /// close folds the WAL into the database file and deletes it, which a
    /// read-only one cannot do, and a stopped hub would otherwise leave rows in
    /// a -wal that a copy of the database file alone misses.
    // ponytail: one reader, so chart requests queue behind one another, at most
    // `api::HISTORY_SLOTS` deep; a pool if that wait becomes visible.
    reader: Option<Mutex<Connection>>,
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
-- 8 MiB of page cache. The whole working set of a few hundred nodes fits, so
-- the read paths stop going back to the filesystem.
PRAGMA cache_size = -8192;
-- Without these the WAL grows to whatever the busiest minute needed and never
-- gives the space back: a hub is a long-running process on a small VPS.
PRAGMA wal_autocheckpoint = 256;
PRAGMA journal_size_limit = 1048576;

CREATE TABLE IF NOT EXISTS setting (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS node (
  id            INTEGER PRIMARY KEY,
  name          TEXT    NOT NULL,
  -- The agent's credential, in the clear: the panel shows a node's install
  -- command whenever it is asked, so it has to be able to read it back.
  token         TEXT    NOT NULL UNIQUE,
  sort          INTEGER NOT NULL DEFAULT 0,
  public        INTEGER NOT NULL DEFAULT 1,
  price         REAL    NOT NULL DEFAULT 0,
  currency      TEXT    NOT NULL DEFAULT 'USD',
  billing_cycle TEXT    NOT NULL DEFAULT 'monthly',
  expires_at    TEXT,
  remark        TEXT    NOT NULL DEFAULT '',
  traffic_limit INTEGER NOT NULL DEFAULT 0,
  traffic_mode  TEXT    NOT NULL DEFAULT 'sum',
  traffic_reset_day INTEGER NOT NULL DEFAULT 1,
  hostname TEXT NOT NULL DEFAULT '', os TEXT NOT NULL DEFAULT '',
  kernel   TEXT NOT NULL DEFAULT '', arch TEXT NOT NULL DEFAULT '',
  virt     TEXT NOT NULL DEFAULT '', cpu_name TEXT NOT NULL DEFAULT '',
  cpu_cores INTEGER NOT NULL DEFAULT 0, mem_total INTEGER NOT NULL DEFAULT 0,
  swap_total INTEGER NOT NULL DEFAULT 0, disk_total INTEGER NOT NULL DEFAULT 0,
  agent_version TEXT NOT NULL DEFAULT '', ip TEXT NOT NULL DEFAULT '',
  ipv4 TEXT NOT NULL DEFAULT '', ipv6 TEXT NOT NULL DEFAULT '',
  -- Where the hub saw the node in the address family `ip` is not in, from the
  -- short connection an agent makes over it (`agent_ws::echo`).
  -- Empty until one has arrived, and left as it was when none does: the family
  -- it belongs to must still be held on an interface for it to be shown.
  ip_other TEXT NOT NULL DEFAULT '',
  -- ISO 3166-1 alpha-2, looked up from `country_ip` once per address. Empty
  -- until the lookup answers, and empty is what a node whose country nobody
  -- could tell stays: the public page just leaves the badge off.
  country TEXT NOT NULL DEFAULT '',
  -- The address `country` belongs to: a public interface address the agent
  -- reported, else `ip`. Empty when neither is public.
  country_ip TEXT NOT NULL DEFAULT '',
  -- The last answered pair `country_ip` / `country` before the current one;
  -- an address never answered does not displace it. A hello taken before
  -- every interface is up picks the other family, and the next one returns;
  -- the address returned to takes its answer back from here instead of
  -- waiting out the hourly lookup limit the detour spent.
  -- One pair suffices: a machine's sources are its v4, or the exit in front of
  -- it, and its v6.
  country_prev_ip TEXT NOT NULL DEFAULT '',
  country_prev TEXT NOT NULL DEFAULT '',
  -- Set in the panel. When not empty it is the country shown, in place of the
  -- looked-up one, which goes on updating underneath.
  country_pin TEXT NOT NULL DEFAULT '',
  -- Set in the panel and shown on the status page, where a theme may divide the
  -- node list by it. Empty is ungrouped. Not `group`, a reserved word.
  group_name TEXT NOT NULL DEFAULT '',
  -- Set in the panel and shown on the status page, unlike `remark`. Empty is
  -- none.
  public_remark TEXT NOT NULL DEFAULT '',
  -- Set in the panel, each replacing the address shown for its family. Empty
  -- means automatic. Panel only, like the reported addresses.
  ipv4_pin TEXT NOT NULL DEFAULT '', ipv6_pin TEXT NOT NULL DEFAULT '',
  -- Survives the disconnection it describes, unlike the in-memory live entry:
  -- an offline node's page is exactly where "since when" is worth reading.
  last_seen INTEGER NOT NULL DEFAULT 0,
  -- Opt-in, as the operator decides which machines are worth an alert.
  notify INTEGER NOT NULL DEFAULT 0,
  -- `last_seen` as of the offline alert, zero while none is outstanding. Stored
  -- rather than held in memory so that a hub restart neither repeats the alert
  -- nor loses the recovery that pairs with it.
  down_since INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);

-- Monotonic byte counters that survive both agent reboots and hub restarts.
CREATE TABLE IF NOT EXISTS traffic (
  node_id  INTEGER PRIMARY KEY REFERENCES node(id) ON DELETE CASCADE,
  boot_id  TEXT    NOT NULL DEFAULT '',
  last_rx  INTEGER NOT NULL DEFAULT 0,
  last_tx  INTEGER NOT NULL DEFAULT 0,
  total_rx INTEGER NOT NULL DEFAULT 0,
  total_tx INTEGER NOT NULL DEFAULT 0,
  month_rx INTEGER NOT NULL DEFAULT 0,
  month_tx INTEGER NOT NULL DEFAULT 0,
  month_start TEXT NOT NULL DEFAULT '',
  day_rx INTEGER NOT NULL DEFAULT 0,
  day_tx INTEGER NOT NULL DEFAULT 0,
  day_start TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS metric (
  node_id INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
  ts      INTEGER NOT NULL,
  cpu REAL NOT NULL,
  mem_used INTEGER NOT NULL, swap_used INTEGER NOT NULL, disk_used INTEGER NOT NULL,
  net_rx INTEGER NOT NULL, net_tx INTEGER NOT NULL,
  tcp INTEGER NOT NULL, udp INTEGER NOT NULL, procs INTEGER NOT NULL,
  net_rx_max INTEGER NOT NULL DEFAULT 0, net_tx_max INTEGER NOT NULL DEFAULT 0,
  cpu_max REAL NOT NULL DEFAULT 0,
  PRIMARY KEY (node_id, ts)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS ping_task (
  id       INTEGER PRIMARY KEY,
  name     TEXT    NOT NULL,
  target   TEXT    NOT NULL,
  interval INTEGER NOT NULL DEFAULT 60,
  auto_join INTEGER NOT NULL DEFAULT 0,
  sort     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS ping_node (
  task_id INTEGER NOT NULL REFERENCES ping_task(id) ON DELETE CASCADE,
  node_id INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
  PRIMARY KEY (task_id, node_id)
);

-- Key order follows the only query there is: one node, one time window,
-- every probe. With task_id ahead of ts SQLite can seek to the node and no
-- further, then scans every record it ever kept -- see migrate_to_1.
CREATE TABLE IF NOT EXISTS ping_record (
  node_id INTEGER NOT NULL, task_id INTEGER NOT NULL,
  ts INTEGER NOT NULL, latency INTEGER NOT NULL,
  PRIMARY KEY (node_id, ts, task_id)
) WITHOUT ROWID;

-- The hourly tier: charts wider than DETAIL_DAYS read these, and they outlive
-- the minute rows they are folded from. `ts` is the start of the hour, and a
-- row covers the minute rows stamped within it. Keyed like the tables above.
--
-- `minutes` counts the rows folded in and weights each average when buckets
-- spanning several hours merge them. Every column of `metric` is kept, whether
-- or not the chart returns it yet: a minute row is gone after DETAIL_DAYS, and
-- a column added here later would start empty.
CREATE TABLE IF NOT EXISTS metric_hour (
  node_id INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
  ts      INTEGER NOT NULL,
  minutes INTEGER NOT NULL,
  cpu REAL NOT NULL, cpu_max REAL NOT NULL,
  mem_used INTEGER NOT NULL, swap_used INTEGER NOT NULL, disk_used INTEGER NOT NULL,
  net_rx INTEGER NOT NULL, net_tx INTEGER NOT NULL,
  net_rx_max INTEGER NOT NULL, net_tx_max INTEGER NOT NULL,
  tcp INTEGER NOT NULL, udp INTEGER NOT NULL, procs INTEGER NOT NULL,
  PRIMARY KEY (node_id, ts)
) WITHOUT ROWID;

-- `latency` is the median of the hour's answers and NULL when none arrived;
-- `lo` and `hi` bound them. `answered` weights the median when buckets merge.
--
-- Foreign keys, unlike `ping_record`: an earlier build run against this file
-- deletes nodes and probes knowing nothing of this table, and SQLite gives a
-- deleted id to the next one created, which would draw the deleted one's
-- hourly latency. The cascade clears the rows whichever build deletes.
CREATE TABLE IF NOT EXISTS ping_hour (
  node_id INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
  task_id INTEGER NOT NULL REFERENCES ping_task(id) ON DELETE CASCADE,
  ts INTEGER NOT NULL,
  answered INTEGER NOT NULL, lost INTEGER NOT NULL,
  latency INTEGER, lo INTEGER, hi INTEGER,
  PRIMARY KEY (node_id, ts, task_id)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS session (
  token_hash TEXT    PRIMARY KEY,
  expires_at INTEGER NOT NULL
);
"#;

/// Schema revision this build expects, stamped into `PRAGMA user_version`.
/// Increment it and add a `migrate_to_N` when the schema changes under a
/// database already in service. Every migration must be:
///
/// - Additive: a new column carries a default, and no column an earlier build
///   reads is renamed or dropped. install-hub.sh rolls a hub that fails to start
///   back to the previous binary, which then runs on the migrated file.
/// - Safe to run twice: an earlier build stamps its own, lower version into a
///   newer file, and the next upgrade runs the migration again.
///
/// A new column goes into `SCHEMA` as well, for fresh files, but an index on it
/// cannot: `open` runs `SCHEMA` before migrating, and on an older file the
/// column is not there yet. `an_upgraded_release_matches_a_fresh_database`
/// holds every migration to these rules, starting from v1.0.0's schema.
const SCHEMA_VERSION: i64 = 13;

/// Locks `mutex`, and when it is taken, waits with this thread's share of the
/// runtime handed to another.
///
/// A vacuum or a restore holds the writer for as long as the disk takes to
/// rewrite the file: minutes, on a slow one. Requests, browser streams, agent
/// handshakes and the hourly housekeeping reach it from async tasks, and each
/// waiting in place would take a worker thread with it: on one core with a
/// browser stream open, a 13 s vacuum would hold every other request for 12.5 s.
///
/// ponytail: each waiter holds a blocking thread -- during a vacuum or a
/// restore, every open browser stream and connected agent, plus each request
/// arriving meanwhile. Past tokio's default of 512 the next waiter stalls a
/// worker again; refuse waiters past a limit, as the history charts do, if
/// hubs reach that.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(e)) => e.into_inner(),
        Err(TryLockError::WouldBlock) => {
            let wait = || mutex.lock().unwrap_or_else(|e| e.into_inner());
            // `block_in_place` panics on a current-thread runtime, which is
            // what `#[tokio::test]` runs. Off the runtime's workers -- in
            // `spawn_blocking`, or already in `block_in_place` -- it calls the
            // closure as it is.
            match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
                Ok(RuntimeFlavor::MultiThread) => tokio::task::block_in_place(wait),
                _ => wait(),
            }
        }
    }
}

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        // Queried before CREATE TABLE runs: a file with no tables receives the
        // current schema directly rather than the history of how it was reached.
        let fresh = conn
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table'", [], |r| r.get::<_, i64>(0))?
            == 0;
        conn.execute_batch(SCHEMA)?;
        restrict(path);

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        migrate(&conn, if fresh { SCHEMA_VERSION } else { version })?;
        traffic_period::ensure(&conn)?;
        crate::management::ensure(&conn)?;
        // A file from a release that kept no high marks gets them now, from its
        // rows. Otherwise deleting the newest node or probe before the next one
        // is created would lose its id, and `next_id` would issue it again.
        for table in ID_TABLES {
            put_setting(&conn, &format!("{table}_id_issued"), &issued(&conn, table)?.to_string())?;
        }
        let reader = match main_file(&conn) {
            file if file.is_empty() => None,
            file => Some(Mutex::new(read_only(&file)?)),
        };
        Ok(Self { reader, conn: Mutex::new(conn) })
    }

    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        lock(&self.conn)
    }

    /// The connection the history charts read through: the read-only one, or
    /// the writer for `:memory:`.
    ///
    /// A scan holds its snapshot throughout, and with chart requests queued the
    /// next begins as the last ends, so no commit's checkpoint would find the
    /// reader idle and the WAL would keep every commit for as long as anyone
    /// sent them: 20 MB in 20 s at 100 commits a second, from four requests kept
    /// in flight. Before the scan the reader is idle, and a checkpoint taken
    /// here resets the WAL itself. Past 4 MiB only, as without readers the WAL
    /// stays within the 1 MiB `journal_size_limit` plus one transaction.
    ///
    /// Without waiting: an export is the one other reader, and waiting on it
    /// would hold the writer, and every agent report behind it, for the busy
    /// timeout.
    ///
    /// The writer is taken after the reader here and must never be held while
    /// taking the reader.
    fn reader(&self) -> MutexGuard<'_, Connection> {
        let Some(reader) = &self.reader else { return self.conn() };
        let reader = reader.lock().unwrap_or_else(|e| e.into_inner());
        if bytes_of(&format!("{}-wal", reader.path().unwrap_or_default())) > 4 << 20 {
            let conn = self.conn();
            let _ = conn.busy_timeout(std::time::Duration::ZERO);
            let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
            let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        }
        reader
    }

    // ---- settings ----

    pub fn get(&self, key: &str) -> Option<String> {
        self.lookup(key).ok().flatten()
    }

    /// As [`Db::get`], with a failed read kept apart from an absent key, for a
    /// caller that would otherwise act on "nothing saved".
    pub fn lookup(&self, key: &str) -> Result<Option<String>> {
        setting(&self.conn(), key)
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        put_setting(&self.conn(), key, value)
    }

    /// Recheck the caller after body extraction, then commit the complete
    /// settings write and any session rotation under the same writer lock.
    /// None means the caller's session expired or was revoked before the write.
    pub fn save_panel_settings(
        &self,
        caller: &str,
        values: &[(&str, &str)],
        password_hash: Option<&str>,
        replacement: (&str, i64),
    ) -> Result<Option<bool>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if !live_session(&tx, caller)? {
            return Ok(None);
        }

        let mut reissue = password_hash.is_some();
        if let Some(hash) = password_hash {
            put_setting(&tx, "admin_password_hash", hash)?;
        }
        for &(key, value) in values {
            if key == "github_allowed_users" {
                reissue |= put_github_allowed(&tx, value)?;
            } else {
                put_setting(&tx, key, value)?;
            }
        }
        if reissue {
            replace_sessions(&tx, replacement)?;
        }
        tx.commit()?;
        Ok(Some(reissue))
    }

    /// The GitHub account id `login` is bound to, binding it to `id` when it has
    /// none; `None` when the allow list no longer names it. The list is read
    /// again under the writer lock: a save that removed the name during the
    /// OAuth exchange would otherwise be followed by a binding the removal was
    /// meant to clear.
    pub fn bind_github(&self, login: &str, id: &str) -> Result<Option<String>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let list = setting(&tx, "github_allowed_users")?.unwrap_or_default();
        if !crate::auth::allowed_logins(&list).iter().any(|l| l == login) {
            return Ok(None);
        }
        let ids = setting(&tx, crate::auth::GITHUB_IDS)?.unwrap_or_default();
        if let Some(bound) = crate::auth::bound_id(&ids, login) {
            return Ok(Some(bound.to_owned()));
        }
        let pair = format!("{login}:{id}");
        put_setting(
            &tx,
            crate::auth::GITHUB_IDS,
            &if ids.is_empty() { pair } else { format!("{ids},{pair}") },
        )?;
        tx.commit()?;
        Ok(Some(id.to_owned()))
    }

    /// Recheck identity and allowlist under the same lock that inserts the session.
    /// A code exchange finishing after access was removed must not issue new authority.
    pub fn create_github_session(&self, login: &str, id: &str, token_hash: &str, expires: i64) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let login = login.to_lowercase();
        let list = setting(&tx, "github_allowed_users")?.unwrap_or_default();
        let ids = setting(&tx, crate::auth::GITHUB_IDS)?.unwrap_or_default();
        if !crate::auth::allowed_logins(&list).contains(&login)
            || crate::auth::bound_id(&ids, &login) != Some(id)
        {
            refuse!("GitHub 登录权限已变化，请重新登录");
        }
        tx.execute("INSERT INTO session(token_hash,expires_at) VALUES(?1,?2)", params![token_hash, expires])?;
        tx.commit()?;
        Ok(())
    }
}

impl Db {
    // ---- sessions ----

    #[cfg(test)]
    pub fn create_session(&self, token_hash: &str, expires_at: i64) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO session (token_hash, expires_at) VALUES (?1, ?2)",
            params![token_hash, expires_at],
        )?;
        Ok(())
    }

    /// The expensive password verification ran outside the writer lock.
    /// Only the exact hash verified by that request may still authorize a login.
    pub fn create_password_session(&self, verified_hash: &str, session: (&str, i64)) -> Result<bool> {
        Ok(self.conn().execute(
            "INSERT INTO session(token_hash, expires_at)
             SELECT ?1, ?2 WHERE EXISTS(
                 SELECT 1 FROM setting WHERE key='admin_password_hash' AND value=?3
             )",
            params![session.0, session.1, verified_hash],
        )? == 1)
    }

    pub fn session_valid(&self, token_hash: &str) -> bool {
        live_session(&self.conn(), token_hash).unwrap_or(false)
    }

    /// Live sessions, newest first. Expired rows are filtered here rather than
    /// left to `expire_sessions`, which sweeps only once an hour.
    pub fn sessions(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT token_hash, expires_at FROM session WHERE expires_at > ?1 ORDER BY expires_at DESC",
        )?;
        let rows = stmt
            .query_map([Utc::now().timestamp()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn drop_session(&self, token_hash: &str) -> Result<()> {
        self.conn().execute("DELETE FROM session WHERE token_hash=?1", [token_hash])?;
        Ok(())
    }

    /// Replaces the admin password hash and signs every session out, both or
    /// neither: a reset that stored the hash and then failed would report failure
    /// while the old password no longer works.
    pub fn replace_password(&self, hash: &str) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        put_setting(&tx, "admin_password_hash", hash)?;
        tx.execute("DELETE FROM session", [])?;
        tx.commit()?;
        Ok(())
    }

    pub fn expire_sessions(&self) -> Result<()> {
        self.conn().execute("DELETE FROM session WHERE expires_at <= ?1", [Utc::now().timestamp()])?;
        Ok(())
    }
}

/// The id for a new row of `table`: one past every id it has issued, deleted
/// rows included. SQLite's default reissues the highest id once its row is
/// deleted, and state the hub keeps by id -- the offline sweep's absence and
/// return times, an editor opened before the delete -- would pass to the new
/// row. The high mark lives in `setting`, so a backup carries it; without one,
/// as after an upgrade, the table's own highest id stands in.
fn next_id(tx: &rusqlite::Transaction<'_>, table: &str) -> Result<i64> {
    let id = issued(tx, table)? + 1;
    put_setting(tx, &format!("{table}_id_issued"), &id.to_string())?;
    Ok(id)
}

/// The highest id `table` has issued, by its high mark or its rows.
fn issued(conn: &Connection, table: &str) -> Result<i64> {
    Ok(conn.query_row(
        &format!(
            "SELECT MAX(COALESCE((SELECT CAST(value AS INTEGER) FROM setting WHERE key = ?1), 0),
                        COALESCE((SELECT MAX(id) FROM {table}), 0))"
        ),
        [format!("{table}_id_issued")],
        |r| r.get(0),
    )?)
}

/// The tables whose ids [`next_id`] issues.
const ID_TABLES: [&str; 2] = ["node", "ping_task"];

fn setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT value FROM setting WHERE key = ?1", [key], |r| r.get(0)).optional()?)
}

fn put_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO setting (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn live_session(conn: &Connection, token_hash: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM session WHERE token_hash=?1 AND expires_at>?2)",
        params![token_hash, Utc::now().timestamp()],
        |row| row.get(0),
    )?)
}

fn replace_sessions(conn: &Connection, replacement: (&str, i64)) -> Result<()> {
    conn.execute("DELETE FROM session", [])?;
    conn.execute("INSERT INTO session(token_hash, expires_at) VALUES (?1, ?2)", replacement)?;
    Ok(())
}

/// Prune bindings for the removed logins as part of the settings transaction.
fn put_github_allowed(conn: &Connection, list: &str) -> Result<bool> {
    let previous = setting(conn, "github_allowed_users")?.unwrap_or_default();
    let allowed = crate::auth::allowed_logins(list);
    let revoked = crate::auth::allowed_logins(&previous).iter().any(|name| !allowed.contains(name));
    let ids = setting(conn, crate::auth::GITHUB_IDS)?.unwrap_or_default();
    put_setting(conn, "github_allowed_users", list)?;
    put_setting(conn, crate::auth::GITHUB_IDS, &crate::auth::keep_listed(&ids, list))?;
    Ok(revoked)
}
