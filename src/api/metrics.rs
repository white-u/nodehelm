//! Charts, four-period traffic history, the live stream and latency probes.

use super::*;

#[derive(Deserialize)]
pub struct Window {
    #[serde(default = "default_hours")]
    pub(super) hours: i64,
    /// How many points the caller can draw. Absent means the full budget.
    pub(super) points: Option<i64>,
    /// Which half the caller will draw, `metrics` or `ping`. Each tab draws one,
    /// and the other accounted for a third to two thirds of every response. Absent
    /// means both.
    pub(super) series: Option<String>,
}

fn default_hours() -> i64 {
    6
}

#[derive(Deserialize)]
pub struct PeriodWindow {
    #[serde(default = "default_period_days")]
    pub(super) days: i64,
}

fn default_period_days() -> i64 {
    7
}

pub async fn traffic_periods(
    _: Admin,
    State(app): State<Shared>,
    Path(id): Path<i64>,
    query: Result<Query<PeriodWindow>, QueryRejection>,
) -> Response {
    let Ok(Query(window)) = query else { return bad("流量历史天数格式不对") };
    if !(1..=db::MAX_RETENTION_DAYS).contains(&window.days) {
        return bad("流量历史天数要在 1 到 365 之间");
    }
    match app.db.node(id) {
        Ok(Some(_)) => {}
        Ok(None) => return no_such_node(),
        Err(e) => return fail(e),
    }
    let Ok(_permit) = HISTORY_GATE.try_acquire() else {
        return answer(StatusCode::SERVICE_UNAVAILABLE, "查询历史的请求太多，稍后再试");
    };
    let now = Local::now();
    let report = tokio::task::spawn_blocking(move || app.db.traffic_periods(id, window.days, now)).await;
    match report.map_err(anyhow::Error::from).and_then(|r| r) {
        Ok(report) => ([(header::CACHE_CONTROL, "no-store, no-transform")], Json(report)).into_response(),
        Err(e) => fail(e),
    }
}

/// How many history windows are built concurrently.
///
/// `span` bounds what one request costs; this bounds how many may run,
/// closing the same gap `auth::PASSWORD_GATE` closes on
/// the other path an anonymous caller can make expensive. This is the more
/// expensive of the two: a week of probe results is a scan of 98 ms at four
/// 60-second probes and 486 ms at eight 10-second ones, growing with the probes
/// the admin configured rather than with anything the caller sends.
///
/// The scans run on the database's read-only connection, so the agents' writes
/// do not wait on them, and they serialise on that one connection instead. Four,
/// because a fifth in flight buys no throughput and only lengthens the wait
/// behind the others: what the number sets is how long a chart can wait, four
/// scans of the slower kind being 2 s, while leaving room for several people
/// opening charts simultaneously.
///
/// Refused rather than queued, as in `auth`: a queue admits the same flood,
/// merely later, and each request waiting in it holds a blocking thread.
///
/// **This gate is ineffective without the `spawn_blocking` below.** The body of
/// this handler never awaits, so a permit taken and dropped within it is held
/// only while a worker thread is actually running the handler -- at most one per
/// worker, three on this hub. Measured at eight: 120 concurrent requests, zero
/// refusals, the panel still at 14 s. This is the same constraint
/// `PASSWORD_CHECKS` is sized against, approached from the other side: not a
/// value too high for the machine, but a handler that cannot hold more permits
/// than the machine has threads. Moving the scan off the runtime is what makes
/// "in flight" meaningful, and is what every other heavy query here already
/// does.
pub(super) const HISTORY_SLOTS: usize = 4;
pub(super) static HISTORY_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(HISTORY_SLOTS);

pub async fn metrics(
    State(app): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(w): Query<Window>,
) -> Response {
    let full = authed(&app, &headers);
    if !readable(&app, full, id) {
        return answer(StatusCode::UNAUTHORIZED, "需要登录后查看");
    }
    // After the two point lookups above, so an unauthorised caller is told so
    // rather than asked to retry later.
    let Ok(_permit) = HISTORY_GATE.try_acquire() else {
        return answer(StatusCode::SERVICE_UNAVAILABLE, "查询历史的请求太多，稍后再试");
    };
    let hours = w.hours.clamp(1, app.db.retention_days() * 24);
    let span = span(hours, w.points, Utc::now().timestamp());
    let wants = |name: &str| w.series.as_deref().is_none_or(|s| s == name);
    let (want_metrics, want_ping) = (wants("metrics"), wants("ping"));
    // Off the runtime: this reads every probe result the node has retained
    // within the window, and a worker thread blocked on that scan, or on the
    // reader while another request holds it, serves nothing else.
    //
    // It is also what makes the gate above effective: the permit is held across
    // an await, so exactly four callers are inside it at once rather than however
    // many worker threads happen to exist.
    let built = tokio::task::spawn_blocking(move || {
        // Probe names accompany the samples they label, so the page needs no
        // second request. Names only: targets and assignments remain behind
        // `Admin`. Skipped when probes were not requested, since the resources tab
        // has nothing to label and this costs a turn at the reader.
        let probes =
            if want_ping { app.db.ping_task_names(id).unwrap_or_else(|_| json!({})) } else { json!({}) };
        let metrics = if want_metrics { app.db.metrics(id, span)? } else { vec![] };
        // `loss` is per probe across the whole window, alongside the per-bucket
        // `loss` on the rows. Both are required and neither replaces the other:
        // the row figure is what a tooltip reads, while the window figure is the
        // only one that can be accurate, since the denominators it divides by are
        // gone by the time the rows are built. Additive, so a theme unaware of it
        // continues to work.
        let (ping, loss) = if want_ping { app.db.ping_records(id, span)? } else { (vec![], json!({})) };
        // `step` is the seconds each point spans, against which a row's `minutes`
        // is read; inferred from the stamps instead, it would be missing for a
        // window holding a single point. The last point is the bucket still in
        // progress and holds only the minutes elapsed.
        anyhow::Ok(json!({
            "metrics": metrics, "ping": ping, "probes": probes, "loss": loss, "step": span.step,
        }))
    })
    .await;
    match built.map_err(|e| anyhow::anyhow!(e)).and_then(|r| r) {
        Ok(body) => Json(body).into_response(),
        Err(e) => fail(e),
    }
}

/// The window a chart request is answered with: `hours` back from `now`,
/// widened to the point boundary at or before that.
///
/// The width is capped by the retention window alone, the same for every
/// caller, because no width costs more than the week: up to `DETAIL_DAYS` a
/// request reads minute rows, 10,080 per series at most, and past it hourly
/// rows, 8,760 per series at a year, with the minute rows not yet folded, which
/// `Db::metrics` bounds to the newest week. The scan is what a request costs --
/// the thinning below bounds the response, not the rows read -- so a width
/// reading more than the week would need a lower ceiling for anonymous callers.
///
/// Thinning exists for what the screen cannot draw rather than as a convention:
/// where the samples fit, every one is sent. A chart of a hundred points reads
/// as a hundred samples taken, which for a probe is a claim about the network.
/// Whole minutes, matching the grid the metric rows sit on, and whole hours
/// past the week, so that no hourly row straddles two points.
///
/// `points` is what the caller reports it can draw, and can only lower the
/// budget: `SAMPLES` is the hub's ceiling rather than the caller's, set at a day
/// of minutes so the day's probe window returns intact.
// ponytail: the budget is per series, so a response is SAMPLES × (1 + probes) --
// bounded by how many probes the admin created, not by the caller. Four probes
// at a day is ~90 kB gzipped; if that list ever grows long, scale SAMPLES by
// the probe count.
pub(super) fn span(hours: i64, points: Option<i64>, now: i64) -> db::Span {
    const SAMPLES: i64 = 1_440;
    let budget = points.unwrap_or(SAMPLES).clamp(60, SAMPLES);
    let hourly = hours > db::DETAIL_DAYS * 24;
    let unit = if hourly { 3_600 } else { 60 };
    // Rounded up, or the budget would not be one: a window that does not divide
    // evenly would keep the finer step and exceed it. `i64::div_ceil` is still
    // unstable, and both operands are positive here.
    let step = unit * ((hours * 3_600 / unit + budget - 1) / budget).max(1);
    // Rows are bucketed by `ts / step` from the epoch, and a window opening
    // partway through a bucket would leave its first point short of the
    // `step / 60` minutes a whole one holds.
    db::Span { since: (now - hours * 3_600).div_euclid(step) * step, step, hourly }
}

/// Guards a per-node read: the panel sees everything, while the public page sees
/// only nodes explicitly published. `full` is the caller's own `authed`, passed
/// in because the handler also needs it for the window ceiling.
pub(super) fn readable(app: &App, full: bool, id: i64) -> bool {
    full || (app.public_page() && app.db.node(id).ok().flatten().is_some_and(|n| n.public))
}

/// Per-connection read buffer for both WebSocket surfaces. The 128 KiB default
/// would be tens of megabytes across a few hundred agents, for frames a few
/// hundred bytes long.
pub const SOCKET_BUFFER: usize = 4 * 1024;

/// Largest frame either socket accepts, matching the 64 KiB cap on the HTTP
/// body. That limit is a tower layer and never applies here, where the default
/// ceiling is 64 MiB -- reachable with a node's own token, for content that is
/// stored and then served to every viewer of the public page.
pub const MAX_FRAME: usize = 64 * 1024;

/// How long one rendered snapshot is reused. Just under the push interval, so
/// every tick rebuilds once and no viewer receives a stale frame twice.
const SNAPSHOT_TTL_MS: i64 = 1_900;

/// One audience's node list, as every browser stream and `/api/nodes` send it.
#[derive(Default)]
pub struct Frame {
    /// The millisecond it was built.
    pub(super) at: i64,
    json: Utf8Bytes,
    /// `json` gzipped, made by the first stream or `/api/nodes` request that asks for it.
    gzip: Option<axum::body::Bytes>,
}

/// Runs `read` on one audience's frame, rebuilt first once it has expired. A
/// frame is built at most once per tick however many tabs are watching: the
/// public page is anonymous, so a per-connection build would make viewer count
/// a multiplier on database work. Two slots, because the admin view carries
/// fields the public one must never expose.
fn current<T>(app: &App, full: bool, read: impl FnOnce(&mut Frame) -> T) -> T {
    let now = Utc::now().timestamp_millis();
    // Held while the frame is built, so that tabs ticking together build it
    // once; the rest wait for it off the runtime, as for the database.
    let mut cache = db::lock(&app.snapshot);
    let frame = &mut cache[usize::from(full)];
    // A cached frame's age must be non-negative. A wall clock can step backwards
    // -- NTP correcting a fresh boot -- and against a bare upper bound the
    // resulting negative reads as young, pinning the panel to a stale frame until
    // real time catches up.
    if !(0..SNAPSHOT_TTL_MS).contains(&now.saturating_sub(frame.at)) {
        let nodes = visible_nodes(app, full).unwrap_or_default();
        // `admin` is included so the panel's first fetch and its stream share one
        // cached frame.
        let json = json!({"nodes": nodes, "admin": full}).to_string().into();
        *frame = Frame { at: now, json, gzip: None };
    }
    read(frame)
}

pub(super) fn live_snapshot(app: &App, full: bool) -> Utf8Bytes {
    current(app, full, |frame| frame.json.clone())
}

/// Whether `Accept-Encoding` lists gzip with a nonzero weight. A wildcard is
/// not read as gzip: browsers name gzip itself, and any other request is still
/// compressed by the compression layer.
pub(super) fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|coding| {
            let mut parts = coding.split(';').map(str::trim);
            let name = parts.next().unwrap_or_default();
            let weight = parts
                .find_map(|p| p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")))
                .map_or(1.0, |q| q.parse::<f32>().unwrap_or(0.0));
            (name.eq_ignore_ascii_case("gzip") || name.eq_ignore_ascii_case("x-gzip")) && weight > 0.0
        })
}

/// The public frame gzipped, for streams opened with `?gzip` and for
/// `/api/nodes`. A hundred reporting nodes are about 110 KB of JSON every two
/// seconds, 15 KB compressed, for each page left open. Compressed once per
/// frame, about a millisecond at that size, however many streams take it.
pub(super) fn live_gzip(app: &App) -> axum::body::Bytes {
    current(app, false, |frame| {
        frame
            .gzip
            .get_or_insert_with(|| {
                let mut packed = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                // Into a Vec, which cannot fail.
                let _ = std::io::Write::write_all(&mut packed, frame.json.as_bytes());
                packed.finish().unwrap_or_default().into()
            })
            .clone()
    })
}

/// Drops the cached frames so the next push rebuilds. Without it a node just
/// added in the panel would disappear from the list until the frame expires.
pub(super) fn invalidate_snapshot(app: &App) {
    for frame in db::lock(&app.snapshot).iter_mut() {
        frame.at = 0;
    }
}

/// What one tick of a browser stream may send: the admin frame while the session
/// that opened it remains live, the public frame while the status page remains
/// open to anonymous callers, and nothing once either ceases to hold.
///
/// Both are checked every tick rather than at the handshake alone, because a
/// socket outlives both answers. The admin frame carries every node's address
/// and private notes, so one outliving its session would keep disclosing them
/// after the session was revoked -- the gap `reset_token` closes on the agent
/// side by dropping its sender. The public frame is what an operator withdraws by
/// switching the status page off, and a socket opened a minute earlier would
/// continue sending it for as long as the tab stayed open: `live_ws` refuses new
/// anonymous connections from that moment and `nodes` answers them 401, leaving
/// this the only remaining route. Whatever the handshake tested must be tested
/// here as well.
pub(super) fn stream_audience(app: &App, session: Option<&str>) -> Option<bool> {
    match session {
        Some(hash) => app.db.session_valid(hash).then_some(true),
        None => app.public_page().then_some(false),
    }
}

#[derive(Deserialize)]
pub struct Stream {
    /// Present for each public frame gzipped, in a binary message; the admin
    /// frame stays text. Without it frames are text, which every theme written
    /// before it reads.
    gzip: Option<String>,
}

/// How many anonymous streams may be open at once. The third bound of the
/// anonymous paths, beside `HISTORY_GATE` and `auth::PASSWORD_GATE`: a stream holds a descriptor and a task for as long as
/// its page stays open, and at a hundred nodes sends about 7 KB/s of compressed
/// frames, 55 KB/s to a page that does not ask for gzip. A thousand pages are
/// 7 to 55 MB/s outbound. Beyond it a page falls back to polling `/api/nodes`,
/// whose rendered frame every viewer shares.
pub(super) const PUBLIC_STREAMS: usize = 1000;
static PUBLIC_STREAM_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(PUBLIC_STREAMS);

/// A send that blocks this long has no reader left: the pages treat 10 s
/// without a frame as a lost stream and open another. A peer that vanished
/// without a FIN would otherwise keep its slot until TCP gives up, about
/// 15 minutes.
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The slot a new stream holds until it ends. A session is the operator's own
/// and is not counted, so whoever holds one can always open the panel.
pub(super) fn stream_slot(
    session: Option<&str>,
) -> Result<Option<tokio::sync::SemaphorePermit<'static>>, ()> {
    if session.is_some() {
        return Ok(None);
    }
    PUBLIC_STREAM_GATE.try_acquire().map(Some).map_err(|_| ())
}

/// Whom a stream handshake speaks for: `Some(Some(digest))` for a session,
/// `Some(None)` for a visitor, `None` for one the closed status page turns away.
///
/// The digest rather than the result: signing out must reach a stream already
/// running, and only the row it names can report whether it has.
/// A sibling subdomain's script opens this with the cookie too, and a WebSocket
/// is not held back by CORS: it would read the admin frame. Such a handshake is
/// answered as an anonymous one.
pub(super) fn stream_viewer(app: &App, headers: &HeaderMap) -> Option<Option<String>> {
    let session = current_session(headers)
        .filter(|_| !from_other_origin(headers))
        .filter(|hash| app.db.session_valid(hash));
    (session.is_some() || app.public_page()).then_some(session)
}

/// Live stream for the browser. Each connection runs its own timer -- simpler to
/// reason about than a fan-out channel -- over a shared snapshot, so a timer
/// costs no more than a send.
pub async fn live_ws(
    State(app): State<Shared>,
    headers: HeaderMap,
    Query(stream): Query<Stream>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(session) = stream_viewer(&app, &headers) else {
        return answer(StatusCode::UNAUTHORIZED, "需要登录后查看");
    };
    let Ok(slot) = stream_slot(session.as_deref()) else {
        return answer(StatusCode::SERVICE_UNAVAILABLE, "同时在线的访客太多，请稍后再试");
    };
    upgrade.read_buffer_size(SOCKET_BUFFER).max_message_size(MAX_FRAME).on_upgrade(move |socket| async move {
        // Held until the stream ends.
        let _slot = slot;
        stream_live(app, socket, session, stream.gzip.is_some()).await
    })
}

async fn stream_live(app: Shared, mut socket: WebSocket, session: Option<String>, gzip: bool) {
    // The panel and both official themes treat 10 s without a frame as a lost
    // stream and reconnect, so the interval must stay well below that. Not one
    // second to match the agent's default report: that would double what every
    // open page costs -- a hundred nodes are 15 KB a frame compressed, 110 KB for
    // the panel, which is never compressed -- while history, the mean and peak of
    // every report in its minute, would gain nothing.
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
    // A delayed tick, for instance behind a vacuum holding the database, is
    // followed by one frame rather than by every tick it missed: those would
    // all carry the same snapshot, 60 of them per tab after a two-minute wait.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            // The browser sends nothing but its close frame, and that is seen only
            // by reading. Unread, the close goes unanswered: the browser waits out
            // its 60 s closing handshake while this loop keeps pushing a full frame
            // every tick to a page that has already discarded the socket.
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => continue,
            },
        }
        // Closed rather than downgraded to the public frame, which would leave the
        // panel rendering a list with every admin field missing. The close allows
        // a client to re-query /api/me and determine its current state.
        let Some(full) = stream_audience(&app, session.as_deref()) else { break };
        // The admin frame stays uncompressed, asked or not: it carries every
        // node's address and private notes beside strings an agent reports, and the
        // length of each compressed frame, every two seconds on the wire,
        // would let one rogue agent guess the rest a character at a time.
        let frame = if gzip && !full {
            Message::Binary(live_gzip(&app))
        } else {
            Message::Text(live_snapshot(&app, full))
        };
        if !matches!(tokio::time::timeout(SEND_TIMEOUT, socket.send(frame)).await, Ok(Ok(()))) {
            break;
        }
    }
}

pub async fn ping_tasks(_: Admin, State(app): State<Shared>) -> Response {
    match app.db.ping_tasks() {
        Ok(tasks) => Json(json!({"tasks": tasks})).into_response(),
        Err(e) => fail(e),
    }
}

/// As `reorder_nodes`. Nothing is pushed to the agents: the list they run is in
/// id order, see `ping_tasks_for`.
pub async fn reorder_ping_tasks(_: Admin, State(app): State<Shared>, Json(order): Json<Order>) -> Response {
    match app.db.reorder_ping_tasks(&order.ids) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => fail(e),
    }
}

/// A probe target the agent can resolve: `host:port`, with an IPv6 literal
/// bracketed as a URL writes one.
///
/// A bare `contains(':')` admitted three forms that never connect: a bare IPv6
/// address, which is all colons; `:443` with no host; and `host:` with no port.
/// The agent's `lookup_host` errors on each, `tcp_ping` returns -1, and the chart
/// draws a probe at 100% loss indefinitely with nothing in any log identifying
/// the target as the cause.
pub(super) fn valid_target(target: &str) -> bool {
    let (host, port) = match target.strip_prefix('[') {
        Some(rest) => match rest.split_once("]:") {
            Some(pair) => pair,
            None => return false,
        },
        // Unbracketed, so the last colon is the port separator; anything still
        // containing a colon is an IPv6 address that required brackets.
        None => match target.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => (host, port),
            _ => return false,
        },
    };
    !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p > 0)
}

pub async fn save_ping_task(_: Admin, State(app): State<Shared>, Json(mut task): Json<PingTask>) -> Response {
    // Trimmed into the stored value rather than a discarded copy: what reaches
    // the agent is `task.target`, and a trailing space from a paste passes
    // `valid_target` while `lookup_host` rejects the stored string outright,
    // leaving the probe reporting -1 indefinitely. The name is trimmed for the
    // same reason, as it travels to the public page as a chart label.
    task.name = task.name.trim().to_owned();
    task.target = task.target.trim().to_owned();
    if task.name.is_empty() || task.target.is_empty() {
        return bad("请填写名称和目标");
    }
    if task.name.chars().count() > MAX_NAME {
        return bad("名称最多 64 个字");
    }
    // A TCP probe requires an explicit port; a bare host would silently never
    // connect.
    if !valid_target(&task.target) {
        return bad("目标要写成「主机:端口」，例如 1.1.1.1:443 或 [2606:4700:4700::1111]:443");
    }
    // Refused rather than clamped, for the reason `setting_error` gives for
    // `retention_days`: the agent clamps this again on arrival, so an
    // out-of-range value never fails but silently becomes a different number
    // while the panel still displays what was entered. Below the floor that
    // number is 5 seconds, the fastest probe available, run by every node the
    // task is assigned to; the panel reaches 0 simply by having its interval
    // field cleared.
    if !(Db::MIN_PROBE_INTERVAL..=3_600).contains(&task.interval) {
        return bad(&format!("间隔要在 {} 到 3600 秒之间", Db::MIN_PROBE_INTERVAL));
    }
    match app.db.save_ping_task(&task) {
        Ok(id) => {
            agent_ws::push_ping_tasks(&app, None);
            Json(json!({"id": id})).into_response()
        }
        Err(e) => fail(e),
    }
}

/// The probes ticked for one node, and `base`, those it had when the editor
/// opened; see `Db::set_node_ping_tasks`.
#[derive(Deserialize)]
pub struct NodeProbes {
    tasks: Vec<i64>,
    base: Vec<i64>,
}

pub async fn set_node_ping_tasks(
    _: Admin,
    State(app): State<Shared>,
    Path(id): Path<i64>,
    body: Result<Json<NodeProbes>, JsonRejection>,
) -> Response {
    let Ok(Json(probes)) = body else { return bad("监控列表格式不对") };
    match app.db.set_node_ping_tasks(id, &probes.tasks, &probes.base) {
        Ok(true) => {
            agent_ws::push_ping_tasks(&app, Some(id));
            Json(json!({"ok": true})).into_response()
        }
        Ok(false) => no_such_node(),
        Err(e) => fail(e),
    }
}

pub async fn delete_ping_task(_: Admin, State(app): State<Shared>, Path(id): Path<i64>) -> Response {
    match app.db.delete_ping_task(id) {
        Ok(()) => {
            agent_ws::push_ping_tasks(&app, None);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => fail(e),
    }
}
