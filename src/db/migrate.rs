//! Schema migrations, each additive and safe to run twice; see `SCHEMA_VERSION`.

use super::*;

/// Adds a column older databases lack. A duplicate column indicates the
/// migration has already run; every other error must propagate.
fn add_column(conn: &Connection, table: &str, column: &str) -> Result<()> {
    match conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column}"), []) {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("duplicate column name") => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// True when `table`'s stored DDL contains `needle`, which is how a migration
/// determines the shape of the database it inherited.
pub(super) fn schema_mentions(conn: &Connection, table: &str, needle: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name=?1 AND sql LIKE ?2",
        params![table, format!("%{needle}%")],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

/// One table's column names. `table` is always a [`TABLES`] entry rather than
/// caller-supplied, which is why it can be formatted into the pragma.
pub(super) fn columns_of(conn: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    Ok(names.collect::<Result<_, _>>()?)
}

/// Everything accumulated before a version was recorded. Runs once, on a
/// database predating the stamp.
fn migrate_to_1(conn: &Connection) -> Result<()> {
    for column in [
        "day_rx INTEGER NOT NULL DEFAULT 0",
        "day_tx INTEGER NOT NULL DEFAULT 0",
        "day_start TEXT NOT NULL DEFAULT ''",
    ] {
        add_column(conn, "traffic", column)?;
    }
    for column in [
        "ipv4 TEXT NOT NULL DEFAULT ''",
        "ipv6 TEXT NOT NULL DEFAULT ''",
        "last_seen INTEGER NOT NULL DEFAULT 0",
    ] {
        add_column(conn, "node", column)?;
    }
    // The column held a sha256 of the token and now holds the token itself.
    // Databases predating the change retain digests no agent can present, so
    // those nodes require a new token issued from the panel.
    if schema_mentions(conn, "node", "token_hash")? {
        conn.execute("ALTER TABLE node RENAME COLUMN token_hash TO token", [])?;
        info!("renamed node.token_hash to node.token; existing nodes need a fresh token");
    }
    // Reordering a key requires rebuilding the table; CREATE TABLE IF NOT EXISTS
    // leaves an existing one untouched. The old order placed task_id between the
    // node and the timestamp, so the chart query scanned a node's entire history
    // to answer for one hour of it: 42 ms against 0.8 ms at a month of
    // retention.
    if schema_mentions(conn, "ping_record", "(node_id, task_id, ts)")? {
        conn.execute_batch(
            "CREATE TABLE ping_record_rekeyed (
               node_id INTEGER NOT NULL, task_id INTEGER NOT NULL,
               ts INTEGER NOT NULL, latency INTEGER NOT NULL,
               PRIMARY KEY (node_id, ts, task_id)
             ) WITHOUT ROWID;
             INSERT INTO ping_record_rekeyed SELECT * FROM ping_record;
             DROP TABLE ping_record;
             ALTER TABLE ping_record_rekeyed RENAME TO ping_record;",
        )?;
        info!("rebuilt ping_record on a key the latency chart can seek");
    }
    Ok(())
}

/// `metric.load1` was written on every history row and read by nothing: the card
/// draws the live `load` array from the report, and no chart draws load from
/// history. Dropping it recovers 21% of what the five unread columns cost, and
/// it is the only one the hub can lose without also losing a figure the UI
/// displays.
///
/// The column is `NOT NULL` with no default, so this migration is mandatory:
/// without it every metric insert this build makes violates the constraint.
fn migrate_to_2(conn: &Connection) -> Result<()> {
    if schema_mentions(conn, "metric", "load1")? {
        conn.execute("ALTER TABLE metric DROP COLUMN load1", [])?;
        info!("dropped metric.load1; nothing read it");
    }
    Ok(())
}

fn migrate_to_3(conn: &Connection) -> Result<()> {
    add_column(conn, "node", "country TEXT NOT NULL DEFAULT ''")
}

fn migrate_to_4(conn: &Connection) -> Result<()> {
    add_column(conn, "node", "notify INTEGER NOT NULL DEFAULT 0")?;
    add_column(conn, "node", "down_since INTEGER NOT NULL DEFAULT 0")
}

/// Every country stored until now was looked up from `ip`. Recording that
/// keeps the badge of a node whose lookup address is still `ip`, and has a node
/// whose public interface address now takes precedence looked up again at its
/// next hello. The columns set by hand start empty: automatic.
fn migrate_to_5(conn: &Connection) -> Result<()> {
    add_column(conn, "node", "country_ip TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "node", "country_pin TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "node", "ipv4_pin TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "node", "ipv6_pin TEXT NOT NULL DEFAULT ''")?;
    conn.execute("UPDATE node SET country_ip = ip WHERE country != ''", [])?;
    Ok(())
}

fn migrate_to_6(conn: &Connection) -> Result<()> {
    add_column(conn, "ping_task", "auto_join INTEGER NOT NULL DEFAULT 0")
}

fn migrate_to_7(conn: &Connection) -> Result<()> {
    add_column(conn, "node", "group_name TEXT NOT NULL DEFAULT ''")
}

fn migrate_to_8(conn: &Connection) -> Result<()> {
    add_column(conn, "node", "country_prev_ip TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "node", "country_prev TEXT NOT NULL DEFAULT ''")
}

/// Every existing probe ties at 0, so `ORDER BY sort, id` keeps the id order an
/// upgraded database listed them in.
fn migrate_to_9(conn: &Connection) -> Result<()> {
    add_column(conn, "ping_task", "sort INTEGER NOT NULL DEFAULT 0")
}

/// Rows written before the peak existed hold 0, which `Db::metrics` reads as
/// the row's mean rather than rewriting every row of history here.
fn migrate_to_10(conn: &Connection) -> Result<()> {
    add_column(conn, "metric", "net_rx_max INTEGER NOT NULL DEFAULT 0")?;
    add_column(conn, "metric", "net_tx_max INTEGER NOT NULL DEFAULT 0")
}

/// The hourly tier, and the peak CPU the minute rows begin to carry. A file in
/// service already has both tables, since `open` runs `SCHEMA` first; a backup
/// from an earlier release does not, and restoring one would leave every rollup
/// failing until the next restart.
fn migrate_to_11(conn: &Connection) -> Result<()> {
    add_column(conn, "metric", "cpu_max REAL NOT NULL DEFAULT 0")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS metric_hour (
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
         CREATE TABLE IF NOT EXISTS ping_hour (
           node_id INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
           task_id INTEGER NOT NULL REFERENCES ping_task(id) ON DELETE CASCADE,
           ts INTEGER NOT NULL,
           answered INTEGER NOT NULL, lost INTEGER NOT NULL,
           latency INTEGER, lo INTEGER, hi INTEGER,
           PRIMARY KEY (node_id, ts, task_id)
         ) WITHOUT ROWID;",
    )?;
    Ok(())
}

fn migrate_to_12(conn: &Connection) -> Result<()> {
    add_column(conn, "node", "public_remark TEXT NOT NULL DEFAULT ''")
}

fn migrate_to_13(conn: &Connection) -> Result<()> {
    add_column(conn, "node", "ip_other TEXT NOT NULL DEFAULT ''")
}

/// Brings a database already in service up to `SCHEMA_VERSION` and stamps it.
/// `from` is its current version, so a fresh file passes `SCHEMA_VERSION` and
/// receives only the stamp.
///
/// One transaction covers every step and the stamp. SQLite rolls back schema
/// changes and `user_version` alike, so a failure part-way -- a full disk, a
/// killed process -- leaves the file at the version it started from rather than
/// between two.
///
/// Restoring a backup also arrives here: the copy carries its own version and
/// requires the same migrations a restart would have run.
pub(super) fn migrate(conn: &Connection, from: i64) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    if from < 1 {
        migrate_to_1(&tx)?;
    }
    if from < 2 {
        migrate_to_2(&tx)?;
    }
    if from < 3 {
        migrate_to_3(&tx)?;
    }
    if from < 4 {
        migrate_to_4(&tx)?;
    }
    if from < 5 {
        migrate_to_5(&tx)?;
    }
    if from < 6 {
        migrate_to_6(&tx)?;
    }
    if from < 7 {
        migrate_to_7(&tx)?;
    }
    if from < 8 {
        migrate_to_8(&tx)?;
    }
    if from < 9 {
        migrate_to_9(&tx)?;
    }
    if from < 10 {
        migrate_to_10(&tx)?;
    }
    if from < 11 {
        migrate_to_11(&tx)?;
    }
    if from < 12 {
        migrate_to_12(&tx)?;
    }
    if from < 13 {
        migrate_to_13(&tx)?;
    }
    tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
    tx.commit()?;
    Ok(())
}

/// Every table this build keeps.
pub(super) const TABLES: [&str; 10] = [
    "setting",
    "node",
    "traffic",
    "metric",
    "ping_task",
    "ping_node",
    "ping_record",
    "session",
    "metric_hour",
    "ping_hour",
];

/// The tables `migrate_to_11` adds. A backup taken before it lacks them and is
/// still a backup this build restores.
pub(super) const HOURLY_TABLES: [&str; 2] = ["metric_hour", "ping_hour"];
