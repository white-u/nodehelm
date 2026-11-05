use super::*;
use chrono::{Days, TimeZone};

fn setup() -> (Db, i64) {
    let db = Db::open(":memory:").unwrap();
    let node = db
        .create_node(
            &Node { name: "period".into(), traffic_reset_day: 1, ..Default::default() },
            "period-token",
        )
        .unwrap();
    (db, node)
}

fn on(day: NaiveDate, hour: u32, minute: u32, second: u32) -> DateTime<Local> {
    Local.with_ymd_and_hms(day.year(), day.month(), day.day(), hour, minute, second).single().unwrap()
}

fn report(db: &Db, node: i64, days: i64) -> serde_json::Value {
    serde_json::to_value(db.traffic_periods(node, days, Local::now()).unwrap()).unwrap()
}

#[test]
fn four_periods_follow_sampling_boundaries_and_sum_recorded_bytes() {
    let (db, node) = setup();
    let day = Local::now().date_naive() - Days::new(1);
    db.accumulate(node, "boot", (100, 100), on(day, 0, 0, 0), None).unwrap();
    let samples = [
        (0, 0, 1, 0),
        (7, 59, 59, 0),
        (8, 0, 0, 1),
        (13, 59, 59, 1),
        (14, 0, 0, 2),
        (19, 59, 59, 2),
        (20, 0, 0, 3),
        (23, 59, 59, 3),
    ];
    for (index, (h, m, s, slot)) in samples.into_iter().enumerate() {
        db.accumulate(
            node,
            "boot",
            (100 + (index as i64 + 1) * 100, 100 + (index as i64 + 1) * 200),
            on(day, h, m, s),
            None,
        )
        .unwrap();
        let shown = report(&db, node, 2);
        assert!(!shown["rows"][1]["periods"][slot].is_null());
    }
    let shown = report(&db, node, 2);
    for slot in 0..4 {
        assert_eq!(shown["rows"][1]["periods"][slot]["rx"], 200.0);
        assert_eq!(shown["rows"][1]["periods"][slot]["tx"], 400.0);
    }
    assert_eq!(shown["summary"]["total"], 2400.0);
    assert!(shown["rows"][0]["total"].is_null());
}

#[test]
fn delayed_old_reading_does_not_become_todays_future_period() {
    let (db, node) = setup();
    let today = Local::now().date_naive();
    let yesterday = today - Days::new(1);
    db.accumulate(node, "boot", (0, 0), on(yesterday, 23, 50, 0), None).unwrap();
    db.accumulate(node, "boot", (100, 100), on(today, 0, 10, 0), None).unwrap();
    db.accumulate(node, "boot", (200, 200), on(yesterday, 23, 59, 59), None).unwrap();
    let shown = report(&db, node, 2);
    assert_eq!(shown["rows"][0]["periods"][0]["rx"], 100.0);
    assert!(shown["rows"][0]["periods"][3].is_null());
    assert_eq!(shown["rows"][1]["periods"][3]["rx"], 100.0);
    assert_eq!(db.stored_traffic(node).day_rx, 200, "the upstream mutable day rule stays intact");
}

#[test]
fn invalid_period_slots_are_rejected_without_indexing_the_periods() {
    let (db, node) = setup();
    let now = Local::now();
    db.accumulate(node, "boot", (0, 0), now, None).unwrap();
    db.accumulate(node, "boot", (100, 200), now, None).unwrap();
    db.conn().execute_batch("PRAGMA ignore_check_constraints=ON").unwrap();
    for slot in [-1, 4, i64::MAX] {
        db.conn().execute("UPDATE fork_traffic_period SET slot=?1", [slot]).unwrap();
        assert!(db.traffic_periods(node, 1, now).is_err(), "invalid slot {slot}");
    }
}

#[test]
fn baseline_is_missing_history_but_observed_zero_is_a_record() {
    let (db, node) = setup();
    let at = Local::now();
    db.accumulate(node, "first", (5000, 7000), at, None).unwrap();
    assert!(report(&db, node, 1)["summary"].is_null());
    db.accumulate(node, "first", (5000, 7000), at, None).unwrap();
    assert_eq!(report(&db, node, 1)["summary"]["total"], 0.0);
    db.accumulate(node, "second", (800, 900), at, None).unwrap();
    db.accumulate(node, "second", (1000, 1000), at, None).unwrap();
    db.accumulate(node, "second", (1000, 1000), at, None).unwrap();
    db.accumulate(node, "second", (10, 20), at, None).unwrap();
    db.accumulate(node, "second", (20, 30), at, None).unwrap();
    assert_eq!(report(&db, node, 1)["summary"]["total"], 320.0);
}

#[test]
fn unseen_reboot_books_catch_up_bytes_into_its_arrival_period_once() {
    let (db, node) = setup();
    let day = Local::now().date_naive() - Days::new(1);
    let seen = on(day, 7, 59, 0);
    db.accumulate(node, "old/iface", (1000, 2000), seen, None).unwrap();
    db.touch_seen(node, seen.timestamp(), &serde_json::Value::Null).unwrap();
    let booted = Some(on(day, 8, 0, 0).timestamp());
    let arrived = on(day, 8, 10, 0);
    let totals = db.accumulate(node, "new/iface", (300, 500), arrived, booted).unwrap();
    assert_eq!((totals.total_rx, totals.total_tx), (300, 500));
    let shown = report(&db, node, 2);
    assert!(shown["rows"][1]["periods"][0].is_null());
    assert_eq!(shown["rows"][1]["periods"][1]["rx"], 300.0);
    assert_eq!(shown["rows"][1]["periods"][1]["tx"], 500.0);
    db.accumulate(node, "new/iface", (400, 700), on(day, 8, 11, 0), booted).unwrap();
    db.accumulate(node, "new/iface", (400, 700), on(day, 8, 12, 0), booted).unwrap();
    let shown = report(&db, node, 2);
    assert_eq!(shown["summary"]["rx"], 400.0);
    assert_eq!(shown["summary"]["tx"], 700.0);
    let totals = db.stored_traffic(node);
    assert_eq!((totals.total_rx, totals.total_tx), (400, 700));
}

#[test]
fn an_unproven_reboot_or_interface_change_does_not_invent_period_history() {
    let day = Local::now().date_naive() - Days::new(1);
    let seen = on(day, 7, 59, 0);
    for (epoch, booted) in [
        ("new/iface", None),
        ("new/iface", Some(on(day, 7, 0, 0).timestamp())),
        ("old/changed", Some(on(day, 8, 0, 0).timestamp())),
    ] {
        let (db, node) = setup();
        db.accumulate(node, "old/iface", (1000, 2000), seen, None).unwrap();
        db.touch_seen(node, seen.timestamp(), &serde_json::Value::Null).unwrap();
        db.accumulate(node, epoch, (300, 500), on(day, 8, 10, 0), booted).unwrap();
        assert!(report(&db, node, 2)["summary"].is_null(), "{epoch}, {booted:?}");
        assert_eq!(db.stored_traffic(node).total_rx, 0);
    }
}

#[test]
fn counter_realigning_does_not_invent_zero_in_a_new_period() {
    let (db, node) = setup();
    let day = Local::now().date_naive() - Days::new(1);
    db.accumulate(node, "boot", (0, 0), on(day, 7, 50, 0), None).unwrap();
    db.accumulate(node, "boot", (100, 200), on(day, 7, 59, 0), None).unwrap();
    db.accumulate(node, "boot", (10, 20), on(day, 8, 0, 0), None).unwrap();
    assert!(report(&db, node, 2)["rows"][1]["periods"][1].is_null());
    db.accumulate(node, "boot", (10, 20), on(day, 8, 1, 0), None).unwrap();
    assert_eq!(report(&db, node, 2)["rows"][1]["periods"][1]["total"], 0.0);
    // The upload is observed even when the download counter re-aligns.
    db.accumulate(node, "boot", (5, 30), on(day, 14, 0, 0), None).unwrap();
    assert_eq!(report(&db, node, 2)["rows"][1]["periods"][2]["tx"], 10.0);
}

#[test]
fn zero_day_prune_clears_period_history_without_resetting_totals() {
    let (db, node) = setup();
    let at = Local::now();
    db.accumulate(node, "boot", (0, 0), at, None).unwrap();
    db.accumulate(node, "boot", (100, 200), at, None).unwrap();
    db.prune(0).unwrap();
    assert!(report(&db, node, 1)["summary"].is_null());
    assert_eq!(db.stored_traffic(node).total_rx, 100);
}

#[test]
fn period_failure_rolls_back_baseline_and_retry_books_once() {
    let (db, node) = setup();
    let at = Local::now();
    db.accumulate(node, "boot", (100, 100), at, None).unwrap();
    db.accumulate(node, "boot", (200, 200), at, None).unwrap();
    db.conn()
        .execute_batch(
            "CREATE TRIGGER fail_period BEFORE UPDATE ON fork_traffic_period
        BEGIN SELECT RAISE(ABORT,'simulated full disk'); END;",
        )
        .unwrap();
    assert!(db.accumulate(node, "boot", (300, 400), at, None).is_err());
    assert_eq!(db.stored_traffic(node).total_rx, 100);
    assert_eq!(db.conn().query_row("SELECT last_rx FROM traffic", [], |r| r.get::<_, i64>(0)).unwrap(), 200);
    db.conn().execute_batch("DROP TRIGGER fail_period").unwrap();
    db.accumulate(node, "boot", (300, 400), at, None).unwrap();
    db.accumulate(node, "boot", (300, 400), at, None).unwrap();
    assert_eq!(report(&db, node, 1)["summary"]["total"], 500.0);
    assert_eq!(db.stored_traffic(node).total_rx, 200);
}

#[test]
fn retention_uses_calendar_days_and_ignores_expired_late_samples() {
    let (db, node) = setup();
    let today = Local::now().date_naive();
    db.set("retention_days", "3").unwrap();
    let old = on(today - Days::new(3), 12, 0, 0);
    db.accumulate(node, "boot", (0, 0), old, None).unwrap();
    db.accumulate(node, "boot", (100, 100), old, None).unwrap();
    assert!(report(&db, node, 365)["summary"].is_null());
    db.accumulate(node, "boot", (200, 200), on(today - Days::new(2), 12, 0, 0), None).unwrap();
    let shown = report(&db, node, 365);
    assert_eq!(shown["days"], 3);
    assert_eq!(shown["since"], (today - Days::new(2)).to_string());
    assert_eq!(shown["summary"]["total"], 200.0);
    db.set("retention_days", "1").unwrap();
    db.prune(1).unwrap();
    assert_eq!(db.stats().unwrap()["rows"][traffic_period::TABLE], 0);
    db.accumulate(node, "boot", (300, 300), old, None).unwrap();
    assert!(report(&db, node, 1)["summary"].is_null());
    assert_eq!(db.stored_traffic(node).total_rx, 300);
}

#[test]
fn deleting_a_node_clears_history_and_a_new_node_gets_a_fresh_id() {
    let (db, node) = setup();
    let at = Local::now();
    db.accumulate(node, "boot", (0, 0), at, None).unwrap();
    db.accumulate(node, "boot", (100, 200), at, None).unwrap();
    db.delete_node(node).unwrap();
    let next = db.create_node(&Node { name: "replacement".into(), ..Default::default() }, "next").unwrap();
    assert!(next > node, "upstream no longer reuses deleted node ids");
    assert_eq!(db.stats().unwrap()["rows"][traffic_period::TABLE], 0);
    assert!(report(&db, next, 1)["summary"].is_null());
}

#[test]
fn bucket_saturation_and_wide_summary_stay_nonnegative() {
    let (db, node) = setup();
    let at = Local::now();
    db.accumulate(node, "boot", (0, 0), at, None).unwrap();
    db.accumulate(node, "boot", (1, 1), at, None).unwrap();
    db.conn().execute("UPDATE fork_traffic_period SET rx=?1,tx=?1", [i64::MAX - 1]).unwrap();
    db.accumulate(node, "boot", (10, 10), at, None).unwrap();
    assert_eq!(
        db.conn().query_row("SELECT rx FROM fork_traffic_period", [], |r| r.get::<_, i64>(0)).unwrap(),
        i64::MAX
    );
    for slot in 0..4 {
        db.conn()
            .execute(
                "INSERT INTO fork_traffic_period VALUES(?1,?2,?3,?4,?4)
            ON CONFLICT(node_id,day,slot) DO UPDATE SET rx=excluded.rx,tx=excluded.tx",
                params![node, at.date_naive().to_string(), slot, i64::MAX],
            )
            .unwrap();
    }
    assert_eq!(report(&db, node, 1)["summary"]["total"].as_f64().unwrap(), (i64::MAX as u128 * 8) as f64);
}

struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("monitor-period-{}", crate::auth::random_token()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn database(&self) -> String {
        self.0.join("hub.db").to_string_lossy().into_owned()
    }
    fn copy(&self) -> String {
        self.0.join("backup.db").to_string_lossy().into_owned()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn fork_table_survives_restart_and_old_or_new_backup_restores() {
    let scratch = Scratch::new();
    let at = Local::now();
    let node = {
        let db = Db::open(&scratch.database()).unwrap();
        let node = db.create_node(&Node { name: "n".into(), ..Default::default() }, "token").unwrap();
        db.accumulate(node, "boot", (0, 0), at, None).unwrap();
        db.accumulate(node, "boot", (100, 200), at, None).unwrap();
        node
    };
    let db = Db::open(&scratch.database()).unwrap();
    db.accumulate(node, "boot", (100, 200), at, None).unwrap();
    assert_eq!(report(&db, node, 1)["summary"]["total"], 300.0);
    db.backup_into(&scratch.copy()).unwrap();
    db.accumulate(node, "boot", (300, 400), at, None).unwrap();
    db.check_backup(&scratch.copy()).unwrap();
    db.restore_from(&scratch.copy()).unwrap();
    assert_eq!(report(&db, node, 1)["summary"]["total"], 300.0);
    // An upstream backup has no extension. Creating it must not invent periods
    // from its existing day or lifetime figures, nor change user_version.
    let copy = Connection::open(scratch.copy()).unwrap();
    copy.execute_batch("DROP TABLE fork_traffic_period").unwrap();
    drop(copy);
    db.check_backup(&scratch.copy()).unwrap();
    db.restore_from(&scratch.copy()).unwrap();
    assert!(report(&db, node, 1)["summary"].is_null());
    assert_eq!(
        db.conn().query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(),
        SCHEMA_VERSION
    );
    traffic_period::ensure(&db.conn()).unwrap();
    assert!(report(&db, node, 1)["summary"].is_null());
}

#[test]
fn backup_refuses_invalid_fork_shapes_and_values_before_live_copy() {
    let scratch = Scratch::new();
    let db = Db::open(&scratch.database()).unwrap();
    let node = db.create_node(&Node { name: "n".into(), ..Default::default() }, "token").unwrap();
    let at = Local::now();
    db.accumulate(node, "boot", (0, 0), at, None).unwrap();
    db.accumulate(node, "boot", (100, 200), at, None).unwrap();
    for bad in [
        "UPDATE fork_traffic_period SET rx='text'",
        "UPDATE fork_traffic_period SET slot=4",
        "UPDATE fork_traffic_period SET day='2026-02-30'",
        "UPDATE fork_traffic_period SET node_id=9999",
        "UPDATE fork_traffic_period SET rx=-1",
        "ALTER TABLE fork_traffic_period RENAME COLUMN rx TO missing",
        "DROP TABLE fork_traffic_period; CREATE TABLE fork_traffic_period(node_id INTEGER NOT NULL,day TEXT NOT NULL,slot INTEGER NOT NULL,rx INTEGER NOT NULL,tx INTEGER NOT NULL,PRIMARY KEY(node_id,day,slot)) WITHOUT ROWID",
    ] {
        let _=std::fs::remove_file(scratch.copy());
        db.backup_into(&scratch.copy()).unwrap();
        let copy=Connection::open(scratch.copy()).unwrap();
        copy.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON").unwrap();
        copy.execute_batch(bad).unwrap(); drop(copy);
        let error=db.check_backup(&scratch.copy()).unwrap_err();
        assert!(error.downcast_ref::<crate::Shown>().is_some(),"{bad}: {error:#}");
        assert_eq!(report(&db,node,1)["summary"]["total"],300.0);
    }
}
