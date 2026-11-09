use super::*;
use crate::db::{Db, Span};

fn live_admin_session(app: &App, token: &str) -> HeaderMap {
    app.db.create_session(&sha256(token), Utc::now().timestamp() + 3600).unwrap();
    HeaderMap::from_iter([(header::COOKIE, format!("monitor_session={token}").parse().unwrap())])
}

/// What a browser on `https://monitor.example.com` sends with a panel write.
fn panel_headers() -> HeaderMap {
    HeaderMap::from_iter([
        (header::ORIGIN, "https://monitor.example.com".parse().unwrap()),
        (header::HeaderName::from_static("sec-fetch-site"), "same-origin".parse().unwrap()),
    ])
}

/// A sibling subdomain is the same site, so `SameSite=Lax` lets its bodiless
/// POST carry the session; only `Sec-Fetch-Site` tells it from the panel.
#[test]
fn a_write_from_another_origin_is_refused() {
    let from = |site: &str| {
        HeaderMap::from_iter([(header::HeaderName::from_static("sec-fetch-site"), site.parse().unwrap())])
    };
    for site in ["same-site", "cross-site", "none"] {
        assert!(cross_site_write(&Method::POST, &from(site)), "{site}");
        assert!(cross_site_write(&Method::DELETE, &from(site)), "{site}");
    }
    assert!(!cross_site_write(&Method::POST, &from("same-origin")));
    assert!(!cross_site_write(&Method::POST, &HeaderMap::new()), "not a browser, or one too old to say");
    assert!(
        !cross_site_write(&Method::GET, &from("cross-site")),
        "reads are answered to the page that cannot read them"
    );
    // A WebSocket handshake is a GET that the page can read.
    assert!(from_other_origin(&from("same-site")));
    assert!(!from_other_origin(&from("same-origin")));
    assert!(!from_other_origin(&HeaderMap::new()));
}

fn app() -> App {
    App::for_test(Db::open(":memory:").unwrap())
}

#[tokio::test]
async fn period_history_requires_admin_and_reports_actual_retention() {
    let _serial = HISTORY_TESTS.lock().await;
    let app = std::sync::Arc::new(app());
    let mut parts = axum::http::Request::new(()).into_parts().0;
    let rejected = Admin::from_request_parts(&mut parts, &app).await.err().unwrap();
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    app.db.create_session(&sha256("session"), Utc::now().timestamp() + 3600).unwrap();
    parts.headers.insert(header::COOKIE, "monitor_session=session".parse().unwrap());
    assert!(Admin::from_request_parts(&mut parts, &app).await.is_ok());
    let id = app.db.create_node(&Node { name: "n".into(), ..Default::default() }, "token").unwrap();
    app.db.set("retention_days", "3").unwrap();
    for days in [-1, 0, 366] {
        let response =
            traffic_periods(Admin, State(app.clone()), Path(id), Ok(Query(PeriodWindow { days }))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    for days in ["all", "invalid", "1.5"] {
        let query = Query::<PeriodWindow>::try_from_uri(
            &format!("/api/nodes/{id}/traffic/periods?days={days}").parse().unwrap(),
        );
        let response = traffic_periods(Admin, State(app.clone()), Path(id), query).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let response =
        traffic_periods(Admin, State(app.clone()), Path(id + 1), Ok(Query(PeriodWindow { days: 7 }))).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response =
        traffic_periods(Admin, State(app.clone()), Path(id), Ok(Query(PeriodWindow { days: 365 }))).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store, no-transform");
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let report: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(report["days"], 3);
    assert_eq!(report["rows"].as_array().unwrap().len(), 3);
    assert_eq!(report["today"], Local::now().date_naive().to_string());
    assert!(report["summary"].is_null());
}

fn peer() -> ConnectInfo<std::net::SocketAddr> {
    ConnectInfo("198.51.100.7:40000".parse().unwrap())
}

/// Taken by every test that calls `metrics`. `HISTORY_GATE` is process-wide,
/// and a test holding all of its permits would refuse a parallel one with a
/// 503. Tokio's mutex, since the guard is held across awaits.
static HISTORY_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A lookup nobody can refresh must not hide an update all day, and a panel
/// left open on a screen must not ask GitHub on every load.
#[test]
fn a_release_lookup_is_held_for_six_hours_and_a_failed_one_for_ten_minutes() {
    let now = Utc::now().timestamp();
    let read = |read_at, hub: &str| crate::Releases { read_at, hub: hub.into() };
    assert!(!fresh_enough(&crate::Releases::default(), now), "nothing has been read yet");
    assert!(fresh_enough(&read(now - 5 * 3600, "1.2.0"), now));
    assert!(!fresh_enough(&read(now - 7 * 3600, "1.2.0"), now));
    assert!(fresh_enough(&read(now - 300, ""), now), "a failure is held briefly");
    assert!(!fresh_enough(&read(now - 1200, ""), now), "and only briefly");
    assert!(!fresh_enough(&read(now + 3600, "1.2.0"), now), "read before the clock was set back");
}

fn app_with_site(site: &str) -> App {
    let mut state = app();
    state.site = site.into();
    state
}

#[tokio::test]
async fn provisioning_follows_the_browsers_own_origin() {
    let app = std::sync::Arc::new(app_with_site("https://monitor.example.com"));
    let good = panel_headers();
    assert!(provisioning_allowed(&app, &good).is_ok());

    // The four shapes a reverse proxy puts in Host and X-Forwarded-Proto
    // while the browser is on https: no X-Forwarded-Proto at all, the proxy's
    // own upstream address as Host, a plaintext hop behind a TLS edge, and a
    // port dropped by `$host`. Reading them instead of the origin refuses
    // each one.
    let mut proxied = good.clone();
    proxied.insert(header::HOST, "127.0.0.1:28080".parse().unwrap());
    proxied.insert("x-forwarded-proto", "http".parse().unwrap());
    assert!(provisioning_allowed(&app, &proxied).is_ok());

    for origin in [
        "http://monitor.example.com",
        "https://198.51.100.1",
        "null",
        // A registered name resolving wherever its owner points it, which is
        // not the loopback entry below however it is spelled.
        "http://127.0.0.1.example.com",
    ] {
        let mut headers = good.clone();
        headers.insert(header::ORIGIN, origin.parse().unwrap());
        assert_eq!(provisioning_allowed(&app, &headers), Err(PROVISIONING_DENIED), "{origin}");
    }

    // A panel opened over a tunnel reads as loopback. It names no address a
    // node could reach, so it provisions alongside --site and not without it.
    for origin in ["http://127.0.0.1:9911", "http://localhost:9911", "http://[::1]:9911"] {
        let mut headers = good.clone();
        headers.insert(header::ORIGIN, origin.parse().unwrap());
        assert!(provisioning_allowed(&app, &headers).is_ok(), "{origin}");
        assert!(provisioning_allowed(&app_with_site(""), &headers).is_err(), "{origin} without --site");
    }
    // No origin at all: every caller that is not a browser, and a browser
    // behind a proxy that clears the header, which the message names.
    let mut headers = good.clone();
    headers.remove(header::ORIGIN);
    assert_eq!(provisioning_allowed(&app, &headers), Err(ORIGIN_MISSING));

    // Both panel paths refuse, and neither leaves anything behind.
    headers = good.clone();
    headers.insert(header::ORIGIN, "http://monitor.example.com".parse().unwrap());
    let node = serde_json::from_value(json!({"name":"blocked"})).unwrap();
    assert_eq!(
        create_node(Admin, State(app.clone()), headers.clone(), Ok(Json(node))).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(open_register(Admin, State(app.clone()), headers).await.status(), StatusCode::FORBIDDEN);
    assert!(app.db.nodes().unwrap().is_empty());
    assert!(app.db.get("register_key").is_none());

    // --site takes the origin's place in the command, so one that is not an
    // https domain refuses every entry.
    for site in [
        "http://monitor.example.com",
        "https://198.51.100.1",
        "https://user@monitor.example.com",
        "https://monitor.example.com/path",
        "https://localhost",
        "https://hub.localhost",
    ] {
        assert!(https_domain(site).is_none());
        assert_eq!(provisioning_allowed(&app_with_site(site), &good), Err(PROVISIONING_DENIED), "{site}");
    }
}

/// Whatever this accepts is pushed to every assigned agent and passed directly
/// to `lookup_host`. Forms it cannot resolve return -1 indefinitely, which the
/// chart draws as a probe losing every packet, so the check must match what
/// the error message claims.
#[test]
fn a_probe_target_must_be_something_the_agent_can_resolve() {
    // Each of these causes `lookup_host` to return an error, verified against
    // it: a bare IPv6 address is all colons, and the other two omit the half
    // the message requires.
    for bad in ["2606:4700:4700::1111", ":443", "example.com:", "1.1.1.1", "1.1.1.1:0", "[::1]:x", "[::1]"] {
        assert!(!valid_target(bad), "{bad}");
    }
    for good in ["1.1.1.1:443", "[2606:4700:4700::1111]:443", "example.com:80", "[::1]:1"] {
        assert!(valid_target(good), "{good}");
    }
}

/// The check above is meaningful only if it runs on the stored string: what
/// reaches the agent is the stored value, and `lookup_host` rejects
/// `"1.1.1.1:443 "` outright -- the permanent -1 `valid_target` exists to
/// prevent, reachable through a check that passed.
#[tokio::test]
async fn a_probe_target_is_stored_as_the_string_that_was_checked() {
    let app = std::sync::Arc::new(app());
    let save = |name: &str, target: &str| {
        let task = PingTask {
            id: 0,
            name: name.to_owned(),
            target: target.to_owned(),
            interval: 60,
            nodes: vec![],
            ..Default::default()
        };
        save_ping_task(Admin, State(app.clone()), Json(task))
    };
    assert_eq!(save(" 探测 ", "1.1.1.1:443 ").await.status(), StatusCode::OK);
    let stored = &app.db.ping_tasks().unwrap()[0];
    assert_eq!(stored.target, "1.1.1.1:443", "the agent gets this string, not the one that was checked");
    assert_eq!(stored.name, "探测", "and it labels an anonymous chart");
    // Trimming must not turn a blank entry into a saved row.
    assert_eq!(save("   ", "   ").await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(app.db.ping_tasks().unwrap().len(), 1);
}

/// The same rule as `retention_days`, applied to the other value this hub
/// clamps downstream: an out-of-range value must fail, or it silently becomes
/// a different one. Below the floor that value is 5 seconds, the fastest probe
/// available, and the panel reaches 0 simply by clearing its interval field,
/// since `Number("")` is 0.
#[tokio::test]
async fn a_probe_interval_out_of_range_is_refused_rather_than_clamped() {
    let app = std::sync::Arc::new(app());
    let save = |interval| {
        let task = PingTask {
            id: 0,
            name: "probe".into(),
            target: "1.1.1.1:443".into(),
            interval,
            nodes: vec![],
            ..Default::default()
        };
        save_ping_task(Admin, State(app.clone()), Json(task))
    };
    for refused in [0, -1, 4, 3_601, i64::MAX] {
        assert_eq!(save(refused).await.status(), StatusCode::BAD_REQUEST, "{refused}");
    }
    let long = PingTask {
        name: "名".repeat(MAX_NAME + 1),
        target: "1.1.1.1:443".into(),
        interval: 60,
        ..Default::default()
    };
    assert_eq!(save_ping_task(Admin, State(app.clone()), Json(long)).await.status(), StatusCode::BAD_REQUEST);
    assert!(app.db.ping_tasks().unwrap().is_empty(), "a refused interval must not store a probe");

    // Both ends of the range still save, storing exactly what was sent.
    for ok in [5, 60, 3_600] {
        assert_eq!(save(ok).await.status(), StatusCode::OK, "{ok}");
    }
    let stored: Vec<i64> = app.db.ping_tasks().unwrap().iter().map(|t| t.interval).collect();
    assert_eq!(stored, vec![5, 60, 3_600]);
}

/// The update and install paths follow a manifest's `url` or a pasted address
/// to build a download address, so what counts as a GitHub repository
/// constitutes the entire trust boundary: whatever this accepts, the hub will
/// fetch.
#[test]
fn only_a_github_repository_url_can_name_a_release_to_download() {
    assert_eq!(github_repo("https://github.com/monitor-probe/monitor"), Some(("monitor-probe", "monitor")));
    // A link to the repository, in whatever form the author wrote it or the
    // address bar showed it.
    assert_eq!(github_repo("https://github.com/a/b.git"), Some(("a", "b")));
    assert_eq!(github_repo("https://github.com/a/b/tree/main"), Some(("a", "b")));
    assert_eq!(github_repo("https://github.com/a/b/"), Some(("a", "b")));
    assert_eq!(github_repo("https://github.com/a/b?tab=readme-ov-file"), Some(("a", "b")));
    assert_eq!(github_repo("https://github.com/a/b#readme"), Some(("a", "b")));

    for hostile in [
        "",
        // Not github.com, however much of it appears in the string.
        "http://github.com/a/b",
        "https://github.com.evil.test/a/b",
        "https://github.com@evil.test/a/b",
        "https://evil.test/https://github.com/a/b",
        // On github.com, but naming no repository to fetch from.
        "https://github.com/a",
        "https://github.com//b",
        "https://github.com/../../etc/passwd",
        "https://github.com/a/..",
        // Anything that could open a segment of its own in the URL built from
        // it, whether encoded, queried or fragmented.
        "https://github.com/a/b%2f..%2fc",
        "https://github.com/a?x=/b",
        "https://github.com/a#/b",
        "https://github.com/a b",
    ] {
        assert_eq!(github_repo(hostile), None, "{hostile} must not name a download");
    }

    // The release tag also lands in that URL, arriving from the API rather
    // than the manifest.
    assert!(path_segment("v0.1.15") && path_segment("2024.1"));
    assert!(!path_segment("release/1.0") && !path_segment("..") && !path_segment(""));
}

/// The entire chunked-upload protocol: an upload is only ever as long as what
/// has landed, so a piece continues it, restarts it, or is refused.
#[tokio::test]
async fn a_chunk_continues_an_upload_only_where_the_last_one_ended() {
    let tmp = crate::TempDir::new("chunk");
    let path = tmp.0.join("upload");
    let path = path.to_str().unwrap();
    let piece = |offset, total| Chunk { offset, total };
    let body = |bytes: &'static [u8]| axum::body::Body::from(bytes);

    // Two pieces in order, with the length indicating where the next begins.
    assert_eq!(receive(path, &piece(0, 6), 1024, body(b"abc")).await.unwrap(), 3);
    assert_eq!(receive(path, &piece(3, 6), 1024, body(b"def")).await.unwrap(), 6);
    assert_eq!(std::fs::read(path).unwrap(), b"abcdef");

    // A gap, a rewind and an overshoot all produce the same refusal.
    assert!(receive(path, &piece(9, 12), 1024, body(b"xyz")).await.is_err());
    assert!(receive(path, &piece(3, 12), 1024, body(b"xyz")).await.is_err());
    assert!(receive(path, &piece(6, 7), 1024, body(b"toolong")).await.is_err());
    // None of them modified the file, so the upload can continue.
    assert_eq!(std::fs::metadata(path).unwrap().len(), 6);

    // The ceiling is checked against the declared total, before any bytes
    // arrive.
    assert!(receive(path, &piece(0, 4096), 1024, body(b"a")).await.is_err());
    assert!(receive(path, &piece(0, 0), 1024, body(b"")).await.is_err());

    // Starting over truncates whatever an interrupted attempt left behind.
    assert_eq!(receive(path, &piece(0, 2), 1024, body(b"hi")).await.unwrap(), 2);
    assert_eq!(std::fs::read(path).unwrap(), b"hi");
}

/// Both scratch names a restore uses sit beside the live database, and one left
/// behind is what the next upload fails on: `receive` refuses a first chunk
/// that does not align with an existing file.
///
/// This does not cover the rename in `db_restore`, which changes which path is
/// open during the copy and would require a second upload landing inside it.
/// It covers the part that outlives the request, which is what a later edit
/// could silently drop.
#[tokio::test]
async fn a_finished_restore_leaves_no_scratch_file_behind() {
    let tmp = crate::TempDir::new("restore");
    let dir = tmp.0.clone();
    let live = dir.join("live.db").to_string_lossy().into_owned();
    let app = std::sync::Arc::new(App::for_test(Db::open(&live).unwrap()));
    node(&app, "kept", true);
    app.db.set("origin_cdn_only", "on").unwrap();
    // In the backup, and so restored with it unless the restore drops it.
    app.db.create_session(&sha256("backed-up"), Utc::now().timestamp() + 3_600).unwrap();

    // What a restore actually receives: a backup of a hub database.
    let copy = format!("{live}.copy");
    app.db.backup_into(&copy).unwrap();
    let bytes = std::fs::read(&copy).unwrap();
    std::fs::remove_file(&copy).unwrap();
    // Not in the backup: it stays until the last chunk has arrived.
    node(&app, "added", true);

    let (total, half) = (bytes.len() as u64, bytes.len() / 2);
    let revoked = live_admin_session(&app, "revoked");
    let headers = live_admin_session(&app, "live-restorer");
    // The handler was authorized at extraction, then the session was
    // revoked while the upload was still arriving.
    app.db.drop_session(&sha256("revoked")).unwrap();
    let refused = db_restore(
        Admin,
        State(app.clone()),
        Query(Chunk { offset: 0, total }),
        revoked,
        axum::body::Body::from(bytes.clone()),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    assert!(refused.headers().get(header::SET_COOKIE).is_none());
    assert_eq!(app.db.nodes().unwrap().len(), 2);
    assert_eq!(app.db.get("origin_cdn_only").as_deref(), Some("on"));
    assert!(app.db.session_valid(&sha256("live-restorer")));

    // In two chunks, as the panel sends any backup larger than one.
    let mut done = None;
    for (offset, piece) in [(0, &bytes[..half]), (half as u64, &bytes[half..])] {
        let piece = axum::body::Body::from(piece.to_vec());
        done = Some(
            db_restore(Admin, State(app.clone()), Query(Chunk { offset, total }), headers.clone(), piece)
                .await,
        );
        let expected = if offset == 0 { 2 } else { 1 };
        assert_eq!(app.db.nodes().unwrap().len(), expected, "restored on the last chunk alone");
    }
    let response = done.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let replacement = replacement_session(&response);
    assert_eq!(app.db.sessions().unwrap().len(), 1);
    assert!(app.db.session_valid(&sha256(replacement)));
    assert!(!app.db.session_valid(&sha256("live-restorer")));
    assert_eq!(app.db.nodes().unwrap().len(), 1, "the backup went in");
    assert!(!app.db.session_valid(&sha256("backed-up")), "a session from the backup does not come back");
    assert_eq!(
        app.db.get("origin_cdn_only").as_deref(),
        Some("off"),
        "the premise is the operator's to restate"
    );

    // The database and its journal are the only files that may remain.
    let left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|name| !matches!(name.as_str(), "live.db" | "live.db-wal" | "live.db-shm"))
        .collect();
    assert!(left.is_empty(), "left beside the database: {left:?}");
}

/// A connected agent holding one report. The receiver is returned because
/// dropping it closes the channel, which is the signal `reset_token` is tested
/// for.
fn connect(app: &App, id: i64, metrics: Value) -> tokio::sync::mpsc::Receiver<String> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let mut agent = crate::agent_ws::Agent::new(7, tx);
    agent.metrics = metrics;
    agent.last_seen = Utc::now().timestamp();
    app.agents.write().unwrap().insert(id, agent);
    rx
}

/// A probe assigned to `nodes`. The window query draws only a node's current
/// assignments, so a fixture holding ping records requires one.
fn task(app: &App, nodes: Vec<i64>) -> i64 {
    app.db
        .save_ping_task(&PingTask {
            id: 0,
            name: "probe".into(),
            target: "1.1.1.1:443".into(),
            interval: 60,
            nodes,
            ..Default::default()
        })
        .unwrap()
}

fn node(app: &App, name: &str, public: bool) -> i64 {
    app.db
        .create_node(
            &Node { name: name.into(), public, remark: "secret note".into(), ..Default::default() },
            &format!("token-of-{name}"),
        )
        .unwrap()
}

/// A chart request costs roughly the same whatever it spans. This path
/// requires no session, so an unbounded window would be megabytes of JSON any
/// caller could have the hub build.
#[test]
fn a_history_window_costs_the_same_however_wide_it_is() {
    let app = app();
    let id = node(&app, "n", true);
    let now = Utc::now().timestamp();
    // A month of history at the rate the hub writes it, folded as the hourly
    // pass would. Two probes, because the budget is per series and a
    // single-probe fixture would conceal that.
    const PROBES: i64 = 2;
    for _ in 0..PROBES {
        task(&app, vec![id]);
    }
    for i in 0..30 * 1440 {
        app.db.insert_metric(id, now - i * 60, &json!({"cpu": 1.0})).unwrap();
        for task in 1..=PROBES {
            app.db.insert_pings(id, &[(task, now - i * 60, 42)]).unwrap();
        }
    }
    app.db.roll_up(now, 365).unwrap();

    // Including windows that do not divide evenly, which are where a step
    // rounded the wrong way overruns, and both sides of the hourly tier.
    for hours in [1, 6, 13, 23, 24, 168, 169, 720, 8_760] {
        let span = span(hours, None, now);
        assert_eq!(span.hourly, hours > 168, "{hours}h");
        if span.hourly {
            assert_eq!(span.step % 3_600, 0, "{hours}h: an hourly row straddles two points");
        }
        let metrics = app.db.metrics(id, span).unwrap();
        let (ping, _) = app.db.ping_records(id, span).unwrap();
        // Against the budget itself rather than whatever the step produced:
        // derived from the step, this would only demonstrate that division
        // works. One bucket of slack, as the window rarely divides evenly.
        let cap = 1_441;
        assert!(metrics.len() <= cap, "{hours}h returned {} metric rows", metrics.len());
        assert!(
            ping.len() <= cap * PROBES as usize,
            "{hours}h returned {} ping rows for {PROBES} probes",
            ping.len()
        );
        // Thinned, but neither empty nor reaching outside the window.
        assert!(!metrics.is_empty() && !ping.is_empty(), "{hours}h returned nothing");
        assert!(
            metrics.iter().all(|m| m["ts"].as_i64().unwrap() >= span.since),
            "{hours}h reached back too far"
        );
        // Begun on a point boundary, so the first point is a whole one where
        // the history reaches past it: this node reported every minute.
        if hours < 720 {
            assert_eq!(metrics[0]["minutes"], span.step / 60, "{hours}h opened partway through a point");
        }
    }

    // A day returns every minute it holds: thinning exists only for what the
    // screen cannot draw.
    let step = |hours, points| span(hours, points, now).step;
    assert_eq!(step(24, Some(2_000)), 60, "a day of minutes fits under the ceiling");
    assert_eq!(step(6, Some(2_000)), 60, "and so does six hours");
    assert_eq!(step(720, None), 3_600, "a month is its hours");

    // A caller may request less than the budget, never more: the ceiling
    // belongs to the hub, since this path takes no credentials.
    assert!(step(24, Some(390)) > step(24, None));
    assert_eq!(step(24, Some(100_000)), step(24, None));
    assert_eq!(step(24, Some(0)), step(24, Some(60)));

    // Requesting one half leaves the other empty rather than sending it: on
    // the day window that half was two thirds of the response.
    let series = |q: &str| serde_urlencoded::from_str::<Window>(q).unwrap().series;
    assert_eq!(series("hours=24&series=ping").as_deref(), Some("ping"));
    assert!(series("hours=24").is_none(), "no series means both, which is what curl gets");
}

/// What a thinned bucket may return. Keeping one row and discarding the rest
/// would integrate the seven-day chart to twice the traffic the minutes hold,
/// and draw a probe losing half its packets as an unbroken line.
#[test]
fn a_thinned_bucket_answers_with_its_mean_and_says_what_it_lost() {
    let app = app();
    let id = node(&app, "n", true);
    // Anchored on a bucket boundary, one whole bucket in the past. Anchored on
    // `now`, the rows would straddle the boundary depending on the second the
    // suite runs at.
    let base = Utc::now().timestamp() / 120 * 120 - 120;
    // One bucket: a quiet minute and a busy one, then a probe that answered
    // once and timed out three times.
    // The quiet minute predates the peak column, whose default is 0.
    app.db.insert_metric(id, base + 10, &json!({"cpu": 0.0, "net_rx": 0, "net_tx": 3_000})).unwrap();
    app.db
        .insert_metric(
            id,
            base + 70,
            &json!({"cpu": 40.0, "net_rx": 1_000, "net_rx_max": 4_000, "net_tx": 1_000, "net_tx_max": 2_000}),
        )
        .unwrap();
    for _ in 0..3 {
        task(&app, vec![id]);
    }
    for (i, latency) in [30, -1, -1, -1].into_iter().enumerate() {
        app.db.insert_pings(id, &[(1, base + 10 + i as i64 * 20, latency)]).unwrap();
    }
    // A second probe that never answered, and a third that answered cleanly.
    app.db.insert_pings(id, &[(2, base + 10, -1)]).unwrap();
    app.db.insert_pings(id, &[(3, base + 10, 12)]).unwrap();

    let m = &app.db.metrics(id, Span::minutes(base, 120)).unwrap()[0];
    assert_eq!(m["cpu"], 20.0, "the bucket is its mean, not one row of it");
    assert_eq!(m["net_rx"], 500);
    assert_eq!(m["net_rx_max"], 4_000, "the bucket peaks where its busiest minute did");
    assert_eq!(m["net_tx_max"], 3_000, "a row without a peak counts as its own mean");
    assert_eq!(m["ts"], base, "stamped with the bucket, so every series shares a grid");

    // Keyed by task rather than index: the order is the panel's, which
    // `a_probe_chart_follows_the_panel_order` covers.
    let (rows, window_loss) = app.db.ping_records(id, Span::minutes(base, 120)).unwrap();
    let probe =
        |task: i64| rows.iter().find(|r| r["task_id"] == task).unwrap_or_else(|| panic!("no probe {task}"));
    assert_eq!(probe(1)["latency"], 30, "the median of what answered, not of the timeouts");
    assert_eq!(probe(1)["loss"], 75);
    assert_eq!(probe(2)["latency"], json!(null), "a bucket that was all timeout has no latency");
    assert_eq!(probe(2)["loss"], 100);
    // One answer, so there is nothing for a band to span.
    assert!(probe(1).get("band").is_none(), "{:?}", probe(1));
    // A clean bucket carries no loss key, which is why the percentage rounds
    // up: the key's absence denotes no loss, so no loss must be the only way
    // to produce it.
    assert!(probe(3).get("loss").is_none(), "{:?}", probe(3));
    // Each probe has one bucket here, so the window and bucket figures agree
    // -- precisely the fixture shape that concealed the difference between
    // them. The test below separates the two.
    assert_eq!(window_loss["2"], 100.0);
    assert!(window_loss.get("3").is_none(), "a probe that lost nothing is left out");

    // One timeout in a bucket too large for it to reach a whole percent:
    // truncating would report the same as a clean bucket.
    let wide = node(&app, "wide", true);
    let wide_probe = task(&app, vec![wide]);
    let wide_base = base / 180 * 180;
    for i in 0..180 {
        app.db.insert_pings(wide, &[(wide_probe, wide_base + i, if i == 0 { -1 } else { 20 })]).unwrap();
    }
    let (rows, _) = app.db.ping_records(wide, Span::minutes(wide_base, 180)).unwrap();
    assert_eq!(rows.len(), 1, "the fixture has to be one bucket for this to mean anything");
    let row = &rows[0];
    assert_eq!(row["loss"], 1, "a bucket that lost one of 180 has not lost none");

    // What the band conveys: the median reading and the two extremes the
    // bucket reached. Drawing 20 alone would render a 40 ms swing as a flat
    // point.
    let jitter = node(&app, "jitter", true);
    let jitter_probe = task(&app, vec![jitter]);
    for (i, latency) in [10, 20, 50, 20, 20].into_iter().enumerate() {
        app.db.insert_pings(jitter, &[(jitter_probe, wide_base + i as i64, latency)]).unwrap();
    }
    let row = &app.db.ping_records(jitter, Span::minutes(wide_base, 180)).unwrap().0[0];
    assert_eq!(row["latency"], 20, "the middle answer, not the mean of 24");
    assert_eq!(row["band"], json!([10, 50]));

    // An even count has no single middle value, so it is the mean of the two
    // straddling it. Every neighbouring pair differs, so selecting one rank
    // either way would yield 20 or 30 rather than 25.
    let even = node(&app, "even", true);
    let even_probe = task(&app, vec![even]);
    for (i, latency) in [40, 10, 30, 20].into_iter().enumerate() {
        app.db.insert_pings(even, &[(even_probe, wide_base + i as i64, latency)]).unwrap();
    }
    assert_eq!(app.db.ping_records(even, Span::minutes(wide_base, 180)).unwrap().0[0]["latency"], 25);
    // An odd count is its middle reading, with the ranks around it far apart.
    let odd = node(&app, "odd", true);
    let odd_probe = task(&app, vec![odd]);
    for (i, latency) in [20, 900, 30].into_iter().enumerate() {
        app.db.insert_pings(odd, &[(odd_probe, wide_base + i as i64, latency)]).unwrap();
    }
    assert_eq!(app.db.ping_records(odd, Span::minutes(wide_base, 180)).unwrap().0[0]["latency"], 30);

    // Several buckets: each row is stamped with its own bucket's start.
    let grid = node(&app, "grid", true);
    let grid_probe = task(&app, vec![grid]);
    // Three whole buckets in the past, whatever second the suite runs at.
    let since = wide_base - 3 * 180;
    for i in 0..3 {
        app.db.insert_pings(grid, &[(grid_probe, since + i * 180 + 7, 20)]).unwrap();
    }
    let rows = app.db.ping_records(grid, Span::minutes(since, 180)).unwrap().0;
    let stamps: Vec<_> = rows.iter().map(|r| r["ts"].clone()).collect();
    assert_eq!(stamps, [since, since + 180, since + 360].map(|t| json!(t)));
}

/// What a window lost is the proportion of its samples lost, and only the hub
/// can determine it: `close_bucket` divides within each bucket and keeps the
/// quotient, so the denominators are gone by the time a reader sees the rows.
/// Averaging the bucket percentages would weight a bucket holding one sample
/// equally with one holding twelve, and unequal buckets are the ordinary case
/// rather than an edge one. The window's first and last are partial by
/// construction, and a probe that starts, stops, loses its node or skips a
/// round on a slow resolver produces more.
#[test]
fn a_probe_reports_the_share_of_the_window_it_lost_not_the_mean_of_its_buckets() {
    let app = app();
    let id = node(&app, "n", true);
    let probe = task(&app, vec![id]);
    let base = Utc::now().timestamp() / 60 * 60 - 120;
    // A full minute at five seconds per round with no loss, then a minute
    // holding one sample, which was lost, before the probe stopped.
    for i in 0..12 {
        app.db.insert_pings(id, &[(probe, base + i * 5, 20)]).unwrap();
    }
    app.db.insert_pings(id, &[(probe, base + 60, -1)]).unwrap();

    let (rows, loss) = app.db.ping_records(id, Span::minutes(base, 60)).unwrap();
    let per_bucket: Vec<i64> = rows.iter().map(|r| r["loss"].as_i64().unwrap_or(0)).collect();
    assert_eq!(per_bucket, vec![0, 100], "the buckets are right about themselves");

    // Their mean is 50%, while one round of thirteen did not answer.
    let window = loss.get(probe.to_string()).and_then(|v| v.as_f64()).expect("this probe lost one");
    assert!((window - 100.0 / 13.0).abs() < 1e-9, "{window}");
    assert!(window < 8.0, "the window lost {window}%, not the 50% its buckets average to");
}

#[test]
fn the_public_view_hides_private_nodes_and_sensitive_fields() {
    let app = app();
    let open = node(&app, "open", true);
    node(&app, "hidden", false);
    app.db.save_facts(open, &json!({"hostname": "vps-1"}), "198.51.100.9", "").unwrap();

    // A live report, so the public view has metrics to strip. `hostname` is
    // what a node token in the wrong hands can insert, and what the agent
    // repository could add to the contract.
    let _held = connect(
        &app,
        open,
        json!({"boot_id": "abc", "net_rx_total": 134_000_000_000i64, "cpu": 1.0, "iface": "eth1",
               "hostname": "db-prod-01", "ip": "203.0.113.7", "total_rx": 1}),
    );

    let public = visible_nodes(&app, false).unwrap();
    assert_eq!(public.len(), 1, "a node marked private must not be listed");
    assert_eq!(public[0]["name"], "open");
    // Disclosing the token would let any visitor impersonate the node.
    for hidden in ["ip", "addresses", "ipv4_auto", "ipv6_auto", "remark", "hostname", "token", "interval"] {
        assert!(public[0].get(hidden).is_none(), "{hidden} must not be public");
    }
    assert!(
        !serde_json::to_string(&public).unwrap().contains("token-of-open"),
        "no node's token may appear anywhere in a public payload"
    );
    // Raw kernel counters would disclose the machine's lifetime traffic, and
    // anything the contract does not name is not published at all, the report
    // coming from a machine holding one node's token.
    for hidden in ["boot_id", "net_rx_total", "net_tx_total", "hostname", "ip"] {
        assert!(public[0]["metrics"].get(hidden).is_none(), "{hidden} must not be public");
    }
    assert_eq!(public[0]["metrics"]["cpu"], 1.0, "the rest of the report still goes out");
    assert!(public[0]["metrics"].get("iface").is_none(), "the interface list is the panel's");

    let admin = visible_nodes(&app, true).unwrap();
    assert_eq!(admin.len(), 2);
    assert_eq!(admin[0]["ip"], "198.51.100.9");
    assert_eq!(admin[0]["remark"], "secret note");
    assert!(
        !serde_json::to_string(&admin).unwrap().contains("token-of-open"),
        "the token is read on its own, never beside what agents report"
    );
    // The panel reads `iface` and nothing else the contract leaves out.
    assert_eq!(admin[0]["metrics"]["iface"], "eth1");
    for hidden in ["boot_id", "net_rx_total", "hostname", "ip"] {
        assert!(admin[0]["metrics"].get(hidden).is_none(), "{hidden} is not part of the panel's frame");
    }
    // The traffic inside `metrics` is the hub's, whatever the report claimed.
    for view in [&public[0], &admin[0]] {
        assert_eq!(view["metrics"]["total_rx"], view["total_rx"]);
        assert_eq!(view["metrics"]["month_tx"], view["month_tx"]);
    }
}

/// One address per family, marked with where it came from.
#[test]
fn a_node_shows_the_address_it_is_reached_by() {
    let rows = |ip, held, pins| -> Vec<String> {
        addresses([ip, ""], held, pins).into_iter().map(|(a, source)| format!("{a} {source}")).collect()
    };
    let none = ("", "");
    // A public interface is the machine; a different exit in front of it is a
    // proxy and stays out.
    assert_eq!(
        rows("2001:db8::2", ("203.0.113.7", "2001:db8::2"), none),
        ["203.0.113.7 interface", "2001:db8::2 interface"]
    );
    assert_eq!(rows("198.51.100.1", ("203.0.113.7", ""), none), ["203.0.113.7 interface"]);
    // NAT: the exit replaces the private interface address, which nobody
    // outside can use.
    assert_eq!(rows("203.0.113.7", ("10.10.2.250", ""), none), ["203.0.113.7 exit"]);
    assert_eq!(rows("203.0.113.7", ("100.64.0.9", ""), none), ["203.0.113.7 exit"]);
    // An LXC guest behind NAT with a public /128, reached over v4 by a current
    // agent...
    assert_eq!(
        rows("203.0.113.7", ("10.10.1.5", "2401:b60:1c::5"), none),
        ["203.0.113.7 exit", "2401:b60:1c::5 interface"]
    );
    // ...and over v6 by an older one reporting the ULA ahead of it.
    assert_eq!(rows("2401:b60:1c::5", ("10.10.1.5", "fd42:43af::1"), none), ["2401:b60:1c::5 exit"]);
    // NAT66: a private v4 and a ULA v6, the session over v4 and a second
    // connection over v6. Each exit stands in for its own family's address,
    // and the one in a family the interface lacks is a translator again.
    let both = |held| -> Vec<String> {
        addresses(["203.0.113.7", "2001:db8:1::9"], held, none)
            .into_iter()
            .map(|(a, source)| format!("{a} {source}"))
            .collect()
    };
    assert_eq!(both(("10.0.0.5", "fd00::5")), ["203.0.113.7 exit", "2001:db8:1::9 exit"]);
    assert_eq!(both(("10.0.0.5", "")), ["203.0.113.7 exit"]);
    assert_eq!(both(("10.0.0.5", "2401:b60:1c::5")), ["203.0.113.7 exit", "2401:b60:1c::5 interface"]);
    // Behind a transparent proxy the exit is the proxy's; the home line can
    // only be set by hand.
    let home = ("192.168.1.5", "2409:8a1e::5");
    assert_eq!(rows("198.51.100.77", home, none), ["198.51.100.77 exit", "2409:8a1e::5 interface"]);
    assert_eq!(
        rows("198.51.100.77", home, ("203.0.113.50", "")),
        ["203.0.113.50 manual", "2409:8a1e::5 interface"]
    );
    // A pin wins over a public interface too, and may name a private address
    // for use on the LAN.
    assert_eq!(
        rows("", ("203.0.113.7", "2001:db8::5"), ("", "2001:db8::9")),
        ["203.0.113.7 interface", "2001:db8::9 manual"]
    );
    assert_eq!(rows("203.0.113.7", ("10.0.0.2", ""), ("10.0.0.2", "")), ["10.0.0.2 manual"]);
    // No interface in the exit's family: a translator (NAT64, WARP) that does
    // not lead to the machine.
    assert_eq!(rows("104.28.1.1", ("", "2001:db8::5"), none), ["2001:db8::5 interface"]);
    // An address reported under the other family's name is not that family's.
    assert_eq!(rows("203.0.113.7", ("2001:db8::5", ""), none), ["203.0.113.7 exit"]);
    // Nothing public anywhere: hub and node share a network, and the private
    // addresses are all there is.
    assert_eq!(rows("192.168.1.2", ("192.168.1.5", ""), none), ["192.168.1.5 interface"]);
    assert_eq!(rows("fd00::2", ("10.0.0.2", "fd00::5"), none), ["10.0.0.2 interface", "fd00::5 interface"]);
    assert_eq!(
        rows("198.18.0.1", ("192.168.1.5", ""), none),
        ["192.168.1.5 interface"],
        "a TUN proxy's fake-IP range is not public"
    );
    // With no interface reported the connection is all there is, and nothing
    // says it is not the machine's own.
    assert_eq!(rows("203.0.113.7", none, none), ["203.0.113.7 connection"]);
    assert_eq!(rows("", ("10.0.0.2", ""), none), ["10.0.0.2 interface"]);
    assert!(rows("", none, none).is_empty());

    // The panel gets the list, and per family what shows with the pin
    // cleared.
    let app = app();
    let id = node(&app, "n", true);
    app.db
        .save_facts(id, &json!({"ipv4": "10.10.1.5", "ipv6": "2401:b60:1c::5"}), "203.0.113.7", "")
        .unwrap();
    let patch: NodePatch = serde_json::from_value(json!({"ipv4_pin": "198.51.100.50"})).unwrap();
    app.db.update_node(id, &patch).unwrap();
    let view = &visible_nodes(&app, true).unwrap()[0];
    assert_eq!(
        view["addresses"],
        json!([{"address": "198.51.100.50", "source": "manual"}, {"address": "2401:b60:1c::5", "source": "interface"}])
    );
    assert_eq!((&view["ipv4_auto"], &view["ipv6_auto"]), (&json!("203.0.113.7"), &json!("2401:b60:1c::5")));

    // Private addresses show only when nothing public is known, so the pin
    // the other family keeps decides: v4 falls back to nothing while the v6
    // pin stands.
    let lan = node(&app, "lan", true);
    app.db.save_facts(lan, &json!({"ipv4": "192.168.1.5", "ipv6": "fd00::5"}), "192.168.1.2", "").unwrap();
    let patch: NodePatch = serde_json::from_value(json!({"ipv6_pin": "2001:db8::9"})).unwrap();
    app.db.update_node(lan, &patch).unwrap();
    let view = visible_nodes(&app, true).unwrap().into_iter().find(|v| v["id"] == lan).unwrap();
    assert_eq!((&view["ipv4_auto"], &view["ipv6_auto"]), (&json!(""), &json!("fd00::5")));
}

#[tokio::test]
async fn rotating_a_token_closes_the_session_the_old_one_opened() {
    let app = std::sync::Arc::new(app());
    let id = node(&app, "n", true);
    let mut rx = connect(&app, id, Value::Null);

    let response = reset_token(Admin, axum::extract::State(app.clone()), Path(id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let issued = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
    let read = node_token(Admin, axum::extract::State(app.clone()), Path(id)).await;
    assert_eq!(
        axum::body::to_bytes(read.into_body(), 1024).await.unwrap(),
        issued,
        "the panel reads the new token"
    );
    // The agent loop selects on this receiver, so a closed channel is how it
    // learns to stop. `try_recv`, because `recv().await` on a channel
    // incorrectly left open would hang the suite rather than fail it.
    assert!(
        matches!(rx.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)),
        "the old agent's channel must be closed"
    );
    assert!(app.agents.read().unwrap().is_empty(), "the node must read as offline at once");
}

/// A batch writes to every selected node or to none, accepts only what a
/// selection can share, and a group reaches the status page.
#[tokio::test]
async fn a_batch_edit_applies_to_all_selected_nodes_or_none() {
    let app = std::sync::Arc::new(app());
    let state = || axum::extract::State(app.clone());
    let (a, b, c) = (node(&app, "a", true), node(&app, "b", true), node(&app, "c", true));
    let batch = |ids: Vec<i64>, patch: Value| {
        Ok(Json(NodeBatch { ids, patch: serde_json::from_value(patch).unwrap() }))
    };
    let group = |id| app.db.node(id).unwrap().unwrap().group;

    let r =
        update_nodes(Admin, state(), batch(vec![a, b, a], json!({"group": " 香港 ", "notify": true}))).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!((group(a), group(b), group(c)), ("香港".into(), "香港".into(), String::new()));
    assert!(app.db.node(b).unwrap().unwrap().notify);

    // One id gone: nothing is written, not even to the nodes still there.
    let r = update_nodes(Admin, state(), batch(vec![a, 999], json!({"group": "东京"}))).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(group(a), "香港", "a refused batch leaves every node as it was");

    // Only the listed fields deserialize, so the extractor refuses the rest.
    for refused in [
        json!({"name": "x"}),
        json!({"ipv4_pin": "1.2.3.4"}),
        json!({"public": false}),
        json!({"public_remark": "x"}),
    ] {
        assert!(serde_json::from_value::<BatchPatch>(refused.clone()).is_err(), "{refused}");
    }
    // Counted in characters, not bytes: thirteen of them take 39 bytes.
    let r = update_nodes(Admin, state(), batch(vec![a], json!({"group": "港".repeat(MAX_GROUP)}))).await;
    assert_eq!(r.status(), StatusCode::OK);
    let r = update_nodes(Admin, state(), batch(vec![a], json!({"group": "港".repeat(MAX_GROUP + 1)}))).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        update_nodes(Admin, state(), batch(vec![], json!({"notify": false}))).await.status(),
        StatusCode::BAD_REQUEST
    );

    assert!(live_snapshot(&app, false).as_str().contains(r#""group":"香港""#), "the group is public");
}

/// The public note reaches visitors trimmed, and one too long for every
/// frame, or one carrying a line break, is refused rather than cut.
#[tokio::test]
async fn a_public_remark_is_bounded_and_reaches_visitors() {
    let app = std::sync::Arc::new(app());
    let id = node(&app, "n", true);
    let put = |text: String| {
        let patch = serde_json::from_value(json!({ "public_remark": text })).unwrap();
        update_node(Admin, axum::extract::State(app.clone()), Path(id), Ok(Json(patch)))
    };

    assert_eq!(put("港".repeat(MAX_PUBLIC_REMARK + 1)).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(put("一行\n两行".into()).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(put(" CN2 GIA ".into()).await.status(), StatusCode::OK);
    let public = &visible_nodes(&app, false).unwrap()[0];
    assert_eq!((&public["public_remark"], &public["remark"]), (&json!("CN2 GIA"), &Value::Null));
    // Counted in characters, not bytes.
    assert_eq!(put("港".repeat(MAX_PUBLIC_REMARK)).await.status(), StatusCode::OK);
}

/// What the panel saves is what an anonymous visitor reads, under the same
/// condition as the node list, and only an installed theme takes a write.
#[tokio::test]
async fn saved_theme_settings_reach_visitors_while_the_status_page_is_open() {
    let app = std::sync::Arc::new(app());
    let state = || axum::extract::State(app.clone());
    let read = |short: &str| theme_config(state(), HeaderMap::new(), Path(short.to_owned()));
    let body = |r: Response| async { axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap() };

    assert_eq!(&body(read("default").await).await[..], b"{}", "nothing saved reads as no overrides");
    let values = json!({"notice": "维护中", "show_price": false}).as_object().unwrap().clone();
    let saved = save_theme_config(Admin, state(), Path("default".into()), Json(values.clone())).await;
    assert_eq!(saved.status(), StatusCode::NO_CONTENT);
    let read_back: Value = serde_json::from_slice(&body(read("default").await).await).unwrap();
    assert_eq!(read_back, Value::Object(values.clone()));

    let missing = save_theme_config(Admin, state(), Path("aurora".into()), Json(values.clone())).await;
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST, "a theme that is not installed takes no write");
    assert_eq!(read("../etc").await.status(), StatusCode::NOT_FOUND);

    app.db.set("public_page", "off").unwrap();
    assert_eq!(
        read("default").await.status(),
        StatusCode::UNAUTHORIZED,
        "a closed status page hides them too"
    );
}

/// Days to expiry are counted on the hub's calendar and are public, like the
/// date itself; no date counts nothing.
#[test]
fn days_to_expiry_follow_the_hubs_calendar() {
    let app = app();
    let id = node(&app, "a", true);
    let expires_in = || visible_nodes(&app, false).unwrap()[0]["expires_in"].clone();
    assert_eq!(expires_in(), Value::Null);
    let today = Local::now().date_naive();
    for days in [3, 0, -1] {
        app.db.set_expiry(id, &(today + chrono::Duration::days(days)).to_string()).unwrap();
        assert_eq!(expires_in(), json!(days));
    }
}

/// A write naming a node that no longer exists, such as one deleted from
/// another tab, is refused rather than reported as saved.
#[tokio::test]
async fn writes_to_a_missing_node_are_not_found() {
    let app = std::sync::Arc::new(app());
    let state = || axum::extract::State(app.clone());
    let patch = Ok(Json(NodePatch { notify: Some(true), ..Default::default() }));
    assert_eq!(update_node(Admin, state(), Path(9), patch).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(delete_node(Admin, state(), Path(9)).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(reset_token(Admin, state(), Path(9)).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(node_token(Admin, state(), Path(9)).await.status(), StatusCode::NOT_FOUND);
    let traffic = Json(TrafficPatch { total_rx: Some(1), ..Default::default() });
    assert_eq!(patch_traffic(Admin, state(), Path(9), traffic).await.status(), StatusCode::NOT_FOUND);
}

/// Deleting a node must reach the connection it opened, for the same reason
/// rotating its token does, and a node created afterwards inherits nothing.
#[tokio::test]
async fn deleting_a_node_closes_its_session_so_the_next_id_does_not_inherit_it() {
    let app = std::sync::Arc::new(app());
    let old = node(&app, "old", true);
    let mut rx = connect(&app, old, json!({"cpu": 42.0}));

    assert_eq!(
        delete_node(Admin, axum::extract::State(app.clone()), Path(old)).await.status(),
        StatusCode::OK
    );
    assert!(
        matches!(rx.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)),
        "the deleted node's agent must be told to go"
    );

    let fresh = node(&app, "fresh", true);
    assert_ne!(fresh, old, "a deleted node's id is not issued again");
    let nodes = visible_nodes(&app, true).unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["online"], json!(false), "a node nobody deployed is not online");
    assert_eq!(nodes[0]["metrics"], Value::Null, "and it has nobody else's metrics");
}

/// Values set by hand take the place of automatic ones, so each is held to
/// what the automatic value would have to be, and of the three only the
/// country reaches the status page. The create path stores none of them.
#[tokio::test]
async fn values_set_by_hand_are_checked_and_only_the_country_goes_public() {
    let app = std::sync::Arc::new(app());
    let id = node(&app, "n", true);
    app.db.save_facts(id, &json!({}), "198.51.100.77", "198.51.100.77").unwrap();
    app.db.set_country(id, "SG", "198.51.100.77").unwrap();
    let put = |patch: Value| {
        update_node(
            Admin,
            axum::extract::State(app.clone()),
            Path(id),
            Ok(Json(serde_json::from_value(patch).unwrap())),
        )
    };
    for bad in [
        json!({"country_pin": "CHN"}),
        json!({"country_pin": "中国"}),
        json!({"country_pin": "C1"}),
        json!({"ipv4_pin": "2409:8a1e::5"}),
        json!({"ipv4_pin": "203.0.113.9:22"}),
        json!({"ipv4_pin": "203.0.113.256"}),
        json!({"ipv6_pin": "203.0.113.9"}),
        json!({"ipv6_pin": "[2409:8a1e::5]:22"}),
    ] {
        assert_eq!(put(bad.clone()).await.status(), StatusCode::BAD_REQUEST, "accepted {bad}");
    }
    let typed = json!({"country_pin": " cn ", "ipv4_pin": " 203.0.113.9 ", "ipv6_pin": "2409:8A1E:0::0088"});
    assert_eq!(put(typed).await.status(), StatusCode::OK);
    let stored = app.db.node(id).unwrap().unwrap();
    assert_eq!(
        (stored.country_pin.as_str(), stored.ipv4_pin.as_str(), stored.ipv6_pin.as_str()),
        ("CN", "203.0.113.9", "2409:8a1e::88"),
        "stored canonical"
    );

    let public = visible_nodes(&app, false).unwrap()[0].to_string();
    assert!(public.contains(r#""country":"CN""#), "the pin replaces the looked-up country: {public}");
    assert!(!public.contains("203.0.113.9") && !public.contains("2409:8a1e::88") && !public.contains("SG"));
    let full = &visible_nodes(&app, true).unwrap()[0];
    assert_eq!((full["country_auto"].as_str(), full["ipv4_pin"].as_str()), (Some("SG"), Some("203.0.113.9")));

    // A hello leaves the pins alone; empty returns each to automatic.
    app.db.save_facts(id, &json!({}), "198.51.100.88", "198.51.100.88").unwrap();
    assert_eq!(app.db.node(id).unwrap().unwrap().ipv4_pin, "203.0.113.9");
    assert_eq!(
        put(json!({"country_pin": "", "ipv4_pin": " ", "ipv6_pin": ""})).await.status(),
        StatusCode::OK
    );
    let stored = app.db.node(id).unwrap().unwrap();
    assert!(stored.country_pin.is_empty() && stored.ipv4_pin.is_empty() && stored.ipv6_pin.is_empty());
    assert_eq!(
        visible_nodes(&app, false).unwrap()[0]["country"],
        "",
        "back to the lookup, which the new address left owing"
    );
}

/// Both writers enforce the same limits, so nothing the update path refuses
/// is reachable through the create path, which takes a whole `Node`.
#[tokio::test]
async fn both_write_paths_refuse_the_same_out_of_range_values() {
    let app = std::sync::Arc::new(app());
    let id = node(&app, "n", true);
    for bad in [
        json!({"name": "x", "traffic_reset_day": 99}),
        json!({"name": "x", "price": -5.0}),
        json!({"name": "x", "traffic_limit": -1}),
        json!({"name": "x", "currency": "USDT"}),
        json!({"name": "x", "currency": "港币"}),
        json!({"name": "x", "billing_cycle": "0m"}),
        json!({"name": "x", "billing_cycle": "1201m"}),
        json!({"name": "x", "billing_cycle": "weekly"}),
        json!({"name": "名".repeat(MAX_NAME + 1)}),
    ] {
        let created = create_node(
            Admin,
            axum::extract::State(app.clone()),
            panel_headers(),
            Ok(Json(serde_json::from_value(bad.clone()).unwrap())),
        )
        .await;
        assert_eq!(created.status(), StatusCode::BAD_REQUEST, "create accepted {bad}");
        let updated = update_node(
            Admin,
            axum::extract::State(app.clone()),
            Path(id),
            Ok(Json(serde_json::from_value(bad.clone()).unwrap())),
        )
        .await;
        assert_eq!(updated.status(), StatusCode::BAD_REQUEST, "update accepted {bad}");
    }
    assert_eq!(app.db.nodes().unwrap().len(), 1, "nothing was created");
}

/// A length with a name is stored under it, so a theme built for hub 1.3.0
/// still labels it.
#[tokio::test]
async fn currency_and_cycle_are_stored_in_one_spelling() {
    let app = std::sync::Arc::new(app());
    let id = node(&app, "n", true);
    for (sent, currency, cycle) in [
        (json!({"currency": " hkd ", "billing_cycle": "60m"}), "HKD", "60m"),
        (json!({"billing_cycle": "12m"}), "HKD", "yearly"),
        (json!({"billing_cycle": "once"}), "HKD", "once"),
    ] {
        let put =
            update_node(Admin, State(app.clone()), Path(id), Ok(Json(serde_json::from_value(sent).unwrap())));
        assert_eq!(put.await.status(), StatusCode::OK);
        let stored = app.db.node(id).unwrap().unwrap();
        assert_eq!((stored.currency.as_str(), stored.billing_cycle.as_str()), (currency, cycle));
    }
}

/// A stream outlives the request that opened it, so everything the handshake
/// tested must be re-read rather than captured -- both answers, not one. The
/// admin frame carries every node's address and private notes, and the public frame
/// is what switching the status page off is meant to withdraw; a socket
/// surviving either decision would continue sending what was withdrawn.
#[test]
fn a_stream_re_reads_both_answers_its_handshake_tested() {
    let app = app();
    let hash = sha256("live-token");
    app.db.create_session(&hash, Utc::now().timestamp() + 3_600).unwrap();

    assert_eq!(stream_audience(&app, Some(&hash)), Some(true), "a live session gets the admin frame");
    assert_eq!(stream_audience(&app, None), Some(false), "an anonymous stream gets the public one");

    // Signing out, another device revoking this one, a password change and a
    // restore all manifest as this row disappearing.
    app.db.drop_session(&hash).unwrap();
    assert_eq!(stream_audience(&app, Some(&hash)), None, "a revoked session must end its stream");

    // The other half. `live_ws` refuses a new anonymous connection from here
    // and `nodes` answers 401, so a stream that continued was the only
    // remaining route, for as long as the tab stayed open.
    app.db.create_session(&hash, Utc::now().timestamp() + 3_600).unwrap();
    app.db.set("public_page", "off").unwrap();
    assert_eq!(stream_audience(&app, None), None, "closing the status page must end anonymous streams");
    assert_eq!(stream_audience(&app, Some(&hash)), Some(true), "a signed-in operator still gets theirs");
}

#[tokio::test]
async fn a_closed_status_page_and_a_sibling_origin_see_no_admin_data() {
    let app = std::sync::Arc::new(app());
    app.db.create_session(&sha256("live-token"), Utc::now().timestamp() + 3_600).unwrap();
    let signed_in = |site: &str| {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, format!("{}=live-token", crate::auth::COOKIE).parse().unwrap());
        headers.insert("sec-fetch-site", site.parse().unwrap());
        headers
    };
    let operator = Some(Some(sha256("live-token")));
    assert_eq!(stream_viewer(&app, &signed_in("same-origin")), operator);
    assert_eq!(stream_viewer(&app, &signed_in("same-site")), Some(None), "a sibling's script is a visitor");
    assert_eq!(stream_viewer(&app, &HeaderMap::new()), Some(None));

    app.db.set("public_page", "off").unwrap();
    assert_eq!(stream_viewer(&app, &signed_in("same-origin")), operator);
    assert_eq!(stream_viewer(&app, &signed_in("same-site")), None);
    assert_eq!(stream_viewer(&app, &HeaderMap::new()), None);
    assert_eq!(nodes(State(app.clone()), HeaderMap::new()).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(nodes(State(app), signed_in("same-origin")).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn only_the_admin_node_list_is_marked_no_transform() {
    let app = std::sync::Arc::new(app());
    app.db.create_session(&sha256("live-token"), Utc::now().timestamp() + 3_600).unwrap();
    let mut signed_in = HeaderMap::new();
    signed_in.insert(header::COOKIE, format!("{}=live-token", crate::auth::COOKIE).parse().unwrap());
    let cache = |res: Response| res.headers().get(header::CACHE_CONTROL).cloned();
    assert_eq!(cache(nodes(State(app.clone()), signed_in).await).unwrap(), "no-store, no-transform");
    assert_eq!(cache(nodes(State(app), HeaderMap::new()).await), None, "the public list compresses");
}

/// The public list a browser asks for gzipped is the stream's compressed
/// frame, the same bytes for every request until the frame expires.
#[tokio::test]
async fn the_public_node_list_reuses_the_compressed_frame() {
    let app = std::sync::Arc::new(app());
    node(&app, "listed", true);
    let ask = |encoding: &'static str| {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, encoding.parse().unwrap());
        headers
    };
    let res = nodes(State(app.clone()), ask("br, gzip, deflate")).await;
    assert_eq!(res.headers()[header::CONTENT_ENCODING], "gzip");
    assert_eq!(res.headers()[header::VARY], "accept-encoding");
    let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    assert_eq!(body, live_gzip(&app), "the frame the streams send");

    for refused in ["identity", "gzip;q=0", "br", "*"] {
        let res = nodes(State(app.clone()), ask(refused)).await;
        assert!(res.headers().get(header::CONTENT_ENCODING).is_none(), "{refused}");
    }
    assert!(nodes(State(app.clone()), ask("GZIP; q=0.5"))
        .await
        .headers()
        .contains_key(header::CONTENT_ENCODING));

    // The admin list is never compressed, asked or not.
    app.db.create_session(&sha256("live-token"), Utc::now().timestamp() + 3_600).unwrap();
    let mut signed_in = ask("gzip");
    signed_in.insert(header::COOKIE, format!("{}=live-token", crate::auth::COOKIE).parse().unwrap());
    assert!(nodes(State(app), signed_in).await.headers().get(header::CONTENT_ENCODING).is_none());
}

#[test]
fn the_shared_snapshot_keeps_the_two_audiences_apart() {
    let app = app();
    let open = node(&app, "open", true);
    node(&app, "hidden", false);
    app.db.save_facts(open, &json!({"hostname": "vps-1"}), "198.51.100.9", "").unwrap();

    let public = live_snapshot(&app, false);
    let admin = live_snapshot(&app, true);
    // Caching must never let one audience's payload reach the other.
    assert!(!public.as_str().contains("198.51.100.9"), "the public frame must carry no address");
    assert!(!public.as_str().contains("hidden"), "the public frame must carry no private node");
    assert!(admin.as_str().contains("198.51.100.9") && admin.as_str().contains("hidden"));

    // Two reads over unchanged data prove nothing, since a rebuild returns the
    // same bytes, so the data is modified first.
    node(&app, "late", true);
    assert_eq!(live_snapshot(&app, false), public, "the frame is reused, not rebuilt per viewer");

    let mut unpacked = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&live_gzip(&app)[..]), &mut unpacked)
        .unwrap();
    assert_eq!(unpacked, public.as_str(), "the gzipped frame is the same frame");
    assert_eq!(live_gzip(&app).as_ptr(), live_gzip(&app).as_ptr(), "compressed once per frame");
}

#[test]
fn a_clock_stepping_backwards_does_not_pin_a_stale_frame() {
    let app = app();
    node(&app, "first", true);
    live_snapshot(&app, false);

    // NTP correcting a fresh boot leaves the cached stamp in the future, which
    // does not constitute a young frame.
    app.snapshot.lock().unwrap()[0].at = Utc::now().timestamp_millis() + 60_000;
    node(&app, "added-after", true);
    assert!(live_snapshot(&app, false).as_str().contains("added-after"));
}

/// The panel sends only a name, and expects the node just added to appear in
/// the frame it is already streaming.
#[tokio::test]
async fn a_node_added_from_the_panel_needs_only_a_name_and_shows_up_at_once() {
    let app = std::sync::Arc::new(app());
    node(&app, "existing", true);
    assert!(!live_snapshot(&app, true).as_str().contains("added"));

    let added: Node = serde_json::from_value(json!({"name": "added"})).unwrap();
    // The defaults the panel relies on by omitting them, `public` above all:
    // the alternative would publish a node that was never published.
    assert!(added.public);
    assert_eq!(added.billing_cycle, "monthly");
    assert_eq!(added.traffic_reset_day, 1);

    let created = create_node(Admin, State(app.clone()), panel_headers(), Ok(Json(added))).await;
    assert_eq!(created.status(), StatusCode::OK);
    // Frames are cached for nearly two seconds, so without dropping the cache
    // the node just added would disappear from the list.
    assert!(live_snapshot(&app, true).as_str().contains("added"));

    // A name consisting only of spaces is refused and leaves no node behind.
    let blank = Json(serde_json::from_value::<Node>(json!({"name": "   "})).unwrap());
    let refused = create_node(Admin, State(app.clone()), panel_headers(), Ok(blank)).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(app.db.nodes().unwrap().len(), 2);
}

/// Every gate on the anonymous route, in the order a batch install encounters
/// them: closed, wrong key, open, expired, closed manually.
#[tokio::test]
async fn registration_only_works_inside_a_window_the_panel_opened() {
    let app = std::sync::Arc::new(app());
    let register = |key: Option<&str>, name: &str| {
        let mut headers = HeaderMap::new();
        if let Some(key) = key {
            headers.insert("authorization", format!("Bearer {key}").parse().unwrap());
        }
        agent_register(
            State(app.clone()),
            ConnectInfo("198.51.100.7:40000".parse().unwrap()),
            headers,
            name.to_owned(),
        )
    };

    // Nothing was opened, so no key is correct.
    assert_eq!(register(Some("guess"), "a").await.status(), StatusCode::FORBIDDEN);
    assert!(app.db.nodes().unwrap().is_empty());

    assert_eq!(open_register(Admin, State(app.clone()), panel_headers()).await.status(), StatusCode::OK);
    let key = app.db.get("register_key").unwrap();
    assert!(register_left(&app) > REGISTER_WINDOW - 5, "the panel counts down from the full hour");
    assert_eq!(register(Some("guess"), "a").await.status(), StatusCode::FORBIDDEN);
    assert_eq!(register(None, "a").await.status(), StatusCode::FORBIDDEN);
    assert!(app.db.nodes().unwrap().is_empty());

    let issued = register(Some(&key), "  web-01\n").await;
    assert_eq!(issued.status(), StatusCode::OK);
    let token = axum::body::to_bytes(issued.into_body(), usize::MAX).await.unwrap().to_vec();
    let token = String::from_utf8(token).unwrap();
    // The purpose of the route: what returned is a token an agent can connect
    // with, not merely a 200.
    let id = app.db.node_by_token(&token).unwrap().expect("token opens a node");
    let node = app.db.nodes().unwrap().into_iter().find(|n| n.id == id).unwrap();
    assert_eq!(node.name, "web-01");
    // Registered nodes take the panel's defaults rather than `Node::default()`.
    assert!(node.public);
    assert_eq!(node.traffic_reset_day, 1);

    // An hour later the same key is worthless, which is what makes leaving the
    // window open harmless.
    app.db.set("register_until", &(Utc::now().timestamp() - 1).to_string()).unwrap();
    assert_eq!(register_left(&app), 0);
    assert_eq!(register(Some(&key), "b").await.status(), StatusCode::FORBIDDEN);

    // Reopened, then closed manually: the key from the open window stops
    // working.
    open_register(Admin, State(app.clone()), panel_headers()).await;
    let key = app.db.get("register_key").unwrap();
    assert_eq!(close_register(Admin, State(app.clone())).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(register(Some(&key), "c").await.status(), StatusCode::FORBIDDEN);
    assert_eq!(app.db.nodes().unwrap().len(), 1);
}

/// A rerun of the batch command on a registered machine keeps its node, also
/// after the window closed, until that node is deleted from the panel.
#[tokio::test]
async fn a_rerun_keeps_its_node_until_the_node_is_deleted() {
    let app = std::sync::Arc::new(app());
    let register = |key: &str, held: &str| {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {key}").parse().unwrap());
        // curl omits a header whose value is empty, so a machine holding no
        // token sends none.
        if !held.is_empty() {
            headers.insert(HELD_TOKEN, held.parse().unwrap());
        }
        agent_register(
            State(app.clone()),
            ConnectInfo("198.51.100.7:40000".parse().unwrap()),
            headers,
            "web-01".to_owned(),
        )
    };
    let text = |r: Response| async {
        String::from_utf8(axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap()
    };

    open_register(Admin, State(app.clone()), panel_headers()).await;
    let key = app.db.get("register_key").unwrap();
    let token = text(register(&key, "").await).await;
    let id = app.db.node_by_token(&token).unwrap().expect("token opens a node");

    assert_eq!(text(register(&key, &token).await).await, token);
    close_register(Admin, State(app.clone())).await;
    assert_eq!(text(register(&key, &token).await).await, token, "the token outlives its window");
    assert_eq!(app.db.nodes().unwrap().len(), 1);

    // Deleted: the held token no longer answers, so a closed window refuses
    // rather than handing back a token the agent would be refused with.
    app.db.delete_node(id).unwrap();
    assert_eq!(register(&key, &token).await.status(), StatusCode::FORBIDDEN);
    open_register(Admin, State(app.clone()), panel_headers()).await;
    let key = app.db.get("register_key").unwrap();
    let fresh = text(register(&key, &token).await).await;
    assert_ne!(fresh, token);
    assert!(app.db.node_by_token(&fresh).unwrap().is_some());
    assert_eq!(app.db.nodes().unwrap().len(), 1);
}

/// The name comes from an unvouched machine: markup characters are dropped,
/// and what that leaves is trimmed. None at all, the machine's hostname
/// withheld by `install.sh`, names the node after its id.
#[tokio::test]
async fn a_registered_name_loses_markup_and_is_never_blank() {
    let app = std::sync::Arc::new(app());
    open_register(Admin, State(app.clone()), panel_headers()).await;
    let key = app.db.get("register_key").unwrap();
    for (sent, stored) in
        [("", "节点 1"), ("< >", "节点 2"), ("<b>web</b>", "bweb/b"), ("Tom's \"VPS\"", "Toms VPS")]
    {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {key}").parse().unwrap());
        let reply = agent_register(
            State(app.clone()),
            ConnectInfo("198.51.100.7:40000".parse().unwrap()),
            headers,
            sent.to_owned(),
        )
        .await;
        let token =
            String::from_utf8(axum::body::to_bytes(reply.into_body(), usize::MAX).await.unwrap().to_vec())
                .unwrap();
        let id = app.db.node_by_token(&token).unwrap().expect("token opens a node");
        assert_eq!(app.db.node(id).unwrap().unwrap().name, stored, "sent {sent:?}");
    }
}

/// The ceiling on the anonymous route: a leaked key cannot fill the table.
#[tokio::test]
async fn one_window_stops_registering_at_the_limit() {
    let app = std::sync::Arc::new(app());
    open_register(Admin, State(app.clone()), panel_headers()).await;
    let key = app.db.get("register_key").unwrap();
    for i in 0..REGISTER_LIMIT {
        node(&app, &format!("n{i}"), true);
    }
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {key}").parse().unwrap());
    let refused = agent_register(
        State(app.clone()),
        ConnectInfo("198.51.100.7:40000".parse().unwrap()),
        headers,
        "one-too-many".to_owned(),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(app.db.nodes().unwrap().len() as i64, REGISTER_LIMIT);
}

#[test]
fn a_node_view_carries_traffic_even_while_offline() {
    let app = app();
    let id = node(&app, "n", true);
    app.db.accumulate(id, "b", (100, 100), Local::now(), None).unwrap();
    app.db.accumulate(id, "b", (900, 500), Local::now(), None).unwrap();
    app.db.touch_seen(id, 1_700_000_000, &serde_json::Value::Null).unwrap();

    let view = &visible_nodes(&app, true).unwrap()[0];
    assert_eq!(view["online"], false);
    assert_eq!(view["metrics"], Value::Null);
    assert_eq!(view["total_rx"], 800, "traffic is stored, not derived from the live state");
    assert_eq!(view["total_tx"], 400);
    // The live entry went with the connection, so "offline since" must come
    // from the node row.
    assert_eq!(view["last_seen"], 1_700_000_000);
    let ago = view["last_seen_ago"].as_i64().unwrap();
    assert!((Utc::now().timestamp() - 1_700_000_000 - ago).abs() <= 1, "counted on the hub's clock");

    node(&app, "never", true);
    assert_eq!(visible_nodes(&app, true).unwrap()[1]["last_seen_ago"], Value::Null, "never seen");
}

/// The stored capacities trail the reports by up to a minute, and a disk is
/// mounted on a running machine at any moment, which the agent detects by
/// re-reading its mount table every sample. Drawn from the stored copy, the
/// card and the detail page would show the same host two different sizes.
#[test]
fn a_capacity_that_changed_since_the_last_write_is_the_reported_one() {
    let app = app();
    let id = node(&app, "n", true);
    // What the last write stored: 30 GB of disk, 1 GB of swap.
    app.db
        .touch_seen(id, 1, &json!({"mem_total": 1_000, "swap_total": 1i64 << 30, "disk_total": 30i64 << 30}))
        .unwrap();

    let offline = &visible_nodes(&app, true).unwrap()[0];
    assert_eq!(offline["disk_total"], 30i64 << 30, "with nobody connected the stored facts are all there is");

    // A 5 GB volume is mounted and swap is disabled, before the next write.
    let _held = connect(
        &app,
        id,
        json!({"mem_total": 1_000, "swap_total": 0, "disk_total": 35i64 << 30, "cpu": 1.0}),
    );
    let live = &visible_nodes(&app, true).unwrap()[0];
    assert_eq!(live["disk_total"], 35i64 << 30, "the report is the truth while the agent is connected");
    assert_eq!(live["swap_total"], 0, "swapoff means zero, not the gigabyte that was there at connect");
    assert_eq!(live["disk_total"], live["metrics"]["disk_total"], "one number, not two");
    assert_eq!(app.db.node(id).unwrap().unwrap().disk_total, 30i64 << 30, "and no extra write to get it");
}

/// Only anonymous streams are counted, and a slot comes back when its
/// stream ends. `PUBLIC_STREAM_GATE` is touched by no other test.
#[test]
fn anonymous_streams_past_the_gate_are_refused_and_a_session_is_not() {
    let held: Vec<_> = (0..PUBLIC_STREAMS).map(|_| stream_slot(None).expect("up to the limit")).collect();
    assert!(stream_slot(None).is_err(), "the stream past the limit must be refused");
    assert!(stream_slot(Some("hash")).is_ok(), "a session opens its stream with every slot taken");
    drop(held);
    assert!(stream_slot(None).is_ok(), "a finished stream gives its slot back");
}

/// `span` bounds one window; this bounds how many are built concurrently.
/// Each holds the reader for its entire scan, and the path takes no
/// credentials. `PASSWORD_GATE` refuses the same way.
#[tokio::test]
async fn history_queries_past_the_gate_are_refused_rather_than_queued() {
    let _serial = HISTORY_TESTS.lock().await;
    let app = std::sync::Arc::new(app());
    let id = node(&app, "n", true);
    let ask = || {
        metrics(
            State(app.clone()),
            HeaderMap::new(),
            Path(id),
            Query(Window { hours: 1, points: None, series: None }),
        )
    };

    let held: Vec<_> =
        (0..HISTORY_SLOTS).map(|_| HISTORY_GATE.try_acquire().expect("up to the limit")).collect();
    assert_eq!(ask().await.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(held);
    assert_eq!(ask().await.status(), StatusCode::OK, "a finished query gives its slot back");

    // An unauthorised caller is told so rather than asked to retry later: the
    // gate sits behind the visibility check deliberately.
    app.db.set("public_page", "off").unwrap();
    let held: Vec<_> =
        (0..HISTORY_SLOTS).map(|_| HISTORY_GATE.try_acquire().expect("up to the limit")).collect();
    assert_eq!(ask().await.status(), StatusCode::UNAUTHORIZED);
    drop(held);
}

/// A settings write lands whole or not at all. Changing the password drops
/// every session and places the replacement cookie on the response, so a 400
/// raised afterwards -- on a later key, in whatever order the map iterates --
/// would sign the admin out of every device without explanation.
#[tokio::test]
async fn a_settings_write_is_all_or_nothing() {
    let app = std::sync::Arc::new(app());
    app.db.set("admin_password_hash", "the-old-hash").unwrap();
    let headers = live_admin_session(&app, "settings-writer");
    let save = |body: Value| save_settings(Admin, State(app.clone()), headers.clone(), Json(body));

    // BTreeMap order places the password first, which is the failing case.
    let refused = save(json!({"admin_password": "a-long-enough-one", "retention_days": "abc"})).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(app.db.get("admin_password_hash").as_deref(), Some("the-old-hash"));

    // A correctly named key carrying the wrong type is refused rather than
    // discarded while the response reports success.
    let refused = save(json!({"public_page": false})).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(app.db.get("public_page"), None);

    let saved = save(json!({"public_page": "off", "retention_days": "7"})).await;
    assert_eq!(saved.status(), StatusCode::OK);
    assert_eq!(app.db.get("retention_days").as_deref(), Some("7"));
}

/// The install script and the sign-in page keep separate counters: five
/// machines started with a stale key is a misconfigured deploy, and a shared
/// counter would lock the operator out of the panel for the lockout window.
#[tokio::test]
async fn a_wrong_registration_key_does_not_lock_the_sign_in_page() {
    let app = std::sync::Arc::new(app());
    app.db.set("register_key", "the-key").unwrap();
    app.db.set("register_until", &(Utc::now().timestamp() + 60).to_string()).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer wrong".parse().unwrap());
    let peer: std::net::SocketAddr = "198.51.100.7:9000".parse().unwrap();

    // The attempt after the last permitted one answers 429 rather than 403.
    for _ in 0..5 {
        let refused =
            agent_register(State(app.clone()), ConnectInfo(peer), headers.clone(), "n".into()).await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    }
    assert!(app.registrations.locked(peer.ip()), "the register route counts its own failures");
    assert!(!app.throttle.locked(peer.ip()), "and the panel's sign-in page is not one of them");
}

#[test]
fn per_node_reads_follow_the_public_flag_and_the_public_page_switch() {
    let app = app();
    let open = node(&app, "open", true);
    let hidden = node(&app, "hidden", false);

    assert!(readable(&app, false, open), "a published node is readable by anyone");
    assert!(!readable(&app, false, hidden), "a private node is not");
    assert!(!readable(&app, false, 9999), "an unknown id is not");
    assert!(readable(&app, true, hidden), "the panel sees a private node");

    // Switching the public page off closes even a published node.
    app.db.set("public_page", "off").unwrap();
    assert!(!readable(&app, false, open));
    assert!(readable(&app, true, open), "and never closes it for the panel");
}

/// The retention window is the ceiling for every caller: no width reads more
/// than the week of minute rows, so wider windows need no separate bound for
/// anonymous callers, and a window past what is kept would only draw
/// history that is not there.
#[tokio::test]
async fn a_history_window_stops_at_the_retention_window() {
    let _serial = HISTORY_TESTS.lock().await;
    let app = std::sync::Arc::new(app());
    let id = node(&app, "n", true);
    let now = Utc::now().timestamp();
    // One sample per day for a month, so a row's presence identifies its
    // window. The minute of slack keeps day seven clear of the 168-hour cutoff:
    // exactly on the boundary, a second elapsing between these inserts and the
    // query below would drop it and leave the count one short.
    for day in 0..30 {
        app.db.insert_metric(id, now - day * 86_400 + 60, &json!({"cpu": 1.0})).unwrap();
    }
    let ask = |hours| {
        let query = format!("hours={hours}&series=metrics");
        let app = app.clone();
        async move {
            let answer = metrics(
                State(app),
                HeaderMap::new(),
                Path(id),
                Query(serde_urlencoded::from_str::<Window>(&query).unwrap()),
            )
            .await;
            axum::body::to_bytes(answer.into_body(), usize::MAX).await.unwrap()
        }
    };
    let rows =
        |body: &[u8]| serde_json::from_slice::<Value>(body).unwrap()["metrics"].as_array().unwrap().len();
    let step = |body: &[u8]| serde_json::from_slice::<Value>(body).unwrap()["step"].clone();

    // Before the first rollup nothing is folded, and a window past the week
    // reads the newest week of minute rows rather than every one.
    app.db.set("retention_days", "30").unwrap();
    assert_eq!(rows(&ask(720).await), 8, "unfolded minute rows past the week are not read");
    app.db.roll_up(now, 90).unwrap();

    app.db.set("retention_days", "7").unwrap();
    let week = ask(168).await;
    assert_eq!(rows(&week), 8, "a week reaches back seven days");
    assert_eq!(step(&week), 420, "in seven-minute points, a week of minutes at the 1,440-point budget");
    assert_eq!(ask(2_160).await, week, "a window past the retention window is narrowed to it");

    // Past the week, from the hourly tier and the minute rows after it.
    app.db.set("retention_days", "30").unwrap();
    let month = ask(2_160).await;
    assert_eq!(rows(&month), 30, "a month reaches back thirty days");
    assert_eq!(step(&month), 3_600, "in whole hours, as no hourly row may straddle two points");
}

fn replacement_session(response: &Response) -> &str {
    response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("monitor_session=")
        .unwrap()
}

#[tokio::test]
async fn a_revoked_settings_request_cannot_write_or_restore_its_access() {
    let app = std::sync::Arc::new(app());
    app.db.set("github_allowed_users", "alice,bob").unwrap();
    let kept = live_admin_session(&app, "bob-token");
    let removed = live_admin_session(&app, "alice-token");
    let remove = save_settings(
        Admin,
        State(app.clone()),
        kept,
        Json(json!({"github_allowed_users": "bob", "site_name": "approved"})),
    )
    .await;
    assert_eq!(remove.status(), StatusCode::OK);
    let replacement = replacement_session(&remove);

    // The Admin extractor completed before the removal, but the
    // handler received the rest of the body only after it committed.
    let late = save_settings(
        Admin,
        State(app.clone()),
        removed,
        Json(json!({"github_allowed_users": "alice", "site_name": "late"})),
    )
    .await;
    assert_eq!(late.status(), StatusCode::UNAUTHORIZED);
    assert!(late.headers().get(header::SET_COOKIE).is_none());
    assert_eq!(app.db.get("github_allowed_users").as_deref(), Some("bob"));
    assert_eq!(app.db.get("site_name").as_deref(), Some("approved"));
    assert_eq!(app.db.sessions().unwrap().len(), 1);
    assert!(app.db.session_valid(&sha256(replacement)));
    assert!(!app.db.session_valid(&sha256("alice-token")));
}

#[tokio::test]
async fn password_and_allowlist_share_one_settings_and_session_commit() {
    let app = std::sync::Arc::new(app());
    app.db.set("admin_password_hash", "old-hash").unwrap();
    app.db.set("github_allowed_users", "alice,bob").unwrap();
    app.db.bind_github("alice", "1").unwrap();
    let writer = live_admin_session(&app, "writer");
    live_admin_session(&app, "alice");
    let save = || {
        save_settings(
            Admin,
            State(app.clone()),
            writer.clone(),
            Json(
                json!({"admin_password": "a-long-enough-password", "github_allowed_users": "bob", "site_name": "saved"}),
            ),
        )
    };
    app.db
        .conn()
        .execute_batch(
            "CREATE TRIGGER reject_replacement BEFORE INSERT ON session
         BEGIN SELECT RAISE(ABORT, 'fixture cannot insert the replacement'); END;",
        )
        .unwrap();
    let refused = save().await;
    assert_eq!(refused.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(refused.headers().get(header::SET_COOKIE).is_none());
    assert_eq!(app.db.get("admin_password_hash").as_deref(), Some("old-hash"));
    assert_eq!(app.db.get("github_allowed_users").as_deref(), Some("alice,bob"));
    assert_eq!(app.db.get(crate::auth::GITHUB_IDS).as_deref(), Some("alice:1"));
    assert!(app.db.get("site_name").is_none());
    assert!(app.db.session_valid(&sha256("writer")));
    assert!(app.db.session_valid(&sha256("alice")));

    app.db.conn().execute_batch("DROP TRIGGER reject_replacement").unwrap();
    let saved = save().await;
    assert_eq!(saved.status(), StatusCode::OK);
    assert_eq!(saved.headers().get_all(header::SET_COOKIE).iter().count(), 1);
    assert_eq!(app.db.sessions().unwrap().len(), 1);
    assert!(app.db.session_valid(&sha256(replacement_session(&saved))));
    assert!(!app.db.session_valid(&sha256("writer")));
    assert!(!app.db.session_valid(&sha256("alice")));
    assert!(app.db.get("admin_password_hash").unwrap().starts_with("$argon2id$"));
    assert_eq!(app.db.get("github_allowed_users").as_deref(), Some("bob"));
    assert_eq!(app.db.get(crate::auth::GITHUB_IDS).as_deref(), Some(""));
    assert_eq!(app.db.get("site_name").as_deref(), Some("saved"));
}

#[tokio::test]
async fn changing_the_password_kills_other_sessions_but_not_the_caller() {
    let app = std::sync::Arc::new(app());
    let stale = random_token();
    app.db.create_session(&sha256(&stale), Utc::now().timestamp() + 3_600).unwrap();

    let caller = live_admin_session(&app, "password-writer");
    let body = Json(json!({"admin_password": "a-long-enough-password"}));
    let response = save_settings(Admin, State(app.clone()), caller, body).await;

    assert!(!app.db.session_valid(&sha256(&stale)), "sessions must not outlive the old password");

    // The caller receives a replacement rather than being logged out by its
    // own password change.
    assert!(app.db.session_valid(&sha256(replacement_session(&response))));
    assert!(!app.db.session_valid(&sha256("password-writer")));
}

/// The panel hides the delete button on the caller's own row, so the mark is
/// all that prevents an admin from signing themselves out.
#[tokio::test]
async fn the_session_list_marks_the_caller_and_hides_expired_rows() {
    let app = std::sync::Arc::new(app());
    let (mine, theirs, stale) = (random_token(), random_token(), random_token());
    let now = Utc::now().timestamp();
    app.db.create_session(&sha256(&mine), now + 3_600).unwrap();
    app.db.create_session(&sha256(&theirs), now + 7_200).unwrap();
    app.db.create_session(&sha256(&stale), now - 1).unwrap();

    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, format!("monitor_session={mine}").parse().unwrap());
    let body = axum::body::to_bytes(
        sessions(Admin, axum::extract::State(app.clone()), headers).await.into_body(),
        usize::MAX,
    )
    .await
    .unwrap();
    let rows: Vec<Value> = serde_json::from_slice(&body).unwrap();

    assert_eq!(rows.len(), 2, "an expired session is not a session");
    assert_eq!(rows[0]["id"], sha256(&theirs), "newest first");
    assert_eq!(rows[0]["current"], false);
    assert_eq!(rows[1]["id"], sha256(&mine));
    assert_eq!(rows[1]["current"], true, "the caller's own row must be marked");
    assert_eq!(rows[1]["created_at"].as_i64().unwrap(), now + 3_600 - 14 * 86_400);

    delete_session(Admin, axum::extract::State(app.clone()), Path(sha256(&theirs))).await;
    assert!(!app.db.session_valid(&sha256(&theirs)), "the deleted device is signed out");
    assert!(app.db.session_valid(&sha256(&mine)), "and nobody else is");
}

#[tokio::test]
async fn a_short_password_is_refused_and_changes_nothing() {
    let app = std::sync::Arc::new(app());
    let live = random_token();
    let headers = live_admin_session(&app, &live);
    let body = Json(json!({"admin_password": "short"}));
    let response = save_settings(Admin, State(app.clone()), headers, body).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(app.db.get("admin_password_hash").is_none(), "the password must not have changed");
    assert!(app.db.session_valid(&sha256(&live)), "a rejected change must not log anyone out");
}

/// Housekeeping clamps whatever it finds, so an unparsable value is not an
/// error downstream: it silently means the default, in a field still
/// displaying what was entered.
#[tokio::test]
async fn a_retention_window_that_would_never_apply_is_refused() {
    let app = std::sync::Arc::new(app());
    let headers = live_admin_session(&app, "retention-writer");
    let put = |v: &str| {
        save_settings(
            Admin,
            State(app.clone()),
            headers.clone(),
            Json(json!({"retention_days": v.to_owned()})),
        )
    };
    for junk in ["", "abc", "0", "-1", "9999"] {
        assert_eq!(put(junk).await.status(), StatusCode::BAD_REQUEST, "{junk:?}");
    }
    assert!(app.db.get("retention_days").is_none(), "a refused window must not be stored");
    assert_eq!(put("7").await.status(), StatusCode::OK);
    assert_eq!(app.db.get("retention_days").as_deref(), Some("7"));
}

/// What `settings` returns must be what `save_settings` accepts. The panel
/// echoes the whole form back and the write is all-or-nothing, so one key
/// returned in a form the write refuses fails the entire page, naming a field
/// that was never edited.
#[tokio::test]
async fn a_fresh_hub_answers_settings_that_it_will_take_back() {
    let app = std::sync::Arc::new(app());
    let Json(read) = settings(Admin, State(app.clone()), peer(), HeaderMap::new()).await;
    assert_eq!(read["retention_days"], "30", "the default belongs in the answer, not in each caller");
    assert_eq!(read["your_address"], "198.51.100.7");

    // Exactly what the panel sends, on a hub where nothing was ever set.
    let echoed = json!({
        "site_name": read["site_name"],
        "retention_days": read["retention_days"],
        "github_proxy": read["github_proxy"],
        "public_page": "on",
        "notify_grace": read["notify_grace"],
        "notify_traffic": read["notify_traffic"],
        "notify_expiry": read["notify_expiry"],
        "notify_login": read["notify_login"],
        "notify_management": read["notify_management"],
        "notify_pm_thresholds": read["notify_pm_thresholds"],
        "notify_telegram_chat": read["notify_telegram_chat"],
        "notify_telegram_text": read["notify_telegram_text"],
        "notify_webhook_body": read["notify_webhook_body"],
    });
    assert_eq!(
        save_settings(Admin, State(app.clone()), live_admin_session(&app, "fresh-admin"), Json(echoed))
            .await
            .status(),
        StatusCode::OK,
        "a fresh hub's own settings must survive a round trip"
    );
    assert_eq!(app.db.retention_days(), 30, "and the stored window is the one that was shown");
}

#[tokio::test]
async fn settings_never_hand_back_a_secret() {
    let app = app();
    app.db.set("github_client_secret", "super-secret").unwrap();
    app.db.set("github_client_id", "public-id").unwrap();
    app.db.set("notify_telegram_token", "123:bot-secret").unwrap();
    app.db.set("notify_webhook_url", "https://hooks.example/url-secret").unwrap();
    app.db.set("notify_webhook_headers", "Authorization: header-secret").unwrap();

    let Json(body) =
        settings(Admin, axum::extract::State(std::sync::Arc::new(app)), peer(), HeaderMap::new()).await;
    assert_eq!(body["github_client_id"], "public-id");
    assert_eq!(body["github_secret_set"], true);
    assert_eq!(body["notify_webhook_url_set"], true);
    assert!(body.get("github_client_secret").is_none());
    for secret in ["super-secret", "bot-secret", "url-secret", "header-secret"] {
        assert!(!body.to_string().contains(secret), "{secret}");
    }
}

/// What every error response is held to: text written for the reader passes
/// as it is, and nothing else -- a library's error, axum's rejection wording
/// -- reaches the body.
#[tokio::test]
async fn only_written_text_reaches_an_error_response() {
    let read = |r: Response| async move {
        let r = plain_errors(r).await;
        let body = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    };
    let raw = || std::io::Error::other("/opt/monitor/data/themes/.staging-1: incomplete deflate stream");

    let internal = fail(raw());
    assert_eq!(internal.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(read(internal).await, INTERNAL);

    // The shown message, wherever it sits in the chain, and never its cause.
    let wrapped = anyhow::Error::from(raw()).context(crate::Shown("主题包损坏".into())).context("installing");
    let refused = fail(wrapped);
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(read(refused).await, "主题包损坏");

    let rejection = (StatusCode::UNPROCESSABLE_ENTITY, "Failed to deserialize the JSON body").into_response();
    assert_eq!(read(rejection).await, "请求格式不对");
    assert_eq!(read(StatusCode::UNAUTHORIZED.into_response()).await, "登录已失效，请重新登录");
    assert_eq!(read(bad("请填写节点名称")).await, "请填写节点名称");
}
