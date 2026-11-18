//! PM on the hub: hourly traffic per port for the instance chart, and the events
//! a report raises. Traffic is kept for a week and is the difference between two
//! consecutive reports, so the node sends nothing new.

use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::notify;

const KEEP: i64 = 7 * 86_400;
/// Beside a stored report's usage: the readings it could not give, for the next
/// report to be compared with. Never sent to the page.
pub(super) const LAST_USAGE: &str = "last_usage";

pub(super) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS managed_port_traffic (
  node INTEGER NOT NULL REFERENCES managed_node(node) ON DELETE CASCADE,
  port INTEGER NOT NULL, hour INTEGER NOT NULL, rx INTEGER NOT NULL, tx INTEGER NOT NULL,
  PRIMARY KEY (node, port, hour)) WITHOUT ROWID;";

/// The stored report as the next one is compared with it: readings it lacked are
/// those of the last report that had them.
pub(super) fn recalled(mut old: Value) -> Value {
    let Some(pm) = old.get_mut("modules").and_then(|m| m.get_mut("pm")).and_then(Value::as_object_mut) else {
        return old;
    };
    let Some(Value::Object(last)) = pm.remove(LAST_USAGE) else { return old };
    if let Some(usage) = pm.entry("usage").or_insert_with(|| json!({})).as_object_mut() {
        usage.extend(last);
    }
    old
}

/// The report to store: `new`, keeping from the recalled `old` the reading of each
/// rule it has none for. A failed read then neither repeats an alert nor loses the
/// traffic it spans.
pub(super) fn remember(old: &Value, new: &Value) -> Value {
    let mut last = serde_json::Map::new();
    for rule in new["modules"]["pm"]["resources"].as_array().into_iter().flatten() {
        let Some(port) = rule["port"].as_u64().map(|port| port.to_string()) else { continue };
        let (was, now) = (&old["modules"]["pm"]["usage"][&port], &new["modules"]["pm"]["usage"][&port]);
        if now.is_null() && !was.is_null() {
            last.insert(port, was.clone());
        }
    }
    let mut stored = new.clone();
    if !last.is_empty() {
        stored["modules"]["pm"][LAST_USAGE] = Value::Object(last);
    }
    stored
}

/// What each port carried since the previous report, added to the current hour.
/// Only a port in both reports, in the same billing period, with neither counter
/// going back: a reset, a restored checkpoint or a missing reading then costs
/// those few seconds instead of counting a whole period again.
pub(super) fn record(c: &Connection, node: i64, old: &Value, new: &Value, now: i64) -> Result<()> {
    let (Some(before), Some(after)) =
        (old["modules"]["pm"]["usage"].as_object(), new["modules"]["pm"]["usage"].as_object())
    else {
        return Ok(());
    };
    let mut statement = c.prepare_cached(
        "INSERT INTO managed_port_traffic(node,port,hour,rx,tx) VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(node,port,hour) DO UPDATE SET rx=min(rx+excluded.rx,9223372036854775807),
         tx=min(tx+excluded.tx,9223372036854775807)",
    )?;
    for (port, state) in after {
        let (Some(was), Ok(port)) = (before.get(port), port.parse::<i64>()) else { continue };
        if was["period_start"] != state["period_start"] {
            continue;
        }
        let delta = |key: &str| Some(state[key].as_i64()? - was[key].as_i64()?).filter(|d| *d >= 0);
        let (Some(rx), Some(tx)) = (delta("rx"), delta("tx")) else { continue };
        if rx + tx > 0 {
            statement.execute(params![node, port, now - now.rem_euclid(3_600), rx, tx])?;
        }
    }
    Ok(())
}

/// A deleted rule's history, so a new instance on its port starts empty.
pub(super) fn forget(c: &Connection, node: i64, port: i64) -> Result<()> {
    c.execute("DELETE FROM managed_port_traffic WHERE node=?1 AND port=?2", params![node, port])?;
    Ok(())
}

/// The kept week of one port, oldest first, as `[hour, rx, tx]`.
pub(super) fn read(c: &Connection, node: i64, port: i64, now: i64) -> Result<Value> {
    let mut statement = c.prepare_cached(
        "SELECT hour,rx,tx FROM managed_port_traffic WHERE node=?1 AND port=?2 AND hour>?3 ORDER BY hour",
    )?;
    let rows = statement
        .query_map(params![node, port, now - KEEP], |r| {
            Ok(json!([r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?]))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({ "hours": rows }))
}

pub fn prune(c: &Connection, now: i64) -> Result<usize> {
    Ok(c.execute("DELETE FROM managed_port_traffic WHERE hour<=?1", [now - KEEP])?)
}

/// PM's part of a report's events: expiry, quota steps and IP alerts per port.
///
/// Usage is compared against `thresholds` (ascending percents ending at 100) in
/// both reports, so each step sounds once per billing period; the 100 % step is
/// the quota-reached message.
pub(super) fn events(old: &Value, new: &Value, thresholds: &[u64]) -> (Vec<String>, Vec<notify::IpAlert>) {
    let (mut events, mut ips) = (Vec::new(), Vec::new());
    let Some(usage) = new["modules"]["pm"]["usage"].as_object() else { return (events, ips) };
    for (port, state) in usage {
        let (rule, before) = (rule_of(new, port), &old["modules"]["pm"]["usage"][port.as_str()]);
        let alert = state["alert"].as_str().unwrap_or_default();
        let what = match alert {
            "expired" => Some("已到期，匹配流量已被阻断"),
            _ => None,
        };
        if let Some(what) = what.filter(|_| before["alert"].as_str() != Some(alert)) {
            events.push(format!("端口 {port}：{what}"));
        }
        let step = threshold(state, &rule, thresholds);
        let was = if before["period_start"] == state["period_start"] {
            threshold(before, &rule_of(old, port), thresholds)
        } else {
            0
        };
        if step > was {
            events.push(if step >= 100 {
                format!("端口 {port}：已达配额，匹配流量已被丢弃")
            } else {
                format!("端口 {port}：用量已达配额 {step}%")
            });
        }
        let level = state["ip_level"].as_u64().unwrap_or(0);
        if level > 0 && state["ip_alert"] != before["ip_alert"] {
            let limit = &rule["ip_limit"];
            let max = limit["max_ips"].as_u64().unwrap_or(0);
            ips.push(notify::IpAlert {
                port: port.clone(),
                name: rule["name"].as_str().unwrap_or_default().to_owned(),
                count: state["ip_count"].as_u64().unwrap_or(0),
                max,
                alarm: limit["alarm_ips"].as_u64().unwrap_or(max + 2),
                block_at: limit["block_ips"].as_u64().unwrap_or(max + 3),
                block: limit["block"] == true,
                level,
                ips: state["ip_list"]
                    .as_array()
                    .map(|l| l.iter().filter_map(|ip| ip.as_str().map(str::to_owned)).collect())
                    .unwrap_or_default(),
            });
        }
    }
    (events, ips)
}
fn rule_of(snapshot: &Value, port: &str) -> Value {
    snapshot["modules"]["pm"]["resources"]
        .as_array()
        .and_then(|rules| rules.iter().find(|r| r["port"].as_u64() == port.parse().ok()))
        .cloned()
        .unwrap_or(Value::Null)
}
/// The highest threshold a report's usage has reached, 0 for none. Counting
/// follows the rule's billing direction; a quota the node reports as reached is
/// 100 % even if the counters lag the nftables quota.
fn threshold(usage: &Value, rule: &Value, thresholds: &[u64]) -> u64 {
    let quota = rule["quota_gb"].as_u64().unwrap_or(0) as u128 * (1 << 30);
    if quota == 0 || !matches!(usage["alert"].as_str(), Some("normal" | "quota_warning" | "quota_reached")) {
        return 0;
    }
    if usage["alert"] == "quota_reached" {
        return 100;
    }
    let tx = usage["tx"].as_u64().unwrap_or(0) as u128;
    let used =
        if rule["quota_mode"] == "out_only" { tx } else { tx + usage["rx"].as_u64().unwrap_or(0) as u128 };
    thresholds.iter().copied().filter(|&t| used * 100 >= quota * t as u128).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "PRAGMA foreign_keys=ON; CREATE TABLE managed_node(node INTEGER PRIMARY KEY);
             INSERT INTO managed_node VALUES(1);",
        )
        .unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c
    }

    fn report(ports: &[(&str, i64, i64, &str)]) -> Value {
        let usage: serde_json::Map<String, Value> = ports
            .iter()
            .map(|(port, rx, tx, period)| {
                ((*port).to_owned(), json!({"rx":rx,"tx":tx,"period_start":period}))
            })
            .collect();
        json!({"modules":{"pm":{"usage":usage}}})
    }

    fn hours(c: &Connection, port: i64) -> Value {
        read(c, 1, port, 10 * 3_600).unwrap().get("hours").cloned().unwrap()
    }

    #[test]
    fn only_comparable_growth_is_counted() {
        let c = conn();
        let at = 9 * 3_600 + 5;
        let start = report(&[("1000", 100, 50, "2028-01-01"), ("2000", 10, 10, "")]);
        // Growth in the same period is added to the current hour, twice over.
        record(&c, 1, &start, &report(&[("1000", 160, 70, "2028-01-01")]), at).unwrap();
        record(
            &c,
            1,
            &report(&[("1000", 160, 70, "2028-01-01")]),
            &report(&[("1000", 170, 70, "2028-01-01")]),
            at,
        )
        .unwrap();
        assert_eq!(hours(&c, 1000), json!([[9 * 3_600, 70, 20]]));
        for (old, new) in [
            // A reset or a restored checkpoint going back is not a whole period again.
            (start.clone(), report(&[("1000", 5, 60, "2028-01-01")])),
            // A new period starts its counters over.
            (start.clone(), report(&[("1000", 500, 500, "2028-02-01")])),
            // A missing reading on either side, or a first report.
            (report(&[]), report(&[("2000", 999, 999, "")])),
            (json!({}), start.clone()),
        ] {
            record(&c, 1, &old, &new, at).unwrap();
        }
        assert_eq!(hours(&c, 1000), json!([[9 * 3_600, 70, 20]]));
        assert_eq!(hours(&c, 2000), json!([]));
    }

    /// A rule with a limit and the reading of its port, or none for a failed read.
    fn reported(reading: Option<Value>) -> Value {
        let usage = reading.map_or_else(|| json!({}), |reading| json!({ "1000": reading }));
        json!({"modules":{"pm":{"usage":usage,"resources":[{"port":1000,"name":"PM","quota_gb":1,
            "quota_mode":"in_out","ip_limit":{"max_ips":1,"cooldown_min":30,"whitelist":[]}}]}}})
    }

    /// Passes reports through what `poll` stores, returning the alerts of each.
    fn alerts(c: &Connection, reports: &[Value]) -> Vec<usize> {
        let mut stored = reports[0].clone();
        reports[1..]
            .iter()
            .map(|new| {
                let old = recalled(stored.clone());
                record(c, 1, &old, new, 9 * 3_600).unwrap();
                stored = remember(&old, new);
                let (notes, ips) = events(&old, new, &[50, 80, 100]);
                notes.len() + ips.len()
            })
            .collect()
    }

    #[test]
    fn failed_reads_repeat_no_alert_and_lose_no_traffic() {
        let c = conn();
        let gib = 1i64 << 30;
        let reading = |rx: i64, alert: &str, seq: u64| json!({"rx":rx,"tx":0,"period_start":"","alert":alert,"ip_level":2,"ip_alert":seq,"ip_count":4});
        for state in [("quota_warning", gib * 85 / 100), ("quota_reached", gib), ("expired", gib / 10)] {
            let (alert, rx) = state;
            let good = reported(Some(reading(rx, alert, 7)));
            let failed = reported(None);
            let reports = [good.clone(), failed.clone(), failed, good];
            assert_eq!(alerts(&c, &reports), [0, 0, 0], "{alert}");
        }
        // What really changed during the failure is still told, once.
        let reports = [
            reported(Some(reading(gib / 10, "normal", 7))),
            reported(None),
            reported(Some(reading(gib / 10, "normal", 8))),
            reported(Some(reading(gib / 10, "normal", 8))),
        ];
        assert_eq!(alerts(&c, &reports), [0, 1, 0]);

        let c = conn();
        let reports = [
            reported(Some(reading(100, "normal", 7))),
            reported(None),
            reported(Some(reading(160, "normal", 7))),
        ];
        alerts(&c, &reports);
        assert_eq!(hours(&c, 1000), json!([[9 * 3_600, 60, 0]]));
    }

    #[test]
    fn only_a_rule_still_reported_is_remembered() {
        let old = reported(Some(json!({"rx":1,"tx":1,"period_start":"","alert":"normal"})));
        let mut removed = reported(None);
        removed["modules"]["pm"]["resources"] = json!([]);
        assert!(remember(&old, &removed)["modules"]["pm"].get(LAST_USAGE).is_none());
        assert!(remember(&old, &old)["modules"]["pm"].get(LAST_USAGE).is_none());
        assert!(remember(&old, &reported(None))["modules"]["pm"].get(LAST_USAGE).is_some());
    }

    #[test]
    fn history_ends_with_its_rule_its_binding_or_the_week() {
        let c = conn();
        let (old, new) = (
            report(&[("1000", 0, 0, ""), ("2000", 0, 0, "")]),
            report(&[("1000", 1, 1, ""), ("2000", 1, 1, "")]),
        );
        record(&c, 1, &old, &new, 3_600).unwrap();
        record(&c, 1, &old, &new, 9 * 3_600).unwrap();
        forget(&c, 1, 2000).unwrap();
        assert_eq!(hours(&c, 2000), json!([]));
        assert_eq!(prune(&c, 3_600 + KEEP).unwrap(), 1);
        assert_eq!(hours(&c, 1000), json!([[9 * 3_600, 1, 1]]));
        c.execute("DELETE FROM managed_node", []).unwrap();
        assert_eq!(hours(&c, 1000), json!([]));
    }
}
