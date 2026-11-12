//! Traffic counters, minute metrics and their hourly tier, and latency probes.

use super::*;

#[derive(Deserialize, Default)]
pub struct TrafficPatch {
    pub total_rx: Option<i64>,
    pub total_tx: Option<i64>,
    pub month_rx: Option<i64>,
    pub month_tx: Option<i64>,
}
pub(super) fn usd() -> String {
    "USD".into()
}
pub(super) fn monthly() -> String {
    "monthly".into()
}
pub(super) fn sum() -> String {
    "sum".into()
}
pub(super) fn one() -> u32 {
    1
}

#[derive(Serialize, Debug, Clone, Default, PartialEq)]
pub struct Traffic {
    pub total_rx: i64,
    pub total_tx: i64,
    pub month_rx: i64,
    pub month_tx: i64,
    pub month_start: String,
    pub day_rx: i64,
    pub day_tx: i64,
}

impl Traffic {
    /// This period's usage as the node's plan meters it. Summing both directions
    /// regardless would hold a plan billed on upload alone against the wrong
    /// figure. The traffic alert and `node_view` both read this, so the
    /// percentage an alert quotes matches the usage the pages show.
    pub fn month_used(&self, traffic_mode: &str) -> i64 {
        match traffic_mode {
            "up" => self.month_tx,
            "down" => self.month_rx,
            "max" => self.month_rx.max(self.month_tx),
            _ => self.month_rx.saturating_add(self.month_tx),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct PingTask {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    pub target: String,
    #[serde(default)]
    pub interval: i64,
    #[serde(default)]
    pub nodes: Vec<i64>,
    /// Nodes created later are assigned this probe as they are added. Existing
    /// nodes follow `nodes` alone.
    #[serde(default)]
    pub auto_join: bool,
    /// The assignments the editor started from. When given on an update, only
    /// the difference between it and `nodes` is applied, so an assignment made
    /// while the editor was open -- a node joining through `auto_join` -- is
    /// not removed by a list that predates it.
    #[serde(default, skip_serializing)]
    pub base: Option<Vec<i64>>,
}

/// The rows behind the latency chart: one node's probe results over a window,
/// bucketed and in time order. Everything the chart draws is folded out of them
/// in [`close_bucket`].
///
/// The key is `(node_id, ts, task_id)`, so this is a seek and the rows emerge
/// sorted without a sorter, which is what allows the fold to hold one bucket at
/// a time. Asking SQLite for the summary instead cost three sorts of the whole
/// window -- two window passes and a GROUP BY -- against this single scan: on a
/// week of four probes, 284 ms against 54 ms, all of it holding the connection
/// the agents write through.
///
/// A constant because the query plan is asserted against it in
/// `rekeying_ping_record_keeps_the_rows_and_lets_the_chart_query_seek`.
pub(super) const PING_ROWS: &str = "SELECT ts/?3, task_id, latency FROM ping_record
     WHERE node_id=?1 AND ts>=?2
           AND task_id IN (SELECT task_id FROM ping_node WHERE node_id=?1)
     ORDER BY ts";

/// [`PING_ROWS`] for the hourly tier: the folded hours before the watermark
/// `?4`, in time order off the key.
const PING_HOURS: &str = "SELECT ts/?3, task_id, answered, lost, latency, lo, hi FROM ping_hour
     WHERE node_id=?1 AND ts>=?2 AND ts<?4
           AND task_id IN (SELECT task_id FROM ping_node WHERE node_id=?1)
     ORDER BY ts";

/// Minute rows are kept this many days at most, and windows up to this wide are
/// drawn from them. It is the widest window where they change what is drawn: at
/// the 1,440-point budget a week is 7-minute points, where hourly rows would
/// give 168. A month is half-hour points from minute rows and hourly from the
/// tier, which a chart the width of a screen cannot tell apart.
pub const DETAIL_DAYS: i64 = 7;

/// The longest history a hub keeps. A year spans the longest billing cycle the
/// panel records, so a machine paid annually can be judged over a whole term.
/// At 100 nodes with four probes the hourly tier holds about 145 MiB for it,
/// where minute rows would hold 6.6 GiB.
pub const MAX_RETENTION_DAYS: i64 = 365;

/// History kept where the panel has never saved a value: a month. Its hourly
/// tier adds about 0.12 MiB per node with four probes to the week of minute rows
/// every setting keeps.
pub(super) const DEFAULT_RETENTION_DAYS: i64 = 30;

/// How long after an hour ends its rows may still arrive. Metric rows are
/// written as they are stamped, but probe results wait in the session until a
/// frame from a later minute arrives, and an agent may send one as seldom as
/// once an hour: its `--interval` and a probe's interval are both capped at
/// 3600 seconds.
pub(super) const LATE: i64 = 3_600 + 60;

/// Setting holding the start of the first hour not yet folded into the hourly
/// tier. Internal: the settings routes neither return nor accept it.
const ROLLED: &str = "history_rolled";

/// One chart window as the database answers it.
#[derive(Clone, Copy, Debug)]
pub struct Span {
    /// Start of the window, in unix seconds.
    pub since: i64,
    /// Seconds each returned point covers: whole minutes, or whole hours when
    /// `hourly`, so that no hourly row straddles two points.
    pub step: i64,
    /// Whether the window reaches past [`DETAIL_DAYS`] and is read from the
    /// hourly tier.
    pub hourly: bool,
}

#[cfg(test)]
impl Span {
    /// A window read from minute rows alone.
    pub fn minutes(since: i64, step: i64) -> Self {
        Span { since, step, hourly: false }
    }
}

/// Lets a caller waiting on the connection take it between the steps of a long
/// maintenance run. Dropping the guard alone does not: `std::sync::Mutex` is
/// not fair, and the thread releasing it takes it back before a woken waiter
/// runs. Without the pause, on the upgrade of 90 days of 100 nodes, a request
/// would wait up to 12 s behind a prune releasing the lock between nodes.
fn let_waiters_in() {
    std::thread::sleep(std::time::Duration::from_millis(1));
}

/// Set once the hub begins to stop, and checked between the steps of
/// `roll_up` and `prune`. The runtime waits on blocking work before the process
/// exits, so the catch-up after an upgrade, minutes long, would otherwise hold
/// the exit past systemd's 90-second stop timeout and end in SIGKILL. Each step
/// commits on its own, and the next pass resumes where this one stopped.
static HALTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn halt() {
    HALTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn halted() -> bool {
    HALTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The first hour not yet folded into the hourly tier, `None` before the first
/// rollup, while every row is still a minute row.
fn rolled(conn: &Connection) -> Result<Option<i64>> {
    let value: Option<String> =
        conn.query_row("SELECT value FROM setting WHERE key=?1", [ROLLED], |r| r.get(0)).optional()?;
    Ok(value.and_then(|v| v.parse().ok()))
}

/// The earliest `ts` across `tables`, sought node by node: every key here
/// begins with `node_id`, and `MIN(ts)` over a whole table scans it, 15.9 s at
/// 90 days of 100 nodes against 1.8 ms this way. `tables` are names from
/// [`TABLES`].
pub(super) fn oldest(conn: &Connection, tables: &[&str]) -> Result<Option<i64>> {
    let per_node: Vec<String> = tables
        .iter()
        .map(|t| format!("SELECT (SELECT MIN(ts) FROM {t} WHERE node_id=n.id) AS ts FROM node n"))
        .collect();
    let sql = format!("SELECT MIN(ts) FROM ({})", per_node.join(" UNION ALL "));
    Ok(conn.query_row(&sql, [], |r| r.get(0))?)
}

/// Probe results as the latency fold receives them. A stored result is a
/// single answer or a single loss; an hourly row is that hour's answers,
/// represented by their median, and its losses.
struct Sample {
    answered: i64,
    lost: i64,
    median: Option<i64>,
    lo: Option<i64>,
    hi: Option<i64>,
}

impl Sample {
    /// One stored result. A timeout is stored as -1: counted as lost, and kept
    /// out of the median.
    fn result(latency: i64) -> Self {
        let answer = (latency >= 0).then_some(latency);
        Sample {
            answered: i64::from(answer.is_some()),
            lost: i64::from(answer.is_none()),
            median: answer,
            lo: answer,
            hi: answer,
        }
    }
}

/// One probe's samples within one bucket.
#[derive(Default)]
struct Tally {
    /// Each sample's median and the answers it stands for.
    medians: Vec<(i64, i64)>,
    lost: i64,
    lo: Option<i64>,
    hi: Option<i64>,
}

impl Tally {
    fn add(&mut self, s: Sample) {
        if let Some(median) = s.median {
            self.medians.push((median, s.answered));
        }
        self.lost += s.lost;
        self.lo = self.lo.into_iter().chain(s.lo).min();
        self.hi = self.hi.into_iter().chain(s.hi).max();
    }

    fn answered(&self) -> i64 {
        self.medians.iter().map(|m| m.1).sum()
    }

    /// The median of the answers, each sample's median counted once per answer
    /// it stands for. Over single results that is the ordinary median, the
    /// middle two averaged. Over hourly rows it is exact while a bucket is one
    /// hour and an approximation once it spans several, since the hours'
    /// medians stand in for the answers themselves.
    fn median(&mut self) -> Option<i64> {
        self.medians.sort_unstable();
        let total = self.answered();
        let at = |rank: i64| {
            let mut seen = 0;
            self.medians.iter().find(|m| {
                seen += m.1;
                seen >= rank
            })
        };
        // The 1-based ranks of the middle answer, or of the middle two.
        Some((at((total + 1) / 2)?.0 + at(total / 2 + 1)?.0) / 2)
    }
}

impl Db {
    // ---- traffic ----

    /// Every node's counters in one query, because the node list renders a row per
    /// node and a query per node would queue the agents' writes behind it.
    ///
    /// The period counters are gated on the period they were written for. They
    /// restart lazily in `accumulate`, on the node's next report, so a node
    /// offline since before a boundary still holds the previous period's bytes on
    /// disk. This is the only reader, so the rule lives in one place.
    pub fn all_traffic(&self) -> HashMap<i64, Traffic> {
        let conn = self.conn();
        let Ok(mut stmt) = conn.prepare_cached(
            "SELECT t.node_id, t.total_rx, t.total_tx, t.month_rx, t.month_tx, t.month_start,
                    t.day_rx, t.day_tx, t.day_start, n.traffic_reset_day
                 FROM traffic t JOIN node n ON n.id = t.node_id",
        ) else {
            return HashMap::new();
        };
        let today = Local::now().date_naive();
        let day = today.to_string();
        let rows = stmt.query_map([], |r| {
            // Zero rather than absent: a theme drawing a meter requires a
            // number.
            let current = |stored: String, now: &str, rx: i64, tx: i64| {
                if stored == now {
                    (rx, tx)
                } else {
                    (0, 0)
                }
            };
            let period = period_start(today, r.get(9)?).to_string();
            let (month_rx, month_tx) = current(r.get(5)?, &period, r.get(3)?, r.get(4)?);
            let (day_rx, day_tx) = current(r.get(8)?, &day, r.get(6)?, r.get(7)?);
            Ok((
                r.get::<_, i64>(0)?,
                Traffic {
                    total_rx: r.get(1)?,
                    total_tx: r.get(2)?,
                    month_rx,
                    month_tx,
                    month_start: period,
                    day_rx,
                    day_tx,
                },
            ))
        });
        rows.map(|r| r.flatten().collect()).unwrap_or_default()
    }

    /// The traffic row as stored, without the period gate `all_traffic` applies,
    /// for tests that compare what two ways of booking left behind.
    #[cfg(test)]
    pub fn stored_traffic(&self, node_id: i64) -> Traffic {
        self.conn()
            .query_row(
                "SELECT total_rx, total_tx, month_rx, month_tx, month_start, day_rx, day_tx
                 FROM traffic WHERE node_id=?1",
                [node_id],
                |r| {
                    Ok(Traffic {
                        total_rx: r.get(0)?,
                        total_tx: r.get(1)?,
                        month_rx: r.get(2)?,
                        month_tx: r.get(3)?,
                        month_start: r.get(4)?,
                        day_rx: r.get(5)?,
                        day_tx: r.get(6)?,
                    })
                },
            )
            .expect("a traffic row")
    }

    /// Books one reading of a node's raw kernel counters into its running totals.
    ///
    /// A changed boot_id, or a counter that moved backwards, means the reading no
    /// longer continues the previous one; the total must not follow it downward.
    /// Readings arrive here about once a minute rather than with every report,
    /// which gives the same totals: see `agent_ws::file`.
    ///
    /// `at` is when the hub received the reading, and dates it for the day and
    /// billing period. A reading held back over midnight is booked after it, and
    /// still belongs to the day it arrived in.
    ///
    /// The billing reset day is read here rather than passed in: it is one join
    /// from a row this already reads, and fetching it separately would cost every
    /// booking a second acquisition of the single write connection.
    ///
    /// `booted` is when the machine started, by the hub's clock: the arrival
    /// less the reported uptime.
    pub fn accumulate(
        &self,
        node_id: i64,
        boot_id: &str,
        (rx, tx): (i64, i64),
        at: DateTime<Local>,
        booted: Option<i64>,
    ) -> Result<Traffic> {
        let keep = self.retention_days();
        let mut conn = self.conn();
        let transaction = conn.transaction()?;
        let (
            prev_boot,
            last_rx,
            last_tx,
            mut total_rx,
            mut total_tx,
            mut month_rx,
            mut month_tx,
            month_start,
            mut day_rx,
            mut day_tx,
            day_start,
            reset_day,
            last_seen,
        ) = transaction
            .prepare_cached(
                "SELECT t.boot_id, t.last_rx, t.last_tx, t.total_rx, t.total_tx, t.month_rx, t.month_tx,
                    t.month_start, t.day_rx, t.day_tx, t.day_start, n.traffic_reset_day, n.last_seen
                 FROM traffic t JOIN node n ON n.id = t.node_id WHERE t.node_id=?1",
            )?
            .query_row([node_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, i64>(9)?,
                    r.get::<_, String>(10)?,
                    r.get::<_, u32>(11)?,
                    r.get::<_, i64>(12)?,
                ))
            })?;

        // Only bytes this hub observed a counter climb through are booked.
        // Without a baseline under this exact boot there is nothing to subtract
        // from, and a bare reading represents the machine's entire history.
        //
        // The baseline can be missing in three ways, all handled identically. A
        // first reading has none. A reading that shrank under the same boot lost
        // one -- an interface included in the sum has disappeared -- so the
        // reading is the remainder of that history and booking it would count it
        // twice. A changed boot_id means the counters restarted, that the agent
        // now sums a different set of interfaces (it appends a digest of them,
        // so a device joining the sum is caught as well as one leaving it), or,
        // indistinguishably from the epoch alone, that a second machine shares
        // the token. Booking the reading would cost hundreds of gigabytes
        // against a total that only increases.
        //
        // Except for a machine that booted after the hub last heard from this
        // token: its counters started from zero while unobserved, so the reading
        // is exactly the bytes since boot -- a reboot during a hub outage would
        // otherwise lose the whole outage. A second machine sharing the token
        // booted before the other's last report, and an epoch whose kernel part
        // is unchanged is a different set of interfaces, not a restart. The
        // bytes between the last booking and the reboot remain lost.
        // `last_seen` trails the last report by up to a minute, so a machine
        // booted within it books its own bytes since boot, once; the next
        // report from the other moves `last_seen` past both boots.
        let unseen_boot = !prev_boot.is_empty()
            && prev_boot.split('/').next() != boot_id.split('/').next()
            && booted.is_some_and(|b| last_seen > 0 && b > last_seen);
        let (d_rx, d_tx) = if unseen_boot {
            info!("node {node_id} restarted after it was last seen; booking its counters since boot");
            (rx, tx)
        } else if prev_boot.is_empty() || prev_boot != boot_id {
            // Logged in either case: on a healthy node this is a reboot or the
            // agent summing a different set of interfaces, while one every few
            // seconds indicates two machines sharing a token or counted
            // interfaces coming and going. The value stays out of the log: it is
            // the agent's text.
            if !prev_boot.is_empty() {
                info!("node {node_id} reports a new boot_id; re-aligning");
            }
            (0, 0)
        } else {
            ((rx.saturating_sub(last_rx)).max(0), (tx.saturating_sub(last_tx)).max(0))
        };
        // Saturating rather than a plain `+`: the release profile disables
        // overflow checks, so a total near i64::MAX would wrap to a large
        // negative -- a lifetime figure that has decreased. Two paths reach this
        // column: a node's own counters, which arrive from another repository's
        // binary, and `set_traffic`, through which the panel writes corrections.
        // Clamping here covers both rather than each caller separately.
        total_rx = total_rx.saturating_add(d_rx);
        total_tx = total_tx.saturating_add(d_tx);
        month_rx = month_rx.saturating_add(d_rx);
        month_tx = month_tx.saturating_add(d_tx);
        day_rx = day_rx.saturating_add(d_rx);
        day_tx = day_tx.saturating_add(d_tx);

        // Both boundaries are calendar dates -- the day a provider resets an
        // allowance, the day a person means by "today" -- so both follow the
        // hub's local timezone rather than UTC.
        //
        // Never dated before a period the row already carries. The panel stamps
        // the current period with a correction, which a reading held back from
        // before the boundary would otherwise read as a new period and discard.
        // A date later than the stamp is used as it is, so a changed reset day
        // still takes effect. A stamp later than today came from a clock since
        // stepped back and is not honoured: it would hold both counters in that
        // period, which the read side answers as zero, until the date caught up.
        let (date, today) = (at.date_naive(), Local::now().date_naive());
        let dated = |stored: &str| {
            stored
                .parse::<NaiveDate>()
                .ok()
                .filter(|stamp| *stamp <= today)
                .map_or(date, |stamp| stamp.max(date))
        };
        let period = period_start(dated(&month_start), reset_day).to_string();
        if month_start != period {
            // A new billing period restarts the month counter but not the total.
            month_rx = d_rx;
            month_tx = d_tx;
        }
        let day = dated(&day_start).to_string();
        if day_start != day {
            day_rx = d_rx;
            day_tx = d_tx;
        }

        transaction
            .prepare_cached(
                "UPDATE traffic SET boot_id=?2, last_rx=?3, last_tx=?4, total_rx=?5, total_tx=?6,
                            month_rx=?7, month_tx=?8, month_start=?9, day_rx=?10, day_tx=?11,
                            day_start=?12 WHERE node_id=?1",
            )?
            .execute(params![
                node_id, boot_id, rx, tx, total_rx, total_tx, month_rx, month_tx, period, day_rx, day_tx, day
            ])?;
        // A first reading establishes a baseline, not a measured zero. A later
        // zero delta is still an observation. Use the sampling date and hour
        // together; the mutable day counter above may clamp its own date. An
        // unseen reboot's counters are observed bytes since boot, and follow
        // the same arrival-period rule as other catch-up readings.
        if (unseen_boot
            || (!prev_boot.is_empty()
                && prev_boot == boot_id
                && ((rx >= last_rx && tx >= last_tx) || d_rx > 0 || d_tx > 0)))
            && date >= traffic_period::since(today, keep)
            && date <= today
        {
            traffic_period::record(&transaction, node_id, at, (d_rx, d_tx))?;
        }
        transaction.commit()?;
        Ok(Traffic { total_rx, total_tx, month_rx, month_tx, month_start: period, day_rx, day_tx })
    }

    /// Allows the panel to correct a total, for example after moving a node to
    /// new hardware.
    ///
    /// The corrected month figures are stamped with the current period; otherwise
    /// they would belong to whichever period the row still held, `all_traffic`
    /// would read them back as zero, and the node's next report would restart the
    /// counter and discard the correction.
    ///
    /// False when no node has this id.
    pub fn set_traffic(&self, node_id: i64, p: &TrafficPatch) -> Result<bool> {
        let conn = self.conn();
        let Some(reset_day): Option<u32> = conn
            .query_row("SELECT traffic_reset_day FROM node WHERE id=?1", [node_id], |r| r.get(0))
            .optional()?
        else {
            return Ok(false);
        };
        let period = period_start(Local::now().date_naive(), reset_day).to_string();
        conn.execute(
            "UPDATE traffic SET total_rx=COALESCE(?2,total_rx), total_tx=COALESCE(?3,total_tx),
                 month_rx=COALESCE(?4,CASE WHEN month_start=?6 THEN month_rx ELSE 0 END),
                 month_tx=COALESCE(?5,CASE WHEN month_start=?6 THEN month_tx ELSE 0 END), month_start=?6
             WHERE node_id=?1",
            params![node_id, p.total_rx, p.total_tx, p.month_rx, p.month_tx, period],
        )?;
        Ok(true)
    }

    pub fn traffic_periods(
        &self,
        node: i64,
        days: i64,
        now: DateTime<Local>,
    ) -> Result<traffic_period::Report> {
        let retention = self.retention_days();
        traffic_period::read(&self.reader(), node, days, retention, now)
    }

    // ---- metrics ----

    pub fn insert_metric(&self, node_id: i64, ts: i64, m: &serde_json::Value) -> Result<()> {
        let f = |k: &str| m.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
        let n = |k: &str| m.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        self.conn()
            .prepare_cached(
                "INSERT OR REPLACE INTO metric
               (node_id, ts, cpu, mem_used, swap_used, disk_used, net_rx, net_tx, tcp, udp, procs,
                net_rx_max, net_tx_max, cpu_max)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            )?
            .execute(params![
                node_id,
                ts,
                f("cpu"),
                n("mem_used"),
                n("swap_used"),
                n("disk_used"),
                n("net_rx"),
                n("net_tx"),
                n("tcp"),
                n("udp"),
                n("procs"),
                n("net_rx_max"),
                n("net_tx_max"),
                f("cpu_max")
            ])?;
        Ok(())
    }

    /// History for one node, one sample every `span.step` seconds.
    ///
    /// Bucketed rather than filtered on a multiple of `step`: rows normally land
    /// on the minute, but nothing enforces it, and a filter would return nothing
    /// for a stamp falling between grid lines.
    ///
    /// Averaged over the bucket rather than sampled from it. Keeping one row per
    /// bucket would reintroduce the 1/60 sampling the write side already rejects:
    /// the seven-day window integrated to 53.69 GB against the 27.52 GB the
    /// minutes hold, while averaging gives 28.02 GB, matching the accumulator.
    ///
    /// `swap_used`, `tcp`, `udp` and `procs` are returned although the official
    /// themes draw none of them from history: a theme the hub does not ship may.
    /// The counts are rounded, as in `fold_next`; a truncated mean would show
    /// a socket held half the bucket as none.
    ///
    /// The stamp is the bucket's start rather than a row inside it, so every
    /// series lands on one grid and the probe rows below can be shared.
    ///
    /// `net_rx_max`, `net_tx_max` and `cpu_max` are the bucket's highest rather
    /// than its mean, since a maximum of maxima loses nothing: a week's window
    /// peaks at the same rate as the minute that reached it. Each row counts as
    /// at least its own mean: rows predating the columns hold 0, and the mean,
    /// timed by the hub's arrivals rather than the agent's clock, can edge past
    /// the agent's own rates by the network's jitter.
    ///
    /// `minutes` is how many minute rows the bucket holds, against the
    /// `step / 60` it spans: a node offline for part of a bucket has its means
    /// taken over the minutes it reported, and a caller integrating the rates
    /// or showing availability needs the difference. That holds while the
    /// agent reports at least once a minute. A row is written only in a minute
    /// a report arrives, so past a 60-second `--interval` each row covers one
    /// interval and `minutes` falls in proportion: a fifth of the full count at
    /// 300 seconds.
    ///
    /// An hourly window reads the folded hours before the watermark and the
    /// minute rows after it, each hour weighted by the minutes it holds, so a
    /// bucket averages the same minutes it would have were they all still kept.
    /// The integer columns lose under one unit to the truncated hourly means.
    ///
    /// The minute rows read are the newest `DETAIL_DAYS` at most. Normally the
    /// watermark sits a few hours back, but it lags while a catch-up runs or a
    /// rollup keeps failing, and every minute row the node holds would then
    /// enter one request: 0.7–1.0 s per series at 90 days of 100 nodes during
    /// the catch-up after an upgrade, against 13–87 ms once it completes. The
    /// hours between the watermark and that week are missing from the chart
    /// until they are folded.
    pub fn metrics(&self, node_id: i64, span: Span) -> Result<Vec<serde_json::Value>> {
        let mut reader = self.reader();
        // One snapshot for every statement below. On the reader, a rollup can
        // otherwise commit between reading the watermark and reading the rows
        // on either side of it.
        let conn = reader.transaction()?;
        let row = |r: &rusqlite::Row<'_>| {
            Ok(serde_json::json!({
                "ts": r.get::<_, i64>(0)?, "cpu": r.get::<_, f64>(1)?,
                "mem_used": r.get::<_, i64>(2)?, "disk_used": r.get::<_, i64>(3)?,
                "net_rx": r.get::<_, i64>(4)?, "net_tx": r.get::<_, i64>(5)?,
                "net_rx_max": r.get::<_, i64>(6)?, "net_tx_max": r.get::<_, i64>(7)?,
                "cpu_max": r.get::<_, f64>(8)?, "minutes": r.get::<_, i64>(9)?,
                "swap_used": r.get::<_, i64>(10)?, "tcp": r.get::<_, i64>(11)?,
                "udp": r.get::<_, i64>(12)?, "procs": r.get::<_, i64>(13)?,
            }))
        };
        if !span.hourly {
            let mut stmt = conn.prepare_cached(
                "SELECT (MIN(ts)/?3)*?3, AVG(cpu), CAST(AVG(mem_used) AS INTEGER),
                        CAST(AVG(disk_used) AS INTEGER),
                        CAST(AVG(net_rx) AS INTEGER), CAST(AVG(net_tx) AS INTEGER),
                        MAX(MAX(net_rx, net_rx_max)), MAX(MAX(net_tx, net_tx_max)),
                        MAX(MAX(cpu, cpu_max)), COUNT(*), CAST(AVG(swap_used) AS INTEGER),
                        CAST(ROUND(AVG(tcp)) AS INTEGER), CAST(ROUND(AVG(udp)) AS INTEGER),
                        CAST(ROUND(AVG(procs)) AS INTEGER)
                 FROM metric WHERE node_id=?1 AND ts>=?2 GROUP BY ts/?3 ORDER BY ts/?3",
            )?;
            let rows = stmt.query_map(params![node_id, span.since, span.step], row)?;
            return Ok(rows.collect::<Result<_, _>>()?);
        }
        let rolled = rolled(&conn)?.unwrap_or(i64::MIN);
        let mut stmt = conn.prepare_cached(
            "SELECT (MIN(ts)/?3)*?3, SUM(cpu*w)/SUM(w), SUM(mem_used*w)/SUM(w), SUM(disk_used*w)/SUM(w),
                    SUM(net_rx*w)/SUM(w), SUM(net_tx*w)/SUM(w), MAX(rx_max), MAX(tx_max), MAX(cpu_top), SUM(w),
                    SUM(swap_used*w)/SUM(w), CAST(ROUND(SUM(tcp*w)*1.0/SUM(w)) AS INTEGER),
                    CAST(ROUND(SUM(udp*w)*1.0/SUM(w)) AS INTEGER),
                    CAST(ROUND(SUM(procs*w)*1.0/SUM(w)) AS INTEGER)
             FROM (SELECT ts, minutes AS w, cpu, mem_used, disk_used, net_rx, net_tx,
                          net_rx_max AS rx_max, net_tx_max AS tx_max, cpu_max AS cpu_top,
                          swap_used, tcp, udp, procs
                   FROM metric_hour WHERE node_id=?1 AND ts>=?2 AND ts<?4
                   UNION ALL
                   SELECT ts, 1, cpu, mem_used, disk_used, net_rx, net_tx,
                          MAX(net_rx, net_rx_max), MAX(net_tx, net_tx_max), MAX(cpu, cpu_max),
                          swap_used, tcp, udp, procs
                   FROM metric WHERE node_id=?1
                        AND ts>=MAX(?2, ?4, (SELECT MAX(ts) FROM metric WHERE node_id=?1) - ?5))
             GROUP BY ts/?3 ORDER BY ts/?3",
        )?;
        let rows =
            stmt.query_map(params![node_id, span.since, span.step, rolled, DETAIL_DAYS * 86_400], row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---- the hourly tier ----

    /// Folds every hour whose rows are complete into the hourly tier and
    /// returns how many were folded.
    ///
    /// One hour per transaction, so a long catch-up -- the first run after an
    /// upgrade folds every hour still held in minute rows -- lets the agents'
    /// writes through between hours. At 100 nodes and four probes an hour takes
    /// about 15 ms.
    ///
    /// Without a watermark it records one at the hour of the oldest minute row,
    /// or with none at all at the current hour, so that `prune` has a watermark
    /// to respect. Hours before `keep_days` are skipped rather than folded, as
    /// `prune` would drop them next.
    pub fn roll_up(&self, now: i64, keep_days: i64) -> Result<usize> {
        {
            let conn = self.conn();
            if rolled(&conn)?.is_none() {
                let from = oldest(&conn, &["metric", "ping_record"])?.unwrap_or(now);
                conn.execute(
                    "INSERT OR REPLACE INTO setting (key, value) VALUES (?1, ?2)",
                    params![ROLLED, (from.div_euclid(3_600) * 3_600).to_string()],
                )?;
            }
        }
        let floor = (now - keep_days * 86_400).div_euclid(3_600) * 3_600;
        let mut folded = 0;
        while !halted() && self.fold_next(floor, now - 3_600 - LATE)? {
            let_waiters_in();
            folded += 1;
        }
        Ok(folded)
    }

    /// Folds the hour at the watermark, or at `floor` when the watermark is
    /// older, into `metric_hour` and `ping_hour` and moves the watermark past
    /// it, in one transaction: a failure leaves the hour to be folded again
    /// rather than half folded. `INSERT OR REPLACE` makes a second fold of the
    /// same hour a rewrite. False, folding nothing, once the hour is past `last`
    /// or when there is no watermark.
    ///
    /// The watermark is read here rather than carried between hours: a restore
    /// replaces it along with the rows it describes, and a stale one would skip
    /// the restored hours.
    ///
    /// By node, as the keys begin with it; `ts` alone would scan each table.
    /// The probe results are held one node's hour at a time, and only for
    /// probes that still exist: `ping_hour` refers to `ping_task`, and one row
    /// left behind by a deleted probe would otherwise fail the hour on every
    /// pass, keeping its minute rows from ever being pruned.
    ///
    /// The byte columns truncate their means, a loss under one byte; the counts
    /// round theirs, as truncating would drop a UDP socket held for half the
    /// hour.
    fn fold_next(&self, floor: i64, last: i64) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let Some(hour) = rolled(&tx)?.map(|h| h.max(floor)).filter(|&h| h <= last) else {
            return Ok(false);
        };
        tx.prepare_cached(
            "INSERT OR REPLACE INTO metric_hour
               (node_id, ts, minutes, cpu, cpu_max, mem_used, swap_used, disk_used, net_rx, net_tx,
                net_rx_max, net_tx_max, tcp, udp, procs)
             SELECT node_id, ?1, COUNT(*), AVG(cpu), MAX(MAX(cpu, cpu_max)), CAST(AVG(mem_used) AS INTEGER),
                    CAST(AVG(swap_used) AS INTEGER), CAST(AVG(disk_used) AS INTEGER),
                    CAST(AVG(net_rx) AS INTEGER), CAST(AVG(net_tx) AS INTEGER),
                    MAX(MAX(net_rx, net_rx_max)), MAX(MAX(net_tx, net_tx_max)),
                    CAST(ROUND(AVG(tcp)) AS INTEGER), CAST(ROUND(AVG(udp)) AS INTEGER),
                    CAST(ROUND(AVG(procs)) AS INTEGER)
             FROM metric WHERE node_id IN (SELECT id FROM node) AND ts>=?1 AND ts<?1+3600
             GROUP BY node_id",
        )?
        .execute([hour])?;
        let nodes: Vec<i64> = tx
            .prepare_cached("SELECT id FROM node")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for node in nodes {
            let mut probes: std::collections::BTreeMap<i64, Tally> = Default::default();
            let mut read = tx.prepare_cached(
                "SELECT task_id, latency FROM ping_record
                 WHERE node_id=?1 AND ts>=?2 AND ts<?2+3600 AND task_id IN (SELECT id FROM ping_task)",
            )?;
            let mut rows = read.query(params![node, hour])?;
            while let Some(r) = rows.next()? {
                probes.entry(r.get(0)?).or_default().add(Sample::result(r.get(1)?));
            }
            let mut write = tx.prepare_cached(
                "INSERT OR REPLACE INTO ping_hour (node_id, task_id, ts, answered, lost, latency, lo, hi)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (task, mut t) in probes {
                let median = t.median();
                write.execute(params![node, task, hour, t.answered(), t.lost, median, t.lo, t.hi])?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO setting (key, value) VALUES (?1, ?2)",
            params![ROLLED, (hour + 3_600).to_string()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Drops history beyond the retention window: minute rows past
    /// `DETAIL_DAYS`, or the window itself when shorter, and hourly rows past
    /// `keep_days`. Traffic totals live in their own table precisely so history
    /// can be pruned freely.
    ///
    /// A minute row outlives its window until its hour is folded, so a rollup
    /// that has fallen behind costs disk rather than history.
    ///
    /// Per node, as the keys begin with `node_id`: `ts < ?` alone scans the whole
    /// table, 25 s at 90 days of 100 nodes against 86 ms by seek. The node table
    /// lists every node with rows: `metric` and the hourly tables cascade from
    /// it, `delete_node` clears `ping_record`, and `insert_pings` writes only for
    /// probes assigned to an existing node.
    ///
    /// Minute rows are deleted at most one node's day per statement, with the
    /// lock the agents write through released in between. An hourly pass
    /// deletes an hour and is one statement per table; the first pass after an
    /// upgrade deletes everything past the week, which in one statement would
    /// hold the lock for 37.8 s at 90 days of 100 nodes, and a node at a time
    /// 0.9 s each.
    ///
    /// Hourly rows are deleted a node at a time, one statement per table:
    /// lowering the window from a year to a month deletes 8,040 and 32,160 rows
    /// of a node with four probes, 24 ms each. A day per statement would spend
    /// 0.96 s per node on the lookups and pauses in between, 4.8 minutes at 300
    /// nodes ahead of the `VACUUM` that usually follows.
    ///
    /// The watermark is read with each statement: a restore can replace it,
    /// and the minute rows it guards, between two of them.
    pub fn prune(&self, keep_days: i64) -> Result<usize> {
        let now = Utc::now().timestamp();
        let period_since = traffic_period::since(Local::now().date_naive(), keep_days);
        let nodes: Vec<i64> = self
            .conn()
            .prepare_cached("SELECT id FROM node")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let minutes = now - keep_days.min(DETAIL_DAYS) * 86_400;
        let hours = now - keep_days * 86_400;
        let mut pruned = 0;
        for id in nodes {
            for (table, cutoff, guarded) in [
                ("metric", minutes, true),
                ("ping_record", minutes, true),
                ("metric_hour", hours, false),
                ("ping_hour", hours, false),
            ] {
                loop {
                    if halted() {
                        return Ok(pruned);
                    }
                    let conn = self.conn();
                    let before =
                        if guarded { cutoff.min(rolled(&conn)?.unwrap_or(i64::MIN)) } else { cutoff };
                    let first: Option<i64> = conn
                        .prepare_cached(&format!("SELECT MIN(ts) FROM {table} WHERE node_id=?1"))?
                        .query_row([id], |r| r.get(0))?;
                    let Some(first) = first.filter(|&ts| ts < before) else { break };
                    pruned += conn
                        .prepare_cached(&format!("DELETE FROM {table} WHERE node_id=?1 AND ts<?2"))?
                        .execute(params![id, if guarded { before.min(first + 86_400) } else { before }])?;
                    drop(conn);
                    let_waiters_in();
                }
            }
            if !halted() {
                pruned += traffic_period::prune(&self.conn(), id, period_since)?;
            }
        }
        Ok(pruned)
    }

    // ---- ping ----

    pub fn ping_tasks(&self) -> Result<Vec<PingTask>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT id, name, target, interval, auto_join FROM ping_task ORDER BY sort, id")?;
        let tasks: Vec<PingTask> = stmt
            .query_map([], |r| {
                Ok(PingTask {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    target: r.get(2)?,
                    interval: r.get(3)?,
                    nodes: Vec::new(),
                    auto_join: r.get(4)?,
                    base: None,
                })
            })?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        let mut stmt = conn.prepare("SELECT node_id FROM ping_node WHERE task_id=?1")?;
        tasks
            .into_iter()
            .map(|mut t| {
                t.nodes = stmt.query_map([t.id], |r| r.get(0))?.collect::<Result<_, _>>()?;
                Ok(t)
            })
            .collect()
    }

    /// The maximum number of probes one node may be assigned.
    ///
    /// The agent enforces the same limit: `MAX_PING_TASKS` in that repository
    /// caps the list it will run, since a compromised or buggy hub could
    /// otherwise ask a node for hundreds of outbound connects per second. That
    /// cap is a defence and remains, but on its own it truncates silently,
    /// leaving one line in the node's journal while the hub continues pushing
    /// probes that never run and drawing charts that stay empty.
    ///
    /// The hub knows the total, so the hub issues the refusal. The two must stay
    /// in step; the agent's copy is the backstop rather than the message.
    pub const MAX_PROBES_PER_NODE: i64 = 64;

    /// The shortest probe interval in seconds. The agent clamps to the same
    /// floor, so together with [`Self::MAX_PROBES_PER_NODE`] it bounds how many
    /// results an honest node can send.
    pub const MIN_PROBE_INTERVAL: i64 = 5;

    /// Replaces the assignments wholesale, or with `base` applies only what
    /// changed from it. Either way in one transaction: failing between the
    /// deletes and the inserts would unassign nodes from a probe the panel still
    /// lists them under.
    pub fn save_ping_task(&self, t: &PingTask) -> Result<i64> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let id = if t.id > 0 {
            let updated = tx.execute(
                "UPDATE ping_task SET name=?2, target=?3, interval=?4, auto_join=?5 WHERE id=?1",
                params![t.id, t.name, t.target, t.interval, t.auto_join],
            )?;
            // Deleted from another session. Without this the first assignment
            // would fail on the task's foreign key and be reported against a
            // node, or, with none, the save would report success.
            if updated == 0 {
                refuse!("监控不存在，可能已被删除");
            }
            t.id
        } else {
            // At the end, as in `create_node`.
            let id = next_id(&tx, "ping_task")?;
            tx.execute(
                "INSERT INTO ping_task (id, name, target, interval, auto_join, sort)
                 VALUES (?5,?1,?2,?3,?4,(SELECT COALESCE(MAX(sort),-1)+1 FROM ping_task))",
                params![t.name, t.target, t.interval, t.auto_join, id],
            )?;
            id
        };
        let added: Vec<i64> = match &t.base {
            Some(base) if t.id > 0 => {
                for node in base.iter().filter(|n| !t.nodes.contains(n)) {
                    tx.execute("DELETE FROM ping_node WHERE task_id=?1 AND node_id=?2", params![id, node])?;
                }
                t.nodes.iter().filter(|n| !base.contains(n)).copied().collect()
            }
            _ => {
                tx.execute("DELETE FROM ping_node WHERE task_id=?1", [id])?;
                t.nodes.clone()
            }
        };
        for node in &added {
            // OR IGNORE covers a node that joined while the editor was open and
            // was then ticked. It does not cover the foreign key, which remains
            // the check; naming the node turns SQLite's "FOREIGN KEY constraint
            // failed" into something the panel can show.
            tx.execute(
                "INSERT OR IGNORE INTO ping_node (task_id, node_id) VALUES (?1,?2)",
                params![id, node],
            )
            .map_err(|e| match e.sqlite_error_code() {
                Some(rusqlite::ErrorCode::ConstraintViolation) => {
                    anyhow::Error::from(e).context(crate::Shown(format!("节点 {node} 不存在，可能已被删除")))
                }
                _ => e.into(),
            })?;
        }
        Self::refuse_crowded(&tx, None)?;
        // The next node created receives every auto-joining probe at once.
        let joining: i64 =
            tx.query_row("SELECT COUNT(*) FROM ping_task WHERE auto_join", [], |r| r.get(0))?;
        if joining > Self::MAX_PROBES_PER_NODE {
            refuse!("新节点会自动加入 {joining} 个探测任务，agent 最多只跑 {} 个", Self::MAX_PROBES_PER_NODE);
        }
        tx.commit()?;
        Ok(id)
    }

    /// Refuses a write that leaves some node over [`Self::MAX_PROBES_PER_NODE`],
    /// or `node` alone when the write touched only that node's rows: another
    /// node already over the limit is not this edit's to fix.
    ///
    /// Queried from the table after the rows are in rather than counted from the
    /// request: an update changes the caller's own assignments, so arithmetic on
    /// the way in would have to subtract them again. Run inside the write's
    /// transaction, so refusing rolls it back. By name: the panel identifies
    /// nodes by name and never shows an id.
    fn refuse_crowded(tx: &rusqlite::Transaction, node: Option<i64>) -> Result<()> {
        let crowded: Option<String> = tx
            .query_row(
                "SELECT n.name FROM ping_node p JOIN node n ON n.id = p.node_id
                 WHERE ?2 IS NULL OR p.node_id = ?2
                 GROUP BY p.node_id HAVING COUNT(*) > ?1 LIMIT 1",
                params![Self::MAX_PROBES_PER_NODE, node],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(node) = crowded {
            refuse!(
                "节点「{node}」会被分配超过 {} 个探测任务，agent 最多只跑这么多，多出来的会被静默丢掉",
                Self::MAX_PROBES_PER_NODE
            );
        }
        Ok(())
    }

    /// One node's probes, edited from the node's side: `save_ping_task` with
    /// the roles swapped. Only the change from `base` is applied, as there, so a
    /// probe assigned to this node elsewhere while the editor was open keeps the
    /// assignment. `false` when the node does not exist.
    pub fn set_node_ping_tasks(&self, node: i64, tasks: &[i64], base: &[i64]) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if tx.query_row("SELECT 1 FROM node WHERE id=?1", [node], |_| Ok(())).optional()?.is_none() {
            return Ok(false);
        }
        for task in base.iter().filter(|t| !tasks.contains(t)) {
            tx.execute("DELETE FROM ping_node WHERE task_id=?1 AND node_id=?2", params![task, node])?;
        }
        for task in tasks.iter().filter(|t| !base.contains(t)) {
            // The node was checked above, so a key failing here is the probe.
            tx.execute(
                "INSERT OR IGNORE INTO ping_node (task_id, node_id) VALUES (?1,?2)",
                params![task, node],
            )
            .map_err(|e| match e.sqlite_error_code() {
                Some(rusqlite::ErrorCode::ConstraintViolation) => {
                    anyhow::Error::from(e).context(crate::Shown("有监控已被删除，刷新后重试".into()))
                }
                _ => e.into(),
            })?;
        }
        Self::refuse_crowded(&tx, Some(node))?;
        tx.commit()?;
        Ok(true)
    }

    /// Deletes a probe and the results filed under it.
    ///
    /// `ping_record` carries no foreign key, so it is cleared explicitly, as in
    /// `delete_node`; `ping_hour` cascades.
    ///
    /// Each delete is a scan -- the keys begin at `node_id` -- bounded by the
    /// week of minute rows and the hourly tier, for an action taken manually a
    /// few times a year.
    pub fn delete_ping_task(&self, id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM ping_record WHERE task_id = ?1", [id])?;
        conn.execute("DELETE FROM ping_task WHERE id=?1", [id])?;
        Ok(())
    }

    /// The task list pushed to one agent.
    ///
    /// Ordered, because the agent keeps the first [`Self::MAX_PROBES_PER_NODE`]
    /// as its backstop against a hub requesting hundreds. Unordered, a list at
    /// that boundary could yield a different subset on each push, restarting half
    /// the timers each time; `refuse_crowded` prevents reaching that boundary,
    /// and this makes the backstop deterministic should a database arrive there
    /// by another route.
    ///
    /// By id rather than the panel's `sort`, so reordering in the panel changes
    /// nothing an agent holds and needs no push.
    pub fn ping_tasks_for(&self, node_id: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT t.id, t.target, t.interval FROM ping_task t
             JOIN ping_node n ON n.task_id = t.id WHERE n.node_id = ?1 ORDER BY t.id",
        )?;
        let rows = stmt.query_map([node_id], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?, "target": r.get::<_, String>(1)?,
                "interval": r.get::<_, i64>(2)?
            }))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Probe names keyed by id, for labelling one node's latency chart. Names
    /// only: targets and node assignments remain behind `Admin`.
    ///
    /// Restricted to the probes assigned to that node, the only ones its chart
    /// has samples to label. A probe name is operator-supplied text that
    /// routinely carries a hostname or a customer, and the rest of the table
    /// belongs to nodes this caller may not be able to see.
    pub fn ping_task_names(&self, node_id: i64) -> Result<serde_json::Value> {
        let conn = self.reader();
        let mut stmt = conn.prepare(
            "SELECT id, name FROM ping_task WHERE id IN (SELECT task_id FROM ping_node WHERE node_id=?1)",
        )?;
        let rows = stmt.query_map([node_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut names = serde_json::Map::new();
        for row in rows {
            let (id, name) = row?;
            names.insert(id.to_string(), serde_json::json!(name));
        }
        Ok(serde_json::Value::Object(names))
    }

    /// Files a node's probe results as `(task_id, ts, latency)`, each only under a
    /// probe this node is assigned. A result for anything else is dropped rather
    /// than treated as an error, since the agent can do nothing useful with the
    /// distinction.
    ///
    /// One transaction for the batch: the session gathers a minute of results and
    /// files them together, since each commit writes at least one page.
    ///
    /// The assignment is tested inside the statement because that is the only
    /// place it is atomic with the write: `ping_record` carries no foreign key,
    /// being WITHOUT ROWID and keyed for the chart query. Two cases arrive
    /// without an assignment. A result already in flight when the panel deleted
    /// its probe, which would otherwise land after `delete_ping_task` swept the
    /// history and stay behind under an id no probe holds.
    /// And a node token in the wrong hands: `task_id` is chosen by the reporter,
    /// so without the test the rows it could create would be unbounded.
    ///
    /// The chart's `task_id IN (assignments)` filter hides both afterwards, but
    /// does not prevent the write or its storage.
    pub fn insert_pings(&self, node_id: i64, results: &[(i64, i64, i64)]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut insert = tx.prepare_cached(
                "INSERT OR REPLACE INTO ping_record (node_id, task_id, ts, latency)
                 SELECT ?1, ?2, ?3, ?4
                 WHERE EXISTS (SELECT 1 FROM ping_node WHERE task_id = ?2 AND node_id = ?1)",
            )?;
            for (task_id, ts, latency) in results {
                insert.execute(params![node_id, task_id, ts, latency])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// One node's probe results since `since`, as `(task_id, ts, latency)`
    /// oldest first: the samples the latency alert judges.
    pub fn recent_pings(&self, node_id: i64, since: i64) -> Result<Vec<(i64, i64, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT task_id, ts, latency FROM ping_record WHERE node_id=?1 AND ts>=?2 ORDER BY ts",
        )?;
        let rows = stmt.query_map(params![node_id, since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// One node's hourly median round trips since `since`, as `(task_id, ts,
    /// latency, percent lost)`, hours with no answer left out: the latency
    /// alert's baseline.
    pub fn hourly_pings(&self, node_id: i64, since: i64) -> Result<Vec<(i64, i64, i64, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT task_id, ts, latency, lost * 100 / (answered + lost) FROM ping_hour
             WHERE node_id=?1 AND ts>=?2 AND answered>0 AND latency IS NOT NULL",
        )?;
        let rows =
            stmt.query_map(params![node_id, since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Probe results for one node, one sample per probe per `step` seconds: the
    /// bucket's median round trip, its range, and the proportion lost.
    ///
    /// These stamps fall wherever the probe finished rather than on a minute, so
    /// the thinning buckets them instead of matching a multiple, as in `metrics`
    /// above. This is the larger half of that response, since a probe reports far
    /// more often than once a minute.
    ///
    /// [`PING_ROWS`] returns rows in time order, so a bucket is complete the
    /// moment the next opens and only one is held at a time -- at most the probes
    /// assigned to the node times the results one bucket spans.
    ///
    /// Returns the buckets and, alongside them, the proportion of the whole
    /// window each probe lost. The latter cannot be recovered from the former:
    /// [`close_bucket`] divides within each bucket and keeps only the quotient,
    /// so averaging those percentages would weight a bucket holding one sample
    /// equally with one holding twelve. The buckets are necessarily unequal --
    /// the window's first and last are partial by construction, and a probe that
    /// starts, stops, loses its node or skips a round produces more. The
    /// denominators are available only here, in the pass that already reads every
    /// row. Probes that lost nothing are omitted, as `loss` is per bucket.
    ///
    /// An hourly window reads [`PING_HOURS`] up to the watermark and
    /// [`PING_ROWS`] after it. Each hourly row enters the fold as its hour's
    /// answers and losses, so the loss figures and the range stay exact and the
    /// median is weighted as [`Tally::median`] describes. The minute rows are
    /// bounded as in [`Db::metrics`].
    pub fn ping_records(
        &self,
        node_id: i64,
        span: Span,
    ) -> Result<(Vec<serde_json::Value>, serde_json::Value)> {
        let mut reader = self.reader();
        // One snapshot for every statement below, as in `metrics`.
        let conn = reader.transaction()?;
        let step = span.step;
        let mut out = Vec::new();
        // Per probe in the bucket being filled.
        let mut open: Vec<(i64, Tally)> = Vec::new();
        // Per probe across the whole window: how many were lost, out of how many.
        // Folded in the same pass rather than queried from SQLite a second time,
        // for the same reason the bucket fold itself is in Rust.
        let mut totals: HashMap<i64, (i64, i64)> = HashMap::new();
        let mut bucket = 0;
        let mut feed = |b: i64, task: i64, sample: Sample| {
            if b != bucket {
                close_bucket(&mut out, &mut open, bucket * step);
                bucket = b;
            }
            let seen = totals.entry(task).or_insert((0, 0));
            seen.0 += sample.lost;
            seen.1 += sample.answered + sample.lost;
            match open.iter_mut().find(|(id, _)| *id == task) {
                Some((_, tally)) => tally.add(sample),
                None => {
                    let mut tally = Tally::default();
                    tally.add(sample);
                    open.push((task, tally));
                }
            }
        };
        // The folded hours first, then the minute rows after them: both come off
        // their keys in time order, and a bucket spanning the watermark takes
        // rows from each.
        let mut minutes_from = span.since;
        if span.hourly {
            let rolled = rolled(&conn)?.unwrap_or(i64::MIN);
            let newest: Option<i64> = conn
                .prepare_cached("SELECT MAX(ts) FROM ping_record WHERE node_id=?1")?
                .query_row([node_id], |r| r.get(0))?;
            minutes_from = minutes_from.max(rolled).max(newest.unwrap_or(0) - DETAIL_DAYS * 86_400);
            let mut stmt = conn.prepare_cached(PING_HOURS)?;
            let mut rows = stmt.query(params![node_id, span.since, step, rolled])?;
            while let Some(r) = rows.next()? {
                let sample = Sample {
                    answered: r.get(2)?,
                    lost: r.get(3)?,
                    median: r.get(4)?,
                    lo: r.get(5)?,
                    hi: r.get(6)?,
                };
                feed(r.get(0)?, r.get(1)?, sample);
            }
        }
        let mut stmt = conn.prepare_cached(PING_ROWS)?;
        let mut rows = stmt.query(params![node_id, minutes_from, step])?;
        while let Some(r) = rows.next()? {
            feed(r.get(0)?, r.get(1)?, Sample::result(r.get(2)?));
        }
        close_bucket(&mut out, &mut open, bucket * step);
        // Probe by probe in the panel's order, each probe's rows still in time
        // order. Themes take their series, colours and legend from the order in
        // which probes first appear; bucket by bucket, that would be whichever
        // probe happened to answer inside the window's partial first bucket.
        let rank: HashMap<i64, usize> = conn
            .prepare_cached("SELECT id FROM ping_task ORDER BY sort, id")?
            .query_map([], |r| r.get(0))?
            .enumerate()
            .map(|(i, id)| id.map(|id| (id, i)))
            .collect::<Result<_, _>>()?;
        // Sorted after releasing the reader, which the next chart request waits
        // on. A probe missing from the rank, which the assignment filter in
        // `PING_ROWS` rules out today, goes last rather than taking the first
        // colour.
        drop(rows);
        drop(stmt);
        drop(conn);
        drop(reader);
        out.sort_by_cached_key(|row| {
            row["task_id"].as_i64().and_then(|id| rank.get(&id).copied()).unwrap_or(usize::MAX)
        });
        // Unrounded: the caller decides how to render it, and rounding here would
        // turn 0.14% into the 0% that denotes no loss at all.
        let loss: serde_json::Map<String, serde_json::Value> = totals
            .into_iter()
            .filter(|(_, (lost, _))| *lost > 0)
            .map(|(task, (lost, samples))| {
                (task.to_string(), serde_json::json!(100.0 * lost as f64 / samples as f64))
            })
            .collect();
        Ok((out, serde_json::Value::Object(loss)))
    }
}

/// Turns one finished bucket into a row per probe, stamped with the bucket's
/// start so every series lands on the same grid.
///
/// Median rather than mean: one SYN retransmit is tens of milliseconds and would
/// drag a mean, and it is the reading that is wrong rather than the link.
///
/// `latency` is null when an entire bucket timed out. `loss` is the percentage
/// that did, included only when non-zero -- a healthy day is 2,880 rows, and
/// `"loss":0` on each would add 29 kB of nothing. Rounded up, so that the absence
/// of a `loss` key means no timeouts occurred: truncating would report a bucket
/// that lost 1 of 180 as clean.
fn close_bucket(out: &mut Vec<serde_json::Value>, open: &mut Vec<(i64, Tally)>, ts: i64) {
    for (task, mut tally) in open.drain(..) {
        let answered = tally.answered();
        let mut row = serde_json::json!({"task_id": task, "ts": ts, "latency": tally.median()});
        // Only when the bucket actually varied. At the hour and six-hour windows a
        // bucket holds one sample, and a band would be a zero-height ribbon under
        // every line.
        if let (Some(lo), Some(hi)) = (tally.lo, tally.hi) {
            if hi > lo {
                row["band"] = serde_json::json!([lo, hi]);
            }
        }
        if tally.lost > 0 {
            let total = answered + tally.lost;
            row["loss"] = ((100 * tally.lost + total - 1) / total).into();
        }
        out.push(row);
    }
}

/// Start of the billing period containing `today`, given a reset day of month.
/// A reset day past the end of a short month lands on that month's last day.
pub fn period_start(today: NaiveDate, reset_day: u32) -> NaiveDate {
    let day = reset_day.clamp(1, 31);
    let clamped = |y: i32, m: u32| {
        let last =
            NaiveDate::from_ymd_opt(if m == 12 { y + 1 } else { y }, if m == 12 { 1 } else { m + 1 }, 1)
                .unwrap()
                .pred_opt()
                .unwrap()
                .day();
        NaiveDate::from_ymd_opt(y, m, day.min(last)).unwrap()
    };
    let this = clamped(today.year(), today.month());
    if today >= this {
        this
    } else if today.month() == 1 {
        clamped(today.year() - 1, 12)
    } else {
        clamped(today.year(), today.month() - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_hours_median_stands_for_each_of_its_answers() {
        // One hour of a single 10 ms answer and one of three at 20 ms are four
        // answers, median 20.
        let mut t = Tally::default();
        t.add(Sample { answered: 1, lost: 0, median: Some(10), lo: Some(10), hi: Some(10) });
        t.add(Sample { answered: 3, lost: 2, median: Some(20), lo: Some(15), hi: Some(40) });
        assert_eq!((t.median(), t.answered(), t.lo, t.hi), (Some(20), 4, Some(10), Some(40)));
    }
}
