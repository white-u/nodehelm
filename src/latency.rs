//! Latency alerts: a probe pair (node, probe) whose round trip or loss turns
//! bad and stays bad, judged against that pair's own history.
//!
//! The samples are TCP handshakes with a 900 ms deadline, so anything slower
//! arrives as a loss: loss is judged on its own, and a slow link shows up there
//! once the round trip passes the deadline.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::notify::Note;
use crate::App;

/// Node ids, comma separated, whose probes are watched. Empty watches none.
pub const NODES: &str = "notify_latency_nodes";
/// What was announced, as JSON; outside `notify_`, which the panel writes.
const STATE: &str = "latency_alerted";

/// Probes run at most this far apart are judged; slower ones would take close
/// to an hour to gather a window.
const MAX_INTERVAL: i64 = 300;
/// The shortest window, and the probe rounds it spans at the least: with five,
/// random spikes on half the answers of a probe run every minute already
/// raised an alert every hour or so.
const WINDOW: i64 = 300;
const WINDOW_ROUNDS: i64 = 10;
/// Samples a window must hold, one short of its rounds: a round skipped or
/// filed late, or stamps drifting past the window's edge, would otherwise leave
/// a probe run every minute unjudged.
const MIN_SAMPLES: usize = 9;
/// A pair back within this long of recovering waits three windows, not one.
const FLAP_WINDOW: i64 = 3_600;
/// An alerted pair recovers over at least this long, so that a dip between two
/// bad spells is not announced.
const RECOVERY_WINDOW: i64 = 600;
/// History the baseline reads, and the hours it needs before anything is judged.
const BASELINE_SPAN: i64 = 7 * 86_400;
const BASELINE_HOURS: usize = 12;
/// A pair whose hours usually lose more than this percentage is a poor route
/// rather than a normal one that changed, and raises no alert. The median, so
/// that an outage of a few hours does not make a normal route poor.
const LOSSY: i64 = 15;
/// Results are filed a minute at a time, so the newest minute may be missing:
/// a window ends at the newest result filed rather than at the present.
const FILING_LAG: i64 = 60;

/// A round trip over both of these is high: double alone would flag 5 ms
/// becoming 15, an increment alone would miss 300 ms becoming 400.
fn threshold(baseline: i64) -> i64 {
    (baseline * 2).max(baseline + 50)
}

/// Back under both of these is normal again; the increment keeps a baseline
/// near zero from never recovering.
fn recovery(baseline: i64) -> i64 {
    (baseline * 3 / 2).max(baseline + 25)
}

/// The watched nodes, read from [`NODES`].
pub fn nodes(app: &App) -> HashSet<i64> {
    app.db.get(NODES).unwrap_or_default().split(',').filter_map(|id| id.trim().parse().ok()).collect()
}

/// Why a value cannot be stored in [`NODES`], or `None` when it can.
pub fn nodes_error(value: &str) -> Option<&'static str> {
    let valid = value.is_empty() || value.split(',').all(|id| id.parse::<u64>().is_ok());
    (!valid).then_some("延迟通知的节点要填逗号分隔的节点 ID")
}

#[derive(Serialize, Deserialize, Default, PartialEq)]
struct State {
    /// Per `node:task`, the baseline frozen when announced. Frozen, because a
    /// long outage would otherwise become the normal and recover by itself.
    alerted: BTreeMap<String, i64>,
    /// Per `node:task`, when it last recovered.
    recovered: BTreeMap<String, i64>,
    /// Per probe, its target and since when, so that history from an earlier
    /// target does not make the baseline.
    targets: BTreeMap<i64, (String, i64)>,
}

/// Baselines per pair, recomputed once an hour; `None` where history is too
/// short, so such a pair is not read again every minute. Kept in memory only:
/// they follow from the database.
#[derive(Default)]
pub struct Watch {
    minute: i64,
    hour: i64,
    baselines: HashMap<(i64, i64), Option<Baseline>>,
}

struct Baseline {
    overall: i64,
    /// The median of the same hour of day, ±1 h, where three hours exist.
    by_hour: [Option<i64>; 24],
    /// Whether the hours usually lose more than [`LOSSY`]. Only raising an
    /// alert is skipped: one raised before still recovers, so that a pair near
    /// the line is not announced again each time it crosses back.
    lossy: bool,
}

impl Baseline {
    /// The higher of the two, so that a route's regular evening peak is not news.
    fn at(&self, now: i64) -> i64 {
        let hour = (now / 3_600 % 24) as usize;
        self.by_hour[hour].map_or(self.overall, |h| h.max(self.overall))
    }

    fn from(hours: &[(i64, i64)]) -> Option<Self> {
        if hours.len() < BASELINE_HOURS {
            return None;
        }
        let overall = median(hours.iter().map(|&(_, l)| l).collect())?;
        let by_hour = std::array::from_fn(|h| {
            let near = |ts: i64| ((ts / 3_600 % 24) as usize + 24 - h) % 24;
            let slot: Vec<i64> =
                hours.iter().filter(|&&(ts, _)| matches!(near(ts), 0 | 1 | 23)).map(|&(_, l)| l).collect();
            (slot.len() >= 3).then(|| median(slot)).flatten()
        });
        Some(Self { overall, by_hour, lossy: false })
    }
}

fn median(mut values: Vec<i64>) -> Option<i64> {
    values.sort_unstable();
    values.get(values.len() / 2).copied()
}

/// What one window of samples says.
#[derive(Debug, PartialEq)]
enum Verdict {
    Bad,
    Good,
    /// Neither: too few samples, or between the two thresholds.
    Unchanged,
}

struct Window {
    median: Option<i64>,
    lost: usize,
    total: usize,
}

impl Window {
    fn loss(&self) -> usize {
        self.lost * 100 / self.total
    }
}

/// Bad when four in five answers are above the threshold, or half the probes
/// were lost; good again when four in five answers are under [`recovery`] and
/// loss is under 20%. The gap between the two keeps a pair near its threshold
/// from flapping.
fn judge(samples: &[i64], baseline: i64, alerted: bool) -> (Verdict, Window) {
    let answered: Vec<i64> = samples.iter().copied().filter(|&l| l >= 0).collect();
    let window = Window {
        lost: samples.len() - answered.len(),
        total: samples.len(),
        median: median(answered.clone()),
    };
    if samples.len() < MIN_SAMPLES {
        return (Verdict::Unchanged, window);
    }
    let high = answered.iter().filter(|&&l| l > threshold(baseline)).count();
    let bad = window.loss() >= 50 || (!answered.is_empty() && high * 5 >= answered.len() * 4);
    let low = answered.iter().filter(|&&l| l < recovery(baseline)).count();
    let good = window.loss() < 20 && !answered.is_empty() && low * 5 >= answered.len() * 4;
    let verdict = match (alerted, bad, good) {
        (false, true, _) => Verdict::Bad,
        (true, _, true) => Verdict::Good,
        _ => Verdict::Unchanged,
    };
    (verdict, window)
}

/// One line per pair, or one per probe when at least half its watched nodes,
/// and three or more, changed at once: then the target is the likelier cause.
fn lines(
    items: Vec<(i64, String, String)>,
    watched: &HashMap<i64, usize>,
    names: &HashMap<i64, &str>,
    what: &str,
) -> Vec<(String, String)> {
    let mut by_task: BTreeMap<i64, Vec<(String, String)>> = BTreeMap::new();
    for (task, node, detail) in items {
        by_task.entry(task).or_default().push((node, detail));
    }
    let mut out = Vec::new();
    for (task, mut pairs) in by_task {
        let total = watched.get(&task).copied().unwrap_or(0);
        let probe = names.get(&task).copied().unwrap_or("?");
        if pairs.len() >= 3 && pairs.len() * 2 >= total {
            out.push((probe.to_owned(), format!("{}/{total} 个节点同时{what}，可能是目标侧", pairs.len())));
        } else {
            out.extend(pairs.drain(..).map(|(node, detail)| (node, format!("{probe}：{detail}"))));
        }
    }
    out
}

fn note(event: &'static str, mark: &str, what: &str, lines: Vec<(String, String)>) -> Option<Note> {
    let (title, message) = match lines.as_slice() {
        [] => return None,
        [(name, detail)] => (format!("{mark} {name} {what}"), detail.clone()),
        _ => (
            format!("{mark} {} 项{what}", lines.len()),
            lines.iter().map(|(name, detail)| format!("{name} · {detail}")).collect::<Vec<_>>().join("\n"),
        ),
    };
    let node = lines.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(", ");
    Some(Note { event, node, title, message, ..Default::default() })
}

/// One pass, once a minute. `armed` is whether any channel is configured:
/// without one nothing is recorded as announced, so a channel configured later
/// still hears of pairs already bad.
pub fn check(
    app: &App,
    watch: &mut Watch,
    online: &HashSet<i64>,
    armed: bool,
    now: i64,
) -> Result<Vec<Note>> {
    if now / 60 == watch.minute {
        return Ok(Vec::new());
    }
    watch.minute = now / 60;
    let chosen = nodes(app);
    let stored = app.db.lookup(STATE)?;
    let mut state: State = stored.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
    let before = serde_json::to_string(&state)?;

    let tasks = app.db.ping_tasks()?;
    let node_names: HashMap<i64, String> = app.db.nodes()?.into_iter().map(|n| (n.id, n.name)).collect();
    let task_ids: HashSet<i64> = tasks.iter().map(|t| t.id).collect();
    state.targets.retain(|id, _| task_ids.contains(id));
    for task in &tasks {
        let known = state.targets.get(&task.id).map(|(target, _)| target == &task.target);
        if known != Some(true) {
            // First seen: its history is its own. Changed: start over.
            let since = if known.is_none() { 0 } else { now };
            state.targets.insert(task.id, (task.target.clone(), since));
            let prefix = format!(":{}", task.id);
            state.alerted.retain(|key, _| !key.ends_with(&prefix));
            watch.baselines.retain(|&(_, t), _| t != task.id);
        }
    }
    let judged: Vec<_> = tasks.iter().filter(|t| t.interval <= MAX_INTERVAL).collect();
    let judged_ids: HashSet<i64> = judged.iter().map(|t| t.id).collect();
    // A node no longer watched, or gone, and a probe gone or no longer judged,
    // leave silently.
    let live = |key: &str| {
        key.split_once(':')
            .and_then(|(n, t)| Some((n.parse::<i64>().ok()?, t.parse::<i64>().ok()?)))
            .is_some_and(|(n, t)| {
                chosen.contains(&n) && node_names.contains_key(&n) && judged_ids.contains(&t)
            })
    };
    state.alerted.retain(|key, _| live(key));
    state.recovered.retain(|key, &mut at| live(key) && now - at < FLAP_WINDOW);

    if now / 3_600 != watch.hour {
        watch.hour = now / 3_600;
        watch.baselines.clear();
    }
    let watched: HashMap<i64, usize> =
        judged.iter().map(|t| (t.id, t.nodes.iter().filter(|n| chosen.contains(n)).count())).collect();
    let probe_names: HashMap<i64, &str> = tasks.iter().map(|t| (t.id, t.name.as_str())).collect();
    let (mut bad, mut good) = (Vec::new(), Vec::new());
    // An offline node has its own alert, and its pairs keep their state.
    let mut nodes: Vec<i64> = chosen.iter().copied().filter(|n| online.contains(n)).collect();
    nodes.sort_unstable();
    for node in nodes {
        let Some(name) = node_names.get(&node) else { continue };
        let mine: Vec<_> = judged.iter().filter(|t| t.nodes.contains(&node)).collect();
        if mine.is_empty() {
            continue;
        }
        if mine.iter().any(|t| !watch.baselines.contains_key(&(node, t.id))) {
            let mut hours: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
            let mut losses: HashMap<i64, Vec<i64>> = HashMap::new();
            for (task, ts, latency, loss) in app.db.hourly_pings(node, now - BASELINE_SPAN)? {
                if ts >= state.targets.get(&task).map_or(0, |(_, since)| *since) {
                    hours.entry(task).or_default().push((ts, latency));
                    losses.entry(task).or_default().push(loss);
                }
            }
            for task in &mine {
                let b =
                    Baseline::from(hours.get(&task.id).map_or(&[][..], Vec::as_slice)).map(|b| Baseline {
                        lossy: losses.remove(&task.id).and_then(median).is_some_and(|l| l > LOSSY),
                        ..b
                    });
                watch.baselines.insert((node, task.id), b);
            }
        }
        let windows: HashMap<i64, i64> = mine
            .iter()
            .map(|t| {
                let key = format!("{node}:{}", t.id);
                let shortest = WINDOW.max(WINDOW_ROUNDS * t.interval.max(1));
                let window = if state.alerted.contains_key(&key) {
                    RECOVERY_WINDOW.max(shortest)
                } else if state.recovered.contains_key(&key) {
                    shortest * 3
                } else {
                    shortest
                };
                (t.id, window)
            })
            .collect();
        let widest = windows.values().copied().max().unwrap_or(WINDOW);
        let mut samples: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
        for (task, ts, latency) in app.db.recent_pings(node, now - widest - FILING_LAG)? {
            samples.entry(task).or_default().push((ts, latency));
        }
        for task in mine {
            let key = format!("{node}:{}", task.id);
            let frozen = state.alerted.get(&key).copied();
            let learned = watch.baselines.get(&(node, task.id)).and_then(|b| b.as_ref());
            if frozen.is_none() && learned.is_some_and(|b| b.lossy) {
                continue;
            }
            let Some(baseline) = frozen.or_else(|| learned.map(|b| b.at(now))) else {
                continue;
            };
            let all = samples.remove(&task.id).unwrap_or_default();
            let window = windows[&task.id];
            // Oldest first, so the last is the newest. A pair that recovered is
            // judged on what followed: its longer window would otherwise still
            // hold the spell it recovered from and announce it again at once.
            let newest = all.last().map_or(now, |&(ts, _)| ts);
            let from = match frozen {
                None => state.recovered.get(&key).copied().unwrap_or(i64::MIN),
                Some(_) => i64::MIN,
            };
            let recent: Vec<i64> =
                all.iter().filter(|&&(ts, _)| ts > newest - window && ts > from).map(|&(_, l)| l).collect();
            let expected = (window / task.interval.max(1)) as usize;
            if recent.len() < MIN_SAMPLES.max(expected * 3 / 4) {
                continue;
            }
            let (verdict, w) = judge(&recent, baseline, frozen.is_some());
            if !armed {
                continue;
            }
            let latency = w.median.map_or_else(|| "无应答".to_owned(), |m| format!("{m} ms"));
            match verdict {
                Verdict::Bad => {
                    state.alerted.insert(key, baseline);
                    bad.push((
                        task.id,
                        name.clone(),
                        format!("{latency}（平时 {baseline} ms），丢包 {}%", w.loss()),
                    ));
                }
                Verdict::Good => {
                    state.alerted.remove(&key);
                    state.recovered.insert(key, now);
                    good.push((task.id, name.clone(), format!("{latency}，丢包 {}%", w.loss())));
                }
                Verdict::Unchanged => {}
            }
        }
    }
    let after = serde_json::to_string(&state)?;
    if after != before {
        // Announced only once recorded, or every pass would announce it again.
        if let Err(e) = app.db.set(STATE, &after) {
            warn!("saving the latency alerts announced failed: {e:#}");
            return Ok(Vec::new());
        }
    }
    Ok([
        note("latency", "🟠", "延迟异常", lines(bad, &watched, &probe_names, "异常")),
        note("latency_recovered", "🟢", "延迟恢复", lines(good, &watched, &probe_names, "恢复")),
    ]
    .into_iter()
    .flatten()
    .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_spikes_do_not_alert_but_a_sustained_rise_does() {
        let base = 40;
        let mixed = |high: usize, l: i64| [vec![l; high], vec![41; 10 - high]].concat();
        // Seven of ten high: a burst, not news.
        assert_eq!(judge(&mixed(7, 300), base, false).0, Verdict::Unchanged);
        assert_eq!(judge(&mixed(8, 300), base, false).0, Verdict::Bad);
        // Double alone is not enough on a short route.
        assert_eq!(judge(&[12; 10], 5, false).0, Verdict::Unchanged);
        assert_eq!(judge(&[60; 10], 5, false).0, Verdict::Bad);
        // One round missing is still judged; two say nothing.
        assert_eq!(judge(&[300; 9], base, false).0, Verdict::Bad);
        assert_eq!(judge(&[300; 8], base, false).0, Verdict::Unchanged);
    }

    #[test]
    fn loss_alerts_on_its_own_and_recovery_needs_both() {
        let mixed = |first: usize, a: i64, b: i64| [vec![a; first], vec![b; 10 - first]].concat();
        // Past the 900 ms deadline a slow link reads as loss.
        assert_eq!(judge(&mixed(5, -1, 400), 400, false).0, Verdict::Bad);
        assert_eq!(judge(&mixed(4, -1, 40), 40, false).0, Verdict::Unchanged);
        // Four in five back under 1.5 × with little loss recovers; a median
        // under it alone, or between the thresholds, holds.
        assert_eq!(judge(&mixed(8, 45, 300), 40, true).0, Verdict::Good);
        assert_eq!(judge(&mixed(6, 45, 300), 40, true).0, Verdict::Unchanged);
        assert_eq!(judge(&[70; 10], 40, true).0, Verdict::Unchanged);
        assert_eq!(judge(&mixed(2, -1, 45), 40, true).0, Verdict::Unchanged);
        // A baseline of 0 ms (a probe on the same host) still recovers.
        assert_eq!(judge(&[3; 10], 0, true).0, Verdict::Good);
    }

    #[test]
    fn the_baseline_takes_the_evening_peak_and_needs_half_a_day() {
        assert!(Baseline::from(&(0..11).map(|h| (h * 3_600, 40)).collect::<Vec<_>>()).is_none());
        // Two days: 40 ms all day, 100 ms from 20:00 to 22:00 UTC.
        let hours: Vec<(i64, i64)> =
            (0..48).map(|h| (h * 3_600, if (20..=22).contains(&(h % 24)) { 100 } else { 40 })).collect();
        let b = Baseline::from(&hours).unwrap();
        assert_eq!(b.at(10 * 3_600), 40);
        assert_eq!(b.at(21 * 3_600), 100, "the regular peak is the normal at that hour");
    }

    #[test]
    fn many_nodes_of_one_probe_become_one_line() {
        let names = HashMap::from([(1, "上海"), (2, "广州")]);
        let watched = HashMap::from([(1, 4), (2, 10)]);
        let items = vec![
            (1, "a".into(), "x".into()),
            (1, "b".into(), "x".into()),
            (1, "c".into(), "x".into()),
            (2, "a".into(), "y".into()),
        ];
        let out = lines(items, &watched, &names, "异常");
        assert_eq!(out[0], ("上海".to_owned(), "3/4 个节点同时异常，可能是目标侧".to_owned()));
        assert_eq!(out[1], ("a".to_owned(), "广州：y".to_owned()));
        assert_eq!(nodes_error("1,2"), None);
        assert!(nodes_error("1,x").is_some());
    }
}
