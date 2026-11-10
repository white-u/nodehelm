//! monitor-hub: collects from monitor agents and serves the panel.
//!
//! No configuration is required to start. Everything beyond the listen address
//! and the database path is configured in the panel and stored in SQLite,
//! leaving no config file to track and no secrets in plaintext TOML.

/// Error text written for whoever reads the reply: the only error wording a
/// response may carry. `api::fail` answers an error with the outermost one in
/// its chain, and an error without one with a fixed message, logging the chain
/// in full; a file path, a SQL error or a library's wording stays in the log.
#[derive(Debug)]
pub struct Shown(pub String);

impl std::fmt::Display for Shown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `anyhow::bail!` with a [`Shown`] message.
macro_rules! refuse {
    ($($arg:tt)*) => {
        return Err(anyhow::Error::msg($crate::Shown(format!($($arg)*))))
    };
}

mod agent_ws;
mod api;
mod auth;
mod db;
mod frontend;
mod latency;
mod management;
mod notify;
mod traffic_period;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use axum::extract::Path;
use axum::http::{header, Extensions, HeaderMap, StatusCode, Version};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::Router;
use chrono::{DateTime, Local, Months, NaiveDate, TimeZone, Timelike, Utc};
use tokio::signal::unix::{signal, SignalKind};
use tower_http::compression::Predicate;
use tracing::{info, warn};

use agent_ws::Agent;
use db::Db;

pub type Shared = Arc<App>;

pub struct App {
    pub db: Db,
    /// Every connected agent: its outbound channel, the session that opened it,
    /// and its latest report. A single map, since connectivity and current
    /// figures are one fact about a node rather than two. See `agent_ws`.
    pub agents: RwLock<HashMap<i64, Agent>>,
    /// Each node's newest traffic reading, booked about once a minute rather
    /// than with every report. Per node rather than per connection; see
    /// `agent_ws::file`.
    pub readings: Mutex<HashMap<i64, agent_ws::Reading>>,
    /// Last rendered node list per audience, `[public, admin]`, with the
    /// millisecond it was built. Shared by every browser stream so viewers do
    /// not multiply the query load. See `api::current`.
    pub snapshot: Mutex<[api::Frame; 2]>,
    pub throttle: auth::Throttle,
    /// Failed agent registrations, counted separately from failed sign-ins: the
    /// two have different threat models, and a batch install run with a stale
    /// key must not lock the operator out of the panel.
    pub registrations: auth::Throttle,
    pub http: reqwest::Client,
    /// Public base URL when `--site` was given, empty otherwise. In the default
    /// case the hub is reached at whatever ip:port the browser used and the
    /// panel falls back to its own origin. Behind a reverse proxy it must be
    /// set, or a loopback listener would place 127.0.0.1 in the install commands
    /// the panel builds.
    pub site: String,
    /// Parent directory containing one folder per installed public theme.
    pub themes: PathBuf,
    /// Alerts on their way out; see `notify::send`.
    pub notes: tokio::sync::mpsc::Sender<notify::Note>,
    /// The latest published tags, as last read from GitHub. Filled when the panel
    /// asks rather than on a timer, so a hub nobody opens makes no outbound
    /// request; see `api::versions`.
    pub releases: Mutex<Releases>,
}

#[derive(Default, Clone)]
pub struct Releases {
    /// The second these were read, 0 before the first read.
    pub read_at: i64,
    /// The tag without its leading `v`, empty where the lookup failed.
    pub hub: String,
}

impl App {
    fn new(db: Db, site: String, themes: PathBuf, notes: tokio::sync::mpsc::Sender<notify::Note>) -> Self {
        Self {
            db,
            agents: RwLock::default(),
            readings: Mutex::default(),
            snapshot: Mutex::default(),
            throttle: auth::Throttle::default(),
            registrations: auth::Throttle::default(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("http client"),
            site,
            themes,
            notes,
            releases: Mutex::default(),
        }
    }

    #[cfg(test)]
    pub fn for_test(db: Db) -> Self {
        // Nothing delivers in tests; `notify::send` drops into the closed channel.
        Self::new(db, String::new(), PathBuf::from("themes"), tokio::sync::mpsc::channel(1).0)
    }

    pub fn public_page(&self) -> bool {
        self.db.get("public_page").as_deref() != Some("off")
    }

    /// Whether a session cookie may be marked Secure. With `--site` this follows
    /// its scheme; without one the hub does not know the address it was reached
    /// on and must rely on the request, where three headers can say https: the
    /// browser's `Origin`, a TLS-terminating proxy's `X-Forwarded-Proto`, and
    /// Cloudflare's `CF-Visitor`. A hub answering plain HTTP directly sees none.
    /// Marking the cookie Secure over plain HTTP would cause the browser to
    /// discard the session.
    ///
    /// Any one of them suffices. A false https costs only the sender's own
    /// session, while a missed one leaves the cookie in the clear -- and each
    /// header alone misses a common shape: a proxy behind a TLS edge rewrites
    /// `X-Forwarded-Proto` to its own `http` hop, `CF-Visitor` exists only behind
    /// Cloudflare, and `Origin` is absent from the navigation that ends GitHub
    /// sign-in. Only that last case still depends on the proxies; `--site
    /// https://...` sets the flag regardless.
    pub fn secure_cookies(&self, headers: &HeaderMap) -> bool {
        if !self.site.is_empty() {
            return !self.site.starts_with("http://");
        }
        let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
        header("origin").is_some_and(|o| o.starts_with("https://"))
            || forwarded_proto(headers) == Some("https")
            || header("cf-visitor")
                .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
                .is_some_and(|v| v["scheme"] == "https")
    }
}

/// The scheme the browser used, as reported by a reverse proxy. Chained proxies
/// append to the header, so the browser's own hop is the first value.
fn forwarded_proto(headers: &HeaderMap) -> Option<&str> {
    let chain = headers.get("x-forwarded-proto")?.to_str().ok()?;
    Some(chain.split(',').next()?.trim())
}

/// Where this hub is published. Not a setting: redirecting it implies a fork,
/// which rebuilds this line anyway.
pub const HUB_REPO: &str = "white-u/nodehelm";

/// The one-line installer pasted onto a new VPS.
async fn install_script() -> Response {
    ([(header::CONTENT_TYPE, "text/x-shellscript")], include_str!("../install.sh")).into_response()
}

/// Places the panel's GitHub proxy in front of a github.com URL when one is
/// set. Shared by the agent relay and the theme updater: a hub that cannot reach
/// github.com for one cannot reach it for the other.
pub fn proxied(app: &App, url: String) -> String {
    match app.db.get("github_proxy").filter(|v| !v.trim().is_empty()) {
        Some(proxy) => format!("{}/{url}", proxy.trim().trim_end_matches('/')),
        None => url,
    }
}

/// The agents this hub was released with, built from `agent/` by the same
/// release job; empty in a development build. See `build.rs`.
mod agents {
    include!(concat!(env!("OUT_DIR"), "/agents.rs"));
}

/// The agent version this hub serves, or "" when it carries none.
pub const AGENT_VERSION: &str = agents::VERSION;

/// Serves the agent binary from the hub itself, so a node that can reach the hub
/// can install without reaching GitHub: IPv6-only machines cannot resolve
/// github.com, and neither can blocked networks.
///
/// `install.sh` executes these bytes as root on every node. They are embedded at
/// build time, so they are exactly as trustworthy as the hub release itself and
/// need no signature of their own; and a hub serves the agent it was tested with.
async fn agent_binary(Path(arch): Path<String>) -> Response {
    let binary = match arch.as_str() {
        "x86_64" => agents::X86_64,
        "aarch64" => agents::AARCH64,
        _ => return api::answer(StatusCode::NOT_FOUND, "unknown architecture"),
    };
    if binary.is_empty() {
        // English, as `install.sh` prints it after its own English line.
        return api::answer(
            StatusCode::NOT_FOUND,
            "this hub build carries no agent; install a released hub to serve one",
        );
    }
    ([(header::CONTENT_TYPE, "application/octet-stream")], binary).into_response()
}

// ---- startup ----

struct Args {
    listen: SocketAddr,
    /// True when `--listen` was omitted, the only case where a refused v6
    /// wildcard may fall back to v4 silently: an explicitly named address is
    /// taken literally.
    listen_defaulted: bool,
    database: String,
    site: String,
    themes: PathBuf,
    reset_password: bool,
}

/// The default listen address. A v6 wildcard also accepts IPv4 through
/// v4-mapped addresses, so one socket serves both -- but only where the kernel
/// permits it: `bindv6only=1` makes it v6-only and drops every IPv4 node, and a
/// kernel booted with `ipv6.disable=1` has no `/proc/sys/net/ipv6` and cannot
/// bind the address at all.
///
/// The proc read is justified because the failure is silent at both ends: a
/// v6-only node has no route to an IPv4 address, so it simply never connects.
fn default_listen() -> &'static str {
    match std::fs::read_to_string("/proc/sys/net/ipv6/bindv6only") {
        Ok(flag) if flag.trim() == "0" => "[::]:28080",
        _ => "0.0.0.0:28080",
    }
}

fn parse_args() -> Result<Args> {
    let mut listen = None;
    let mut database = "monitor.db".to_owned();
    let mut site = String::new();
    let mut themes = None;
    let mut reset_password = false;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().unwrap_or_default();
        match arg.as_str() {
            "--listen" => listen = Some(value()),
            "--db" => database = value(),
            "--site" => site = value(),
            "--themes" => themes = Some(PathBuf::from(value())),
            "--reset-password" => reset_password = true,
            "--lifecycle-protocol" => {
                println!("node-manager-hub-lifecycle-v1");
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!(
                    "monitor-hub {}\n\n\
                     Usage: monitor-hub [--listen [::]:28080] [--db monitor.db] [--themes themes] [--site https://hub.example.com]\n       \
                     monitor-hub --db monitor.db --reset-password\n\n\
                     --listen defaults to [::]:28080, one socket serving IPv6 and IPv4\n\
                     both; where the kernel has no dual-stack sockets it is 0.0.0.0:28080.\n\
                     --themes defaults to a themes/ directory beside the database.\n\
                     --site is the https:// domain agents should use. Left out, the hub\n\
                     answers on whatever ip:port it is asked, and the panel builds install\n\
                     commands from the address in the browser's bar, which behind a TLS\n\
                     reverse proxy is already right. It is needed where agents reach the\n\
                     hub by another name than the panel's; where the panel is opened\n\
                     through an SSH tunnel, whose loopback address no node can reach and\n\
                     which adds no node without it; and where GitHub sign-in sits behind\n\
                     proxies that do not report https -- no X-Forwarded-Proto, or a CDN\n\
                     other than Cloudflare ahead of an nginx that rewrites it to http --\n\
                     since it then sets the session cookie's Secure flag. A value that is\n\
                     not an https:// domain disables adding nodes.\n\
                     --reset-password replaces the emergency password, signs every session\n\
                     out, prints the new password and exits. The database must exist.",
                    env!("CARGO_PKG_VERSION")
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    let listen_defaulted = listen.is_none();
    let listen: SocketAddr = listen.unwrap_or_else(|| default_listen().to_owned()).parse()?;
    let themes = themes.unwrap_or_else(|| {
        std::path::Path::new(&database).parent().unwrap_or_else(|| std::path::Path::new(".")).join("themes")
    });
    Ok(Args {
        listen,
        listen_defaulted,
        database,
        site: site.trim_end_matches('/').to_owned(),
        themes,
        reset_password,
    })
}

/// The soft open-file limit the hub asks for. Every agent holds one descriptor,
/// every open page another, and every request in flight a third. 65536 leaves
/// about 65 for each of the 1000 nodes a hub is sized for.
const NOFILE_TARGET: u64 = 65_536;

/// Raises the soft open-file limit toward [`NOFILE_TARGET`], capped by the hard
/// one. systemd and Docker start a process at a soft limit of 1024, which Rust
/// does not raise as Go does: with about a thousand connections `accept` fails
/// with EMFILE, which the server retries silently, and the hub stops answering
/// without logging a line (measured: 846 of 1000 agents connected, `/api/me`
/// unanswered).
fn raise_open_files() {
    use rustix::process::{getrlimit, setrlimit, Resource};
    let mut limit = getrlimit(Resource::Nofile);
    // Unlimited needs nothing.
    let Some(soft) = limit.current else { return };
    let wanted = limit.maximum.map_or(NOFILE_TARGET, |hard| hard.min(NOFILE_TARGET));
    let mut now = soft;
    if soft < wanted {
        limit.current = Some(wanted);
        match setrlimit(Resource::Nofile, limit) {
            Ok(()) => now = wanted,
            Err(e) => warn!("open-file limit could not be raised from {soft}: {e}"),
        }
    }
    if now < NOFILE_TARGET {
        warn!(
            "open-file limit is {now}, and the hub stops accepting connections near it. Raise \
             LimitNOFILE for the service, or --ulimit nofile for the container"
        );
    } else if now > soft {
        info!("open-file limit raised from {soft} to {now}");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MONITOR_LOG")
                .unwrap_or_else(|_| "monitor_hub=info,tower_http=warn".into()),
        )
        .init();

    let args = parse_args()?;
    // Delivered on the terminal rather than through the service log, which some
    // hosts do not keep. A running hub reads the hash and its sessions from the
    // database on every request, so the change applies without a restart.
    if args.reset_password {
        // A mistyped path would otherwise create an empty database and print a
        // password no running hub reads.
        anyhow::ensure!(std::path::Path::new(&args.database).is_file(), "no database at {}", args.database);
        // install-hub.sh extracts the password by this exact line prefix.
        println!("Emergency password: {}", new_password(&Db::open(&args.database)?)?);
        return Ok(());
    }
    // After --reset-password, which prints and exits, so its output stays its own.
    raise_open_files();
    std::fs::create_dir_all(&args.themes)?;
    if let Err(e) = db::temp_files_beside(&args.database) {
        warn!("SQLite keeps its temporary files in its default directory: {e:#}");
    }
    let (notes, inbox) = tokio::sync::mpsc::channel(notify::QUEUE);
    let app = Arc::new(App::new(Db::open(&args.database)?, args.site.clone(), args.themes, notes));
    let url = advertised_url(&args.site, args.listen);
    first_run(&app, &url)?;
    let port = args.listen.port();
    // On a container's bridge network the host side of `-p` decides who can
    // connect, which the hub cannot see, and a loopback listener would leave the
    // published port with nothing behind it. Under host networking the listener
    // is the host's own, as on a bare host. The marker files do not tell the two
    // apart, so the advice names both.
    let close = if in_container() {
        format!(
            "publish it on the host's loopback only (-p 127.0.0.1:HOST_PORT:{port}) or not at all when \
             the proxy shares the container's network; under host networking, --listen \
             127.0.0.1:{port} instead"
        )
    } else {
        format!("--listen 127.0.0.1:{port}")
    };
    if exposed_over_plain_http(&url) {
        warn!(
            "this hub answers plain HTTP at {url}; sessions and agent tokens travel in the clear. \
             Put it behind a TLS reverse proxy -- the panel builds install commands from the \
             browser's own address, so nothing here has to change -- and close this port so the \
             proxy is the only way in: {close}"
        );
    }
    // The warning above derives from --site, the address the operator
    // advertises. This one derives from the socket actually open, and the two
    // diverge in the deployment that needs it most: `--site https://...` with
    // --listen left at its wildcard default prints nothing while the port answers
    // plain HTTP to anyone who finds it.
    else if !args.listen.ip().is_loopback() {
        warn!(
            "listening on {} in the clear. If a TLS proxy fronts this hub, callers can still reach \
             this port directly, without TLS. Close it so the proxy is the only way in: {close}",
            args.listen
        );
    }
    // Checked once here, because the answer is static: `provisioning_allowed`
    // measures every request against --site, so a value that is not an https
    // domain permanently refuses adding and installing nodes however the panel is
    // reached. The panel names --site in that refusal, and this warning reaches
    // an operator who never opens the panel. A warning rather than a fatal
    // error: the hub still serves everything else, and an operator upgrading
    // into this check should not lose a running hub. `install-hub.sh` refuses
    // the same values where they are entered.
    if !args.site.is_empty() && api::https_domain(&args.site).is_none() {
        warn!(
            "--site {} is not an https domain entry, so adding and installing nodes will be refused \
             however the panel is reached: it has to be https://, a domain rather than an address, \
             and nothing after the host",
            args.site
        );
    }

    let held = app.clone();
    tokio::spawn(housekeeping(app.clone()));
    tokio::spawn(notify::deliver(app.clone(), inbox));
    tokio::spawn(notify::watch(app.clone()));

    let router = Router::new()
        // Read paths; the public page reaches these unauthenticated.
        .route("/api/me", get(api::me))
        .route("/api/nodes", get(api::nodes))
        .route("/api/nodes/{id}/metrics", get(api::metrics))
        .route("/api/nodes/{id}/traffic/periods", get(api::traffic_periods))
        .route("/api/ws", get(api::live_ws))
        .route("/api/themes/{short}/config", get(api::theme_config))
        // Sign-in.
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/github", get(auth::github_start))
        .route("/api/auth/github/callback", get(auth::github_callback))
        // Panel.
        .route("/api/nodes", post(api::create_node))
        .route("/api/register-window", post(api::open_register).delete(api::close_register))
        .route("/api/nodes/order", put(api::reorder_nodes))
        .route("/api/nodes/batch", put(api::update_nodes))
        .route("/api/nodes/{id}", put(api::update_node).delete(api::delete_node))
        .route("/api/nodes/{id}/token", get(api::node_token).post(api::reset_token))
        .route("/api/nodes/{id}/traffic", put(api::patch_traffic))
        .route("/api/nodes/{id}/ping-tasks", put(api::set_node_ping_tasks))
        .route("/api/ping-tasks", get(api::ping_tasks).post(api::save_ping_task))
        .route("/api/ping-tasks/order", put(api::reorder_ping_tasks))
        .route("/api/ping-tasks/{id}", delete(api::delete_ping_task))
        .route("/api/sessions", get(api::sessions))
        .route("/api/sessions/{id}", delete(api::delete_session))
        .route("/api/settings", get(api::settings).put(api::save_settings))
        .route("/api/version", get(api::versions))
        .route("/api/notify/test", post(notify::test))
        .route("/api/themes", get(api::themes))
        .route("/api/themes/{short}", delete(api::delete_theme))
        .route("/api/themes/{short}/preview", get(api::theme_preview))
        .route("/api/themes/{short}/update", post(api::update_theme))
        // Not under /api/themes/: a fixed segment there would shadow the theme
        // of that name for the routes keyed by `{short}`.
        .route("/api/theme-install", post(api::install_theme))
        .route("/api/themes/{short}/config", put(api::save_theme_config))
        .route("/api/db", get(api::db_stats))
        .route("/api/db/backup", get(api::db_backup))
        .route("/api/db/vacuum", post(api::db_vacuum))
        .merge(management::admin_routes())
        .fallback(frontend::serve)
        // Every body above is a JSON form of a few KiB at most. The ceiling also
        // bounds a theme's saved settings, which anonymous callers read back.
        .layer(tower_http::limit::RequestBodyLimitLayer::new(64 * 1024))
        // The two chunked uploads, merged after that layer rather than beneath
        // it. They raise the ceiling on a single request to `api::MAX_CHUNK`,
        // not on the file behind it: a 1 GiB backup arrives as 256 of the
        // panel's 4 MiB pieces, so no reverse proxy needs to know the database
        // size. The whole-file ceilings live on `total` and are checked on the
        // first request.
        .merge(
            Router::new()
                .route("/api/db/restore", post(api::db_restore))
                .route("/api/themes", post(api::upload_theme))
                .layer(tower_http::limit::RequestBodyLimitLayer::new(api::MAX_CHUNK))
                .with_state(app.clone()),
        )
        .layer(axum::middleware::from_fn(api::same_origin_writes))
        .layer(axum::middleware::map_response(api::plain_errors))
        // Agents, merged after those layers: `install.sh` and the agent print these
        // replies beside their own English output, so they are left as written.
        .merge(
            Router::new()
                .merge(management::agent_routes())
                .route("/management-files/{name}", get(management::files))
                .route("/api/agent/ws", get(agent_ws::handler))
                .route("/api/agent/echo", get(agent_ws::echo))
                .route("/api/agent/register", post(api::agent_register))
                .route("/install.sh", get(install_script))
                .route("/agent/{arch}", get(agent_binary))
                .layer(tower_http::limit::RequestBodyLimitLayer::new(64 * 1024))
                .with_state(app.clone()),
        )
        // A poll includes all bounded module snapshots and a receipt. Its maximum
        // legitimate payload exceeds a small form; keep other agent routes small.
        .merge(
            Router::new()
                .route("/api/management-agent/v1/poll", post(management::poll_route))
                .layer(tower_http::limit::RequestBodyLimitLayer::new(512 * 1024))
                .with_state(app.clone()),
        )
        .layer(axum::middleware::from_fn_with_state(app.clone(), management::lifecycle_barrier))
        .layer(tower_http::compression::CompressionLayer::new().compress_when(compressible()))
        .with_state(app);

    let listener = match tokio::net::TcpListener::bind(args.listen).await {
        Ok(listener) => listener,
        // A host that refuses the dual-stack wildcard must still start, and on
        // such a host IPv4 is all there is to serve.
        Err(e) if args.listen_defaulted && args.listen.is_ipv6() => {
            let v4 = SocketAddr::from(([0, 0, 0, 0], args.listen.port()));
            warn!("could not bind {} ({e}); falling back to {v4}", args.listen);
            tokio::net::TcpListener::bind(v4).await?
        }
        Err(e) => return Err(e.into()),
    };
    info!("listening on {} ({url})", listener.local_addr()?);
    serve(listener, router).await;
    // The readings held back from the traffic rows. Unbooked, they cost nothing
    // to a node that reports again under the same boot, but a node rebooting
    // while the hub is down would take them with it.
    agent_ws::book_held(&held, None);
    Ok(())
}

/// Serves HTTP/1 until a stop signal, then lets in-flight requests finish.
///
/// `axum::serve` gives hyper no timer, and without one hyper enforces no
/// header read timeout: a connection that sends nothing, half a request head,
/// or idles between keep-alive requests would hold its descriptor indefinitely,
/// and 1100 such connections would stop a hub limited to 1024 descriptors. With
/// the timer every wait for a request head ends after hyper's default 30 s. A
/// WebSocket has left hyper once upgraded and is bounded by its own heartbeat.
async fn serve(listener: tokio::net::TcpListener, router: Router) {
    use axum::extract::ConnectInfo;
    use hyper::body::Incoming;
    use hyper_util::rt::{TokioIo, TokioTimer};
    use hyper_util::service::TowerToHyperService;
    use tower::ServiceExt;

    let mut http1 = hyper::server::conn::http1::Builder::new();
    http1.timer(TokioTimer::new());
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(());
    let mut stop = std::pin::pin!(shutdown());
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(e) => {
                    accept_failed(e).await;
                    continue;
                }
            },
            () = &mut stop => break,
        };
        // hyper has no write timeout, so a client that stops reading would park
        // its response on a closed window and hold the descriptor indefinitely.
        // The kernel aborts a window that stays closed, or data that nobody
        // acknowledges, after this long.
        #[cfg(target_os = "linux")]
        let _ = rustix::net::sockopt::set_tcp_user_timeout(&stream, STALL.as_millis() as u32);
        let service = router.clone().map_request(move |mut req: axum::http::Request<Incoming>| {
            req.extensions_mut().insert(ConnectInfo(peer));
            req
        });
        let conn = http1.serve_connection(TokioIo::new(stream), TowerToHyperService::new(service));
        let mut stop_rx = stop_rx.clone();
        tokio::spawn(async move {
            let mut conn = std::pin::pin!(conn.with_upgrades());
            tokio::select! {
                _ = conn.as_mut() => return,
                _ = stop_rx.changed() => conn.as_mut().graceful_shutdown(),
            }
            let _ = conn.await;
        });
    }
    drop((listener, stop_rx));
    let _ = stop_tx.send(());
    // Every connection task holds a receiver until its connection ends. A
    // client that stops reading, or sits on half a request head, would hold the
    // drain open past `docker stop`'s 10 s, and the readings booked after it
    // would go with the killed process.
    let _ = tokio::time::timeout(DRAIN, stop_tx.closed()).await;
}

/// How long the peer's receive window may stay shut, or data go unacknowledged,
/// before the kernel drops the connection. The push stream gives up on a send
/// after 10 s of its own; this bounds the rest.
#[cfg(target_os = "linux")]
const STALL: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a stop waits for in-flight requests.
const DRAIN: std::time::Duration = std::time::Duration::from_secs(5);

/// An accept that failed for want of a descriptor would fail again at once,
/// so the loop pauses rather than spin while connections close.
async fn accept_failed(e: std::io::Error) {
    use std::io::ErrorKind::{ConnectionAborted, ConnectionRefused, ConnectionReset};
    if !matches!(e.kind(), ConnectionAborted | ConnectionRefused | ConnectionReset) {
        warn!("accept failed: {e}");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// Waits for whichever stop signal arrives first. SIGTERM is the significant
/// one: it is how systemd stops a service, and without handling it a deploy
/// terminates the hub outright rather than letting it finish in-flight
/// requests.
async fn shutdown() {
    // SIGTERM can always be registered; a failure here indicates a broken
    // runtime, and falling back to Ctrl-C alone would reinstate the problem
    // described above.
    let mut term = signal(SignalKind::terminate()).expect("listen for SIGTERM");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    info!("shutting down");
    db::halt();
}

/// The address printed at startup: `--site` when given, otherwise the listen
/// address with any wildcard resolved to a concrete one, since
/// `http://0.0.0.0:28080` cannot be opened in a browser.
fn advertised_url(site: &str, listen: SocketAddr) -> String {
    if !site.is_empty() {
        return site.to_owned();
    }
    let ip =
        if listen.ip().is_unspecified() { outbound_ip().unwrap_or_else(|| listen.ip()) } else { listen.ip() };
    format!("http://{}", SocketAddr::new(ip, listen.port()))
}

/// This host's own address on its outbound route. Asking the kernel to route a
/// datagram it never sends is the cheapest way to select one interface among
/// several, and it answers without any network traffic. Behind NAT it yields the
/// private address, since the hub cannot know its public one; `--site` states it.
fn outbound_ip() -> Option<IpAddr> {
    [("0.0.0.0:0", "1.1.1.1:80"), ("[::]:0", "[2606:4700:4700::1111]:80")].into_iter().find_map(
        |(bind, route_to)| {
            let socket = std::net::UdpSocket::bind(bind).ok()?;
            socket.connect(route_to).ok()?;
            socket.local_addr().ok().map(|addr| addr.ip())
        },
    )
}

/// True when the hub's own address transmits cookies and tokens in the clear.
/// Plain HTTP to loopback is local development; to anything else it means the
/// session cookie is readable by every intermediate hop.
///
/// A hub behind a TLS-terminating proxy or tunnel is excluded by either route:
/// `--site` is then the https:// address even though the listener speaks plain
/// HTTP, and without one the listener is on loopback, unreachable by others.
fn exposed_over_plain_http(site: &str) -> bool {
    let Some(rest) = site.strip_prefix("http://") else {
        return false;
    };
    !host_is_loopback(rest)
}

/// Loopback test over an `authority` such as `example.com:8080` or `[::1]:8080`.
/// IPv6 literals are bracketed, so the port is not split off at the first
/// colon.
fn host_is_loopback(authority: &str) -> bool {
    let authority = authority.split('/').next().unwrap_or("");
    // RFC 3986 places userinfo before the host, so `127.0.0.1:28080@example.com`
    // reads as loopback to any check splitting at the first colon while the
    // browser resolves the name that follows -- and this decides whether the
    // plaintext warning is printed at all. `provisioning_allowed` parses --site
    // with reqwest::Url and strips it there; both must agree.
    let authority = authority.rsplit('@').next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    // Parsed rather than prefix-matched: `127.example.com` is a registered name
    // resolving wherever its owner points it, and reading it as loopback would
    // suppress the only warning that the cookie travels in the clear.
    host.is_empty() || host == "localhost" || host.parse::<IpAddr>().is_ok_and(|a| a.is_loopback())
}

/// Whether the hub runs in a Docker or Podman container, which mark each one
/// with /.dockerenv and /run/.containerenv. The wider signals
/// (/run/systemd/container, cgroup paths) are not consulted: they also mark
/// LXC, and a VPS that is itself an LXC container is a bare host for this
/// purpose.
///
/// ponytail: Kubernetes pods carry neither marker and receive the bare-host
/// advice; checking $KUBERNETES_SERVICE_HOST would cover them.
fn in_container() -> bool {
    ["/.dockerenv", "/run/.containerenv"].into_iter().any(|p| std::path::Path::new(p).exists())
}

/// Prints a one-time admin password when the database is first created, since a
/// fresh hub is otherwise inaccessible until GitHub is configured.
fn first_run(app: &App, url: &str) -> Result<()> {
    if app.db.get("admin_password_hash").is_some() {
        return Ok(());
    }
    let password = new_password(&app.db)?;
    println!(
        "\n  Monitor hub is ready.\n\n  \
         Sign in at {url}/admin\n  \
         Emergency password: {password}\n\n  \
         This is shown once. Change it, and set up GitHub sign-in, under Security.\n"
    );
    Ok(())
}

/// Sets a random 24-character admin password and signs every session out.
fn new_password(db: &Db) -> Result<String> {
    let password = auth::random_token()[..24].to_owned();
    db.replace_password(&auth::hash_password(&password)?)?;
    Ok(password)
}

/// Cycles stored under a name. Any other length is stored as `<n>m`; the names
/// remain because themes built for hub 1.3.0 and earlier recognize only these.
const NAMED_CYCLES: [(&str, u32); 6] = [
    ("monthly", 1),
    ("quarterly", 3),
    ("semiannual", 6),
    ("yearly", 12),
    ("biennial", 24),
    ("triennial", 36),
];

/// The longest cycle accepted, in months: 100 years.
const MAX_CYCLE: u32 = 1_200;

/// Billing cycles as whole months. `once` has none, so it never rolls over.
fn cycle_months(cycle: &str) -> Option<u32> {
    match NAMED_CYCLES.iter().find(|(name, _)| *name == cycle) {
        Some(&(_, months)) => Some(months),
        None => cycle.strip_suffix('m')?.parse().ok().filter(|m| (1..=MAX_CYCLE).contains(m)),
    }
}

/// The stored spelling of a cycle of `months`: its name where it has one.
fn cycle_name(months: u32) -> String {
    NAMED_CYCLES
        .iter()
        .find(|(_, m)| *m == months)
        .map_or_else(|| format!("{months}m"), |(name, _)| (*name).into())
}

/// A node still reporting past its expiry date has been renewed, so the date is
/// rolled forward by whole cycles until it lies in the future.
fn renewed(expires: NaiveDate, cycle: &str, today: NaiveDate) -> Option<NaiveDate> {
    let months = Months::new(cycle_months(cycle)?);
    let mut next = expires;
    while next < today {
        next = next.checked_add_months(months)?;
    }
    (next != expires).then_some(next)
}

fn renew_online_nodes(app: &App) -> Result<()> {
    // The hub's local timezone, as with the traffic boundaries: an expiry date
    // is one a person entered, and on a UTC+8 hub `Utc` reports the previous day
    // until 08:00 while the panel already shows it expired.
    let today = Local::now().date_naive();
    let online: Vec<i64> = app.agents.read().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
    let nodes = app.db.nodes()?;
    let mut rolled = Vec::new();
    for node in &nodes {
        if !online.contains(&node.id) {
            continue;
        }
        let Some(expires) = node.expires_at.as_deref().and_then(|d| d.parse::<NaiveDate>().ok()) else {
            continue;
        };
        let Some(next) = renewed(expires, &node.billing_cycle, today) else { continue };
        app.db.set_expiry(node.id, &next.to_string())?;
        info!("node {} is still up past {expires}, expiry rolled to {next}", node.name);
        rolled.push((node.name.as_str(), format!("{expires} → {next}")));
    }
    notify::renewed(app, rolled);
    Ok(())
}

/// Rolls over expiry dates, expires sessions, sends the daily expiry digest and
/// folds and trims history: once at startup, then on the hour of the hub's clock.
///
/// On the hour because renewal falls due when the hub's date changes. Passes
/// counted from startup would leave an online node shown expired for up to an
/// hour after midnight; aligned, the midnight pass rolls it forward within
/// seconds. A node that comes back online past its expiry date waits for the
/// next hour.
async fn housekeeping(app: Shared) {
    loop {
        // First, so the midnight pass does not wait on anything below.
        if let Err(e) = renew_online_nodes(&app) {
            warn!("rolling expiry dates failed: {e:#}");
        }
        if let Err(e) = app.db.expire_sessions() {
            warn!("expiring sessions failed: {e:#}");
        }
        if let Err(e) = management::prune_operations(&app.db, Utc::now().timestamp()) {
            warn!("pruning management operations failed: {e:#}");
        }
        // After the roll-over, so the digest lists dates as they now stand.
        match notify::expiry_digest(&app, Local::now()) {
            Ok(Some(note)) => notify::send(&app, note),
            Ok(None) => {}
            Err(e) => warn!("expiry digest failed: {e:#}"),
        }
        // Last, and off the runtime: the first pass after an upgrade folds every
        // hour still held in minute rows and prunes the week's excess, which
        // takes seconds to minutes. Folding precedes pruning, and pruning runs
        // even when folding fails: it keeps minute rows until their hour is
        // folded, and skipping it would leave the database growing.
        let history = app.clone();
        let done = tokio::task::spawn_blocking(move || {
            let keep = history.db.retention_days();
            match history.db.roll_up(Utc::now().timestamp(), keep) {
                // More than the hour a pass normally folds is a catch-up, logged
                // for the disk activity it causes.
                Ok(folded) if folded > 1 => info!("folded {folded} hours of history into the hourly tier"),
                Ok(_) => {}
                Err(e) => warn!("folding history failed: {e:#}"),
            }
            history.db.prune(keep)
        })
        .await;
        if let Err(e) = done.map_err(anyhow::Error::from).and_then(|r| r) {
            warn!("maintaining history failed: {e:#}");
        }
        tokio::time::sleep(until_next_hour(Local::now())).await;
    }
}

/// Time from `now` to the start of the next hour on its clock. Read afresh on
/// every pass, so a clock step or a daylight-saving change shifts no later one.
fn until_next_hour<Tz: TimeZone>(now: DateTime<Tz>) -> std::time::Duration {
    std::time::Duration::from_secs(u64::from(3_600 - now.minute() * 60 - now.second()))
}

/// Which answers the hub compresses. Not the agent binary, already compressed,
/// nor database backups, hundreds of megabytes on a large fleet: deflating either
/// would occupy the cores argon2 and the SQLite writer share for the whole
/// transfer. Nor an answer marked `no-transform`, which Cloudflare leaves
/// uncompressed as well: the admin node list, whose secrets sit beside strings an
/// agent reports, so that the length of a compressed copy would let one rogue
/// agent guess them a character at a time. nginx's gzip ignores the directive,
/// so a proxy configured to compress JSON compresses this answer regardless.
fn compressible() -> impl Predicate {
    tower_http::compression::predicate::DefaultPredicate::new()
        .and(tower_http::compression::predicate::NotForContentType::const_new("application/octet-stream"))
        .and(|status: StatusCode, _: Version, headers: &HeaderMap, _: &Extensions| {
            // Directives are case-insensitive and comma-separated.
            let no_transform = headers
                .get_all(header::CACHE_CONTROL)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(','))
                .any(|d| d.trim().eq_ignore_ascii_case("no-transform"));
            status != StatusCode::SWITCHING_PROTOCOLS && !no_transform
        })
}

/// A fresh directory under the temp directory, removed with everything in it
/// when dropped, so a failing test leaves nothing behind either. A mutation run
/// fails thousands of them.
#[cfg(test)]
pub(crate) struct TempDir(pub PathBuf);

#[cfg(test)]
impl TempDir {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("monitor-{name}-{:016x}", rand::random::<u64>()));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::extract::State;
    use axum::http::{StatusCode, Uri};

    /// A development build carries no agent and says so, rather than serving an
    /// empty file that `install.sh` would install as the agent.
    #[tokio::test]
    async fn the_agent_route_serves_the_embedded_binary_or_explains_its_absence() {
        let answer = |arch: &str| agent_binary(Path(arch.to_owned()));
        assert_eq!(answer("riscv64").await.status(), StatusCode::NOT_FOUND);
        let served = answer("x86_64").await;
        if agents::X86_64.is_empty() {
            assert_eq!(served.status(), StatusCode::NOT_FOUND);
            assert_eq!(AGENT_VERSION, "", "no version is announced for agents the hub cannot serve");
        } else {
            assert_eq!(served.status(), StatusCode::OK);
            let body = axum::body::to_bytes(served.into_body(), usize::MAX).await.unwrap();
            assert_eq!(&body[..], agents::X86_64);
            assert!(!AGENT_VERSION.is_empty());
        }
    }

    /// The limit only moves up, so raising it here cannot disturb another test.
    #[test]
    fn the_soft_open_file_limit_reaches_the_target_or_the_hard_one() {
        use rustix::process::{getrlimit, Resource};
        raise_open_files();
        let limit = getrlimit(Resource::Nofile);
        let wanted = limit.maximum.map_or(NOFILE_TARGET, |hard| hard.min(NOFILE_TARGET));
        assert!(
            limit.current.is_none_or(|soft| soft >= wanted),
            "soft limit {:?} below {wanted}",
            limit.current
        );
    }

    fn app(site: &str) -> App {
        App::new(
            Db::open(":memory:").unwrap(),
            site.into(),
            PathBuf::from("themes"),
            tokio::sync::mpsc::channel(1).0,
        )
    }

    /// A request as a reverse proxy would forward it, or as it arrives with none
    /// in front.
    fn proto(forwarded: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(scheme) = forwarded {
            headers.insert("x-forwarded-proto", scheme.parse().unwrap());
        }
        headers
    }

    #[test]
    fn an_answer_marked_no_transform_is_left_uncompressed() {
        let json = |cache: Option<&'static str>| {
            let mut res = Response::new(axum::body::Body::from("x".repeat(1024)));
            res.headers_mut().insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            if let Some(cache) = cache {
                res.headers_mut().insert(header::CACHE_CONTROL, cache.parse().unwrap());
            }
            res
        };
        assert!(compressible().should_compress(&json(None)));
        assert!(!compressible().should_compress(&json(Some("no-store, no-transform"))));
        assert!(!compressible().should_compress(&json(Some("No-Transform"))), "directives ignore case");
        assert!(
            compressible().should_compress(&json(Some("x-no-transform-ext"))),
            "only the directive itself"
        );
    }

    /// Whichever wildcard this kernel supports must parse and carry the default
    /// port; a typo here would surface only as a refused bind at startup.
    #[test]
    fn the_default_listener_is_a_wildcard_on_the_default_port() {
        let addr: SocketAddr = default_listen().parse().expect("the default must parse");
        assert!(addr.ip().is_unspecified(), "{addr}");
        assert_eq!(addr.port(), 28_080);
    }

    #[test]
    fn an_expired_node_that_is_still_up_rolls_forward_whole_cycles() {
        let d = |s: &str| s.parse::<NaiveDate>().unwrap();
        // One day past a monthly expiry: the next month, clamped to its end.
        assert_eq!(renewed(d("2026-01-31"), "monthly", d("2026-02-01")), Some(d("2026-02-28")));
        // Years overdue: cycles are added until the date is in the future.
        assert_eq!(renewed(d("2024-03-10"), "yearly", d("2026-08-28")), Some(d("2027-03-10")));
        // A length without a name rolls the same way.
        assert_eq!(renewed(d("2026-03-10"), "60m", d("2026-08-28")), Some(d("2031-03-10")));
        // Not yet due, and one-off billing: both left unchanged.
        assert_eq!(renewed(d("2026-09-01"), "monthly", d("2026-08-28")), None);
        assert_eq!(renewed(d("2020-01-01"), "once", d("2026-08-28")), None);
    }

    /// Only a node still reporting has evidently been paid for; one that is down
    /// may have been let go, and rolling it would hide that.
    #[test]
    fn only_an_online_node_is_renewed() {
        let app = app("");
        let yesterday = (Local::now().date_naive() - chrono::Days::new(1)).to_string();
        let mut ids = Vec::new();
        for name in ["up", "down"] {
            let id = app
                .db
                .create_node(
                    &db::Node { name: name.into(), billing_cycle: "monthly".into(), ..Default::default() },
                    name,
                )
                .unwrap();
            app.db.set_expiry(id, &yesterday).unwrap();
            ids.push(id);
        }
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        app.agents.write().unwrap().insert(ids[0], Agent::new(1, tx));
        renew_online_nodes(&app).unwrap();
        let expiry = |id| app.db.node(id).unwrap().unwrap().expires_at.unwrap();
        assert!(expiry(ids[0]) > yesterday, "the online node rolls forward");
        assert_eq!(expiry(ids[1]), yesterday, "the offline one is left to lapse");
    }

    /// The hour is the local one, which in a half-hour zone is not UTC's.
    #[test]
    fn housekeeping_wakes_on_the_local_hour() {
        let india = chrono::FixedOffset::east_opt(5 * 3_600 + 1_800).unwrap();
        let at = |h, m, s| until_next_hour(india.with_ymd_and_hms(2026, 9, 23, h, m, s).unwrap()).as_secs();
        assert_eq!(at(23, 59, 30), 30, "the midnight pass lands as the date changes");
        assert_eq!(at(10, 0, 0), 3_600);
    }

    #[tokio::test]
    async fn an_unknown_api_path_is_a_404_not_the_single_page_app() {
        let app = Arc::new(app("http://localhost:8080"));
        let spa = |p: &str| frontend::serve(State(app.clone()), HeaderMap::new(), p.parse::<Uri>().unwrap());

        // The case that would conceal a misconfigured OAuth callback.
        assert_eq!(spa("/api/oauth_callback?code=x").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(spa("/api/nope").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(spa("/api").await.status(), StatusCode::NOT_FOUND);

        // Client-side routes still fall through to the app.
        assert_eq!(spa("/admin/nodes").await.status(), StatusCode::OK);
        // The panel's entry is a redirect to its first page, keeping the query.
        let entry = spa("/admin?login_error=x").await;
        assert_eq!(entry.headers()[axum::http::header::LOCATION], "/admin/nodes?login_error=x");
        assert_eq!(spa("/").await.status(), StatusCode::OK);
        // A path merely beginning with "api" is not an API path.
        assert_eq!(spa("/apiary").await.status(), StatusCode::OK);
    }

    /// A build writes hashed filenames under `assets/`, so a miss there means a
    /// tab left open across a deploy. Answering with index.html would hand a
    /// script tag HTML, failing on MIME type long after the request that caused
    /// it. Both bundles share the same fallback, so both must refuse.
    #[tokio::test]
    async fn a_missing_hashed_asset_is_a_404_not_the_single_page_app() {
        let app = Arc::new(app("http://localhost:8080"));
        let spa = |p: &str| frontend::serve(State(app.clone()), HeaderMap::new(), p.parse::<Uri>().unwrap());

        assert_eq!(spa("/assets/index-STALE.js").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(spa("/admin/assets/index-STALE.js").await.status(), StatusCode::NOT_FOUND);

        // A route merely beginning with those letters is still a route.
        assert_eq!(spa("/assetsomething").await.status(), StatusCode::OK);
        // A deep client route still reloads into the app.
        assert_eq!(spa("/node/7").await.status(), StatusCode::OK);
    }

    /// What determines the Secure flag: `--site` when set, otherwise the request
    /// -- the default ip:port deployment, where the hub does not know its own
    /// address.
    #[test]
    fn the_cookie_flag_follows_site_when_it_is_set_and_the_request_when_it_is_not() {
        // Local development: no Secure flag, or the browser discards the cookie
        // entirely.
        for local in ["http://127.0.0.1:28080", "http://localhost:28080", "http://[::1]:28080"] {
            assert!(!app(local).secure_cookies(&proto(None)), "{local}");
            assert!(!exposed_over_plain_http(local), "{local} is not exposed");
        }
        // A configured --site takes precedence over the request in both
        // directions: operator configuration outranks a client-settable header.
        assert!(app("https://hub.example.com").secure_cookies(&proto(Some("http"))));
        assert!(!app("http://hub.example.com").secure_cookies(&proto(Some("https"))));
        assert!(!exposed_over_plain_http("https://m.example.com"));
        // A registered name is not an address however it begins: reading one as
        // loopback would suppress the plaintext-cookie warning.
        assert!(exposed_over_plain_http("http://127.example.com"));
        assert!(exposed_over_plain_http("http://127.0.0.1.nip.io"));
        // Nor is userinfo an address: the host follows the '@', and reading the
        // part before it as loopback suppresses the same warning.
        assert!(exposed_over_plain_http("http://127.0.0.1:28080@hub.example.com"));
        assert!(!app("http://127.0.0.1:28080@hub.example.com").secure_cookies(&proto(None)));

        // Without --site the request's headers are the only indication of scheme.
        let bare = app("");
        assert!(!bare.secure_cookies(&proto(None)), "plain HTTP, answered directly");
        assert!(bare.secure_cookies(&proto(Some("https"))));
        // Chained proxies append, so the browser's own hop is the first value.
        assert!(bare.secure_cookies(&proto(Some("https, http"))));
        assert!(!bare.secure_cookies(&proto(Some("http, https"))));

        // A TLS edge ahead of nginx's `X-Forwarded-Proto $scheme`: the header
        // reads http, yet the browser is on https. Its own Origin says so on
        // every write, Cloudflare's CF-Visitor on every request.
        let with = |name: &str, value: &str| {
            let mut headers = proto(Some("http"));
            headers.insert(name.parse::<header::HeaderName>().unwrap(), value.parse().unwrap());
            headers
        };
        assert!(bare.secure_cookies(&with("origin", "https://hub.example.com")));
        assert!(bare.secure_cookies(&with("cf-visitor", r#"{"scheme":"https"}"#)));
        assert!(!bare.secure_cookies(&with("origin", "http://hub.example.com")));
        assert!(!bare.secure_cookies(&with("cf-visitor", r#"{"scheme":"http"}"#)));
        assert!(bare.secure_cookies(&with("cf-visitor", r#"{ "scheme": "https" }"#)), "re-serialized");
        assert!(!bare.secure_cookies(&with("origin", "null")));
    }

    /// A hub serving in the clear must report it, and a wildcard listener is not
    /// an address anyone can open. Both concern the URL the hub advertises, which
    /// is `--site` only when one is set.
    #[test]
    fn the_advertised_url_resolves_a_wildcard_listener_and_defers_to_site() {
        let listen = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert_eq!(
            advertised_url("https://hub.example.com", listen("127.0.0.1:28080")),
            "https://hub.example.com"
        );
        assert_eq!(advertised_url("", listen("127.0.0.1:9911")), "http://127.0.0.1:9911");
        assert_eq!(advertised_url("", listen("[::1]:9911")), "http://[::1]:9911");
        // Genuinely in the clear: warn, and still no Secure flag, which is what
        // makes the warning worth printing.
        for remote in ["http://203.0.113.10:28080", "http://hub.example.com"] {
            assert!(!app(remote).secure_cookies(&proto(None)), "{remote}");
            assert!(exposed_over_plain_http(remote), "{remote} is exposed");
        }

        let resolved = advertised_url("", listen("0.0.0.0:28080"));
        assert!(resolved.starts_with("http://") && resolved.ends_with(":28080"), "{resolved}");
        // A host with no outbound route keeps the wildcard, there being nothing
        // else to print; anywhere else the wildcard must not appear.
        if outbound_ip().is_some() {
            assert!(!resolved.contains("0.0.0.0"), "{resolved}");
            assert!(exposed_over_plain_http(&resolved), "{resolved} is exposed");
        }
    }

    #[test]
    fn the_public_page_is_on_unless_it_is_switched_off() {
        let app = app("http://x");
        assert!(app.public_page());
        app.db.set("public_page", "off").unwrap();
        assert!(!app.public_page());
        app.db.set("public_page", "on").unwrap();
        assert!(app.public_page());
    }

    /// Without a timer hyper would wait for a request head forever, both on a
    /// fresh connection and between keep-alive requests.
    #[tokio::test(start_paused = true)]
    async fn a_connection_without_a_request_head_is_closed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, Router::new()));
        let mut silent = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut kept = tokio::net::TcpStream::connect(addr).await.unwrap();
        kept.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
        let mut answer = [0; 12];
        kept.read_exact(&mut answer).await.unwrap();
        assert_eq!(&answer, b"HTTP/1.1 404");
        for (name, conn) in [("silent", &mut silent), ("kept alive", &mut kept)] {
            let mut rest = Vec::new();
            let closed =
                tokio::time::timeout(std::time::Duration::from_secs(31), conn.read_to_end(&mut rest)).await;
            assert!(matches!(closed, Ok(Ok(_))), "the {name} connection is still open");
        }
    }

    /// The proxy is a hub setting shared by every release download. A trailing
    /// slash in the setting must not become a double slash the proxy will not
    /// match.
    #[test]
    fn a_github_proxy_prefixes_a_release_url_and_an_empty_one_does_not() {
        let app = app("");
        let direct = format!("https://github.com/{HUB_REPO}/releases/download/v1/theme.tar.gz");
        assert_eq!(proxied(&app, direct.clone()), direct);
        for set in ["https://ghfast.top", "https://ghfast.top/", "  https://ghfast.top/  "] {
            app.db.set("github_proxy", set).unwrap();
            assert_eq!(proxied(&app, direct.clone()), format!("https://ghfast.top/{direct}"), "{set:?}");
        }
        // Cleared in the panel, which stores an empty string rather than removing
        // the row.
        app.db.set("github_proxy", "").unwrap();
        assert_eq!(proxied(&app, direct.clone()), direct);
    }

    #[test]
    fn first_run_sets_a_password_once_and_leaves_it_alone_after() {
        let app = app("http://x");
        first_run(&app, "http://x").unwrap();
        let hash = app.db.get("admin_password_hash").unwrap();
        assert!(hash.starts_with("$argon2"));
        first_run(&app, "http://x").unwrap();
        assert_eq!(app.db.get("admin_password_hash").unwrap(), hash, "must not rotate on restart");

        app.db.create_session("s", i64::MAX).unwrap();
        new_password(&app.db).unwrap();
        assert_ne!(app.db.get("admin_password_hash").unwrap(), hash, "a reset must rotate it");
        assert!(!app.db.session_valid("s"), "a reset must sign every session out");
    }
}
