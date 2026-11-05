//! The fork's four-period history. It shares the existing counter deltas, not
//! the mutable day/month totals. No upstream migration number is consumed.

use anyhow::Result;
use chrono::{DateTime, Days, Local, NaiveDate, Timelike};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

pub const TABLE: &str = "fork_traffic_period";
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS fork_traffic_period (
    node_id INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
    day TEXT NOT NULL,
    slot INTEGER NOT NULL CHECK (typeof(slot)='integer' AND slot BETWEEN 0 AND 3),
    rx INTEGER NOT NULL CHECK (typeof(rx)='integer' AND rx>=0),
    tx INTEGER NOT NULL CHECK (typeof(tx)='integer' AND tx>=0),
    PRIMARY KEY (node_id, day, slot)
) WITHOUT ROWID;";

pub fn slot(hour: u32) -> usize {
    match hour {
        0..=7 => 0,
        8..=13 => 1,
        14..=19 => 2,
        _ => 3,
    }
}

pub fn since(today: NaiveDate, days: i64) -> NaiveDate {
    // prune(0) is also used internally to clear all retained history.
    today - chrono::Duration::days(days - 1)
}

/// Called after upstream migrations, both on startup and on the candidate
/// backup. Missing means an old upstream backup, never four inferred periods.
pub fn ensure(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    let mut statement = conn.prepare("PRAGMA table_info(fork_traffic_period)")?;
    let columns = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?.to_ascii_uppercase(),
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let expected: Vec<_> = [
        ("node_id", "INTEGER", 1),
        ("day", "TEXT", 2),
        ("slot", "INTEGER", 3),
        ("rx", "INTEGER", 0),
        ("tx", "INTEGER", 0),
    ]
    .into_iter()
    .map(|(name, ty, key)| (name.to_owned(), ty.to_owned(), 1, key))
    .collect();
    if columns != expected {
        refuse!("流量时段表结构不兼容，无法使用这份数据库");
    }
    let mut statement = conn.prepare("PRAGMA foreign_key_list(fork_traffic_period)")?;
    let foreign = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if foreign != [("node".into(), "node_id".into(), "id".into(), "CASCADE".into())] {
        refuse!("流量时段表缺少节点级联关系，无法使用这份数据库");
    }
    // SQLite's INTEGER affinity and integrity_check alone accept text values.
    let invalid: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM fork_traffic_period p
         WHERE typeof(node_id)!='integer' OR typeof(day)!='text'
            OR typeof(slot)!='integer' OR slot NOT BETWEEN 0 AND 3
            OR typeof(rx)!='integer' OR rx<0 OR typeof(tx)!='integer' OR tx<0
            OR NOT EXISTS(SELECT 1 FROM node n WHERE n.id=p.node_id))",
        [],
        |r| r.get(0),
    )?;
    if invalid {
        refuse!("流量时段表含有无效记录，无法使用这份数据库");
    }
    let mut statement = conn.prepare("SELECT DISTINCT day FROM fork_traffic_period")?;
    for day in statement.query_map([], |r| r.get::<_, String>(0))? {
        let day = day?;
        if !day.parse::<NaiveDate>().is_ok_and(|date| date.to_string() == day) {
            refuse!("流量时段表含有无效日期，无法使用这份数据库");
        }
    }
    Ok(())
}

/// The caller owns the transaction containing the counter baseline update.
pub fn record(conn: &Connection, node: i64, at: DateTime<Local>, delta: (i64, i64)) -> Result<()> {
    let day = at.date_naive().to_string();
    let slot = slot(at.hour()) as i64;
    let old: (i64, i64) = conn
        .prepare_cached("SELECT rx,tx FROM fork_traffic_period WHERE node_id=?1 AND day=?2 AND slot=?3")?
        .query_row(params![node, day, slot], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
        .unwrap_or((0, 0));
    conn.prepare_cached(
        "INSERT INTO fork_traffic_period(node_id,day,slot,rx,tx) VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(node_id,day,slot) DO UPDATE SET rx=excluded.rx,tx=excluded.tx",
    )?
    .execute(params![node, day, slot, old.0.saturating_add(delta.0), old.1.saturating_add(delta.1)])?;
    Ok(())
}

#[derive(Clone, Copy, Serialize)]
pub struct Bytes {
    rx: f64,
    tx: f64,
    total: f64,
}

impl Bytes {
    fn new(rx: u128, tx: u128) -> Self {
        Self { rx: rx as f64, tx: tx as f64, total: (rx + tx) as f64 }
    }
}

#[derive(Serialize)]
pub struct Day {
    day: String,
    periods: [Option<Bytes>; 4],
    total: Option<Bytes>,
}

#[derive(Serialize)]
pub struct Report {
    today: String,
    current_slot: usize,
    utc_offset_seconds: i32,
    retention_days: i64,
    days: i64,
    since: String,
    until: String,
    rows: Vec<Day>,
    summary: Option<Bytes>,
}

pub fn read(conn: &Connection, node: i64, days: i64, retention: i64, now: DateTime<Local>) -> Result<Report> {
    let days = days.min(retention);
    let today = now.date_naive();
    let since = since(today, days).to_string();
    let mut rows: Vec<_> = (0..days)
        .map(|i| Day { day: (today - Days::new(i as u64)).to_string(), periods: [None; 4], total: None })
        .collect();
    let mut totals = vec![(0u128, 0u128); days as usize];
    let mut summary = (0u128, 0u128);
    let mut statement = conn.prepare_cached(
        "SELECT day,slot,rx,tx FROM fork_traffic_period
         WHERE node_id=?1 AND day>=?2 AND day<=?3 ORDER BY day,slot",
    )?;
    let records = statement.query_map(params![node, since, today.to_string()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
    })?;
    let mut recorded = false;
    for record in records {
        let (day, slot, rx, tx) = record?;
        let index = (today - day.parse::<NaiveDate>()?).num_days() as usize;
        anyhow::ensure!(
            index < rows.len() && (0..4).contains(&slot) && rx >= 0 && tx >= 0,
            "invalid traffic period"
        );
        rows[index].periods[slot as usize] = Some(Bytes::new(rx as u128, tx as u128));
        totals[index].0 += rx as u128;
        totals[index].1 += tx as u128;
        summary.0 += rx as u128;
        summary.1 += tx as u128;
        recorded = true;
    }
    for (row, (rx, tx)) in rows.iter_mut().zip(totals) {
        if row.periods.iter().any(Option::is_some) {
            row.total = Some(Bytes::new(rx, tx));
        }
    }
    Ok(Report {
        today: today.to_string(),
        current_slot: slot(now.hour()),
        utc_offset_seconds: now.offset().local_minus_utc(),
        retention_days: retention,
        days,
        since,
        until: today.to_string(),
        rows,
        summary: recorded.then(|| Bytes::new(summary.0, summary.1)),
    })
}

pub fn prune(conn: &Connection, node: i64, since: NaiveDate) -> Result<usize> {
    Ok(conn
        .prepare_cached("DELETE FROM fork_traffic_period WHERE node_id=?1 AND day<?2")?
        .execute(params![node, since.to_string()])?)
}
