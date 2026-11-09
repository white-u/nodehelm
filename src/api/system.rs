//! The database itself, version information, sessions and settings.

use super::*;

/// Settings the panel may read. Secrets are deliberately excluded: the client can
/// set the GitHub secret but never read it back.
const READABLE_SETTINGS: &[&str] = &[
    "site_name",
    "public_page",
    "github_client_id",
    "github_allowed_users",
    "retention_days",
    "theme",
    "github_proxy",
    "update_notice",
    "favicon",
    "touch_icon",
    "origin_cdn_only",
];

// ---- the database itself ----

/// The largest single request the two upload routes accept, and the reason they
/// sit outside the router's 64 KiB body limit. It is twice the 4 MiB the panel
/// sends, so the chunk size remains the panel's concern alone and requires no
/// negotiated handshake.
///
/// **This, not the two ceilings below, is what a reverse proxy must pass.** A
/// backup of any size arrives 4 MiB at a time, so `client_max_body_size` no
/// longer tracks the size of the database.
pub const MAX_CHUNK: usize = 8 * 1024 * 1024;

/// Whole-file ceilings, one per route, checked against the declared `total` on
/// the first request rather than by counting bytes as they arrive, so an
/// oversized upload is refused before a byte is sent.
///
/// The backup ceiling bounds how long a restore holds the connection every read
/// and write passes through, while the panel and the public page wait and the
/// agents' reports queue: a 1.5 GiB copy measured 20.5 s from the page cache, so
/// 1 GiB is 14 s, or 27 s at the ~40 MB/s measured from disk -- within the 90 s
/// an agent waits before reconnecting. It clears what history can reach: at 300
/// nodes with four probes, a week of minute rows and a year of hourly ones come
/// to about 720 MiB.
pub const MAX_RESTORE: u64 = 1024 * 1024 * 1024;
pub const MAX_THEME: u64 = 32 * 1024 * 1024;

/// One request of an upload: `total` is the whole file, `offset` where this piece
/// belongs within it.
///
/// There is no upload id, session or server-side bookkeeping: the state of an
/// upload is the length of the file on disk. A piece continues an upload only if
/// it begins exactly where the last ended, `offset = 0` truncates whatever an
/// interrupted attempt left behind, and nothing is ever left to collect.
#[derive(Deserialize)]
pub struct Chunk {
    pub(super) offset: u64,
    pub(super) total: u64,
}

/// Appends one piece to `path`, returning the file's length afterwards; the
/// caller compares that against `total` to determine completion.
///
/// A piece lands whole or not at all -- a failure truncates back to where it
/// began -- so retrying one always aligns on the same offset.
///
/// ponytail: strictly sequential, one round trip per chunk. Concurrent pieces
/// would require pwrite, a commit step and a hash to prove there are no gaps,
/// and would save about a second on a 6.7 MB backup.
pub(super) async fn receive(
    path: &str,
    chunk: &Chunk,
    max: u64,
    body: axum::body::Body,
) -> Result<u64, anyhow::Error> {
    if chunk.total == 0 || chunk.total > max {
        refuse!("文件必须在 1 字节到 {} MiB 之间", max / 1024 / 1024);
    }
    if chunk.offset > chunk.total {
        refuse!("分片位置越过了文件末尾");
    }

    let mut options = std::fs::OpenOptions::new();
    // Only the first piece may create the file, and it truncates: whatever an
    // interrupted upload left behind is overwritten rather than accumulated.
    if chunk.offset == 0 {
        options.write(true).create(true).truncate(true);
    } else {
        options.append(true);
    }
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            refuse!("这次上传已经不在了，请从头开始")
        }
        Err(e) => return Err(e.into()),
    };

    let already = file.metadata()?.len();
    if already != chunk.offset {
        refuse!("分片接不上：已经收到 {already} 字节，这一片却从 {} 开始", chunk.offset);
    }

    match append(&mut file, chunk, body).await {
        Ok(received) => Ok(chunk.offset + received),
        Err(e) => {
            // Undo a partially written piece so a retry aligns again.
            let _ = file.set_len(chunk.offset);
            Err(e)
        }
    }
}

/// Streams one request body onto the end of `file`. The byte count is checked
/// here as well as by the route's body limit: these are the only paths on the hub
/// that write a caller's bytes to disk, so they do not depend on a layer that
/// could be reordered away.
async fn append(
    file: &mut std::fs::File,
    chunk: &Chunk,
    body: axum::body::Body,
) -> Result<u64, anyhow::Error> {
    use anyhow::Context;
    use std::io::Write;
    use std::pin::Pin;

    let mut stream = body.into_data_stream();
    let mut received = 0u64;
    while let Some(piece) =
        std::future::poll_fn(|cx| futures_core::Stream::poll_next(Pin::new(&mut stream), cx)).await
    {
        // The client went away mid-piece, as a cancelled upload does.
        let piece = piece.context(crate::Shown("上传中断了，请重新上传".into()))?;
        received += piece.len() as u64;
        if chunk.offset + received > chunk.total {
            refuse!("这一片超出了声明的文件大小");
        }
        file.write_all(&piece)?;
    }
    Ok(received)
}

/// A scratch file beside the database, so the copy lands on the same filesystem
/// the database has room on. The random component keeps two concurrent calls
/// apart, since `VACUUM INTO` refuses a file that is not empty.
fn scratch_path(app: &App, kind: &str) -> String {
    format!("{}.{kind}-{}.tmp", app.db.file(), &random_token()[..16])
}

/// The data page's figures.
///
/// Off the runtime, like the three routes below: `stats` counts every row of
/// both tiers of history -- all WITHOUT ROWID, so each count is a full index
/// scan -- 127 ms at 2.2M rows with the file cached, growing with
/// `retention_days`, and uncached, as long as reading the file takes.
pub async fn db_stats(_: Admin, State(app): State<Shared>) -> Response {
    match tokio::task::spawn_blocking(move || app.db.stats()).await {
        Ok(Ok(stats)) => Json(stats).into_response(),
        Ok(Err(e)) => fail(e),
        Err(e) => fail(anyhow::anyhow!(e)),
    }
}

/// Returns a compact copy of the whole database.
///
/// The copy is written beside the live file and then unlinked while still open,
/// so it exists only for the duration of this response: a client that
/// disconnects partway through leaves nothing behind, and nothing on disk
/// outlives the download.
pub async fn db_backup(_: Admin, State(app): State<Shared>) -> Response {
    let path = scratch_path(&app, "backup");
    // Off the runtime: this reads the entire database, through a connection of
    // its own, so the agents' writes continue meanwhile.
    let copied = {
        let (app, path) = (app.clone(), path.clone());
        tokio::task::spawn_blocking(move || app.db.backup_into(&path)).await
    };
    if let Err(e) = copied.map_err(|e| anyhow::anyhow!(e)).and_then(|r| r) {
        let _ = std::fs::remove_file(&path);
        return fail(e);
    }
    let opened = tokio::fs::File::open(&path).await;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(&path);
    match opened {
        Ok(file) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                (header::CONTENT_LENGTH, size.to_string()),
                // The entire credential store: no shared cache may retain a copy.
                (header::CACHE_CONTROL, "no-store".to_owned()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"monitor-{}.db\"", Local::now().format("%Y%m%d-%H%M%S")),
                ),
            ],
            axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(file)),
        )
            .into_response(),
        Err(e) => fail(e),
    }
}

/// Replaces the live database with an uploaded backup, one chunk per request.
///
/// The upload streams to a file beside the database and is validated in full
/// before a single page is copied; see `Db::check_backup`. Afterwards every
/// session in the restored file is dropped and the caller is issued a new one: a
/// backup carries the session rows it held when taken, and restoring it must not
/// revive logged-out sessions.
pub async fn db_restore(
    _: Admin,
    State(app): State<Shared>,
    Query(chunk): Query<Chunk>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    let Some(caller) = current_session(&headers) else {
        return answer(StatusCode::UNAUTHORIZED, "登录已失效，请重新登录");
    };
    // One fixed path, which is what allows the file's own length to constitute
    // the entire protocol.
    // ponytail: one upload in flight per hub. Two started simultaneously land on
    // this same name, and equal-sized chunks align their offsets, so they splice
    // rather than collide. The cost is a failed upload; distinguishing them would
    // require the upload id the protocol deliberately omits.
    let path = format!("{}.upload", app.db.file());
    let received = match receive(&path, &chunk, MAX_RESTORE, body).await {
        Ok(received) => received,
        Err(e) => return fail(e),
    };
    if received < chunk.total {
        return Json(json!({"received": received})).into_response();
    }

    // Moved off the upload name before a byte is read. Splicing costs an upload;
    // what it must not cost is the live database, which without this it could:
    // the other upload would continue appending through its own handle while
    // `check_backup` reads the file and the page copy follows, and SQLite cannot
    // observe a write it did not make. A file that passed every gate would then
    // be copied over in a different state. Afterwards the other upload's next
    // chunk finds nothing and is told to restart, which is the error it already
    // has for an upload that disappeared.
    //
    // ponytail: the rename itself is not covered by a test. What it changes is
    // which path is open during the read, and reaching that would require a
    // second upload landing inside the copy. What is verified afterwards is that
    // neither name is left behind, in
    // `a_finished_restore_leaves_no_scratch_file_behind`.
    let source = scratch_path(&app, "restoring");
    if let Err(e) = std::fs::rename(&path, &source) {
        let _ = std::fs::remove_file(&path);
        return fail(e);
    }

    let token = random_token();
    let outcome = restore(&app, &source, caller, sha256(&token)).await;
    // SQLite writes a -wal and a -shm beside any file it opens in WAL mode, and a
    // plain copy of a running hub's database is exactly that. They are removed
    // when the connection closes cleanly; these three lines cover the case where
    // it does not.
    for leftover in [source.clone(), format!("{source}-wal"), format!("{source}-shm")] {
        let _ = std::fs::remove_file(leftover);
    }
    match outcome {
        Ok(true) => {
            // Agents authenticate at the handshake, and the tokens they hold may
            // now belong to different nodes, or to none. Dropping the senders ends
            // those loops; each reconnects against the restored database.
            app.agents.write().unwrap_or_else(|e| e.into_inner()).clear();
            // Held readings belong to the database just replaced.
            app.readings.lock().unwrap_or_else(|e| e.into_inner()).clear();
            invalidate_snapshot(&app);
            with_cookies(Json(json!({"ok": true})), [session_cookie(&app, &headers, &token)])
        }
        Ok(false) => answer(StatusCode::UNAUTHORIZED, "登录已失效，请重新登录"),
        Err(e) => fail(e),
    }
}

async fn restore(
    app: &Shared,
    path: &str,
    caller: String,
    replacement: String,
) -> Result<bool, anyhow::Error> {
    // Both halves read the whole file, off the runtime: `PRAGMA integrity_check`
    // on a 1 GiB upload is not runtime work, and the copy that follows holds
    // the connection the agents write through.
    let (app, source) = (app.clone(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        app.db.check_backup(&source)?;
        app.db.restore_authenticated(&source, &caller, (&replacement, session_expiry()))
    })
    .await?
}

/// Drops history beyond the retention window and rebuilds the file around what
/// remains, which is the only way SQLite returns the space to the filesystem.
pub async fn db_vacuum(_: Admin, State(app): State<Shared>) -> Response {
    let keep = app.db.retention_days();
    let app = app.clone();
    // A rebuild of the whole file, holding the connection the agents write
    // through, so it belongs on a blocking thread.
    let done = tokio::task::spawn_blocking(move || {
        let pruned = app.db.prune(keep)?;
        app.db.vacuum().map(|freed| json!({"pruned": pruned, "freed": freed}))
    })
    .await;
    match done.map_err(|e| anyhow::anyhow!(e)).and_then(|r| r) {
        Ok(result) => Json(result).into_response(),
        Err(e) => fail(e),
    }
}

/// How long a release lookup stands before the panel asks GitHub again, and how
/// long a failed one does. The short retry keeps one unreachable moment from
/// hiding an update for the rest of the day; the long one keeps a panel left
/// open on a screen to four lookups a day, well inside the 60 per hour an
/// unauthenticated caller is allowed.
const RELEASES_FRESH: i64 = 6 * 3600;
const RELEASES_RETRY: i64 = 600;

/// What is running here, and what is published. The agent's own version travels
/// in its hello and is already in the node list, so the panel compares the two
/// itself and names the nodes to upgrade.
///
/// Admin-only, and deliberately not part of `/api/me`: that route answers
/// anonymous callers, to whom the running hub version is not disclosed. Nothing
/// is fetched until an administrator opens the panel, so a hub whose panel is
/// never opened makes no outbound request.
pub async fn versions(_: Admin, State(app): State<Shared>) -> Json<Value> {
    let cached = app.releases.lock().unwrap().clone();
    let latest = if fresh_enough(&cached, Utc::now().timestamp()) {
        cached
    } else {
        let hub = latest_tag(&app, crate::HUB_REPO).await;
        let read = crate::Releases { read_at: Utc::now().timestamp(), hub: hub.unwrap_or_default() };
        *app.releases.lock().unwrap() = read.clone();
        read
    };
    Json(json!({
        "hub": env!("CARGO_PKG_VERSION"),
        // Empty where GitHub could not be reached, which the panel renders as no
        // update rather than as an error: a hub on a network that cannot reach
        // github.com is a supported deployment, not a fault to report.
        "hub_latest": latest.hub,
        // The agent this hub serves, which `install.sh --upgrade` installs: an
        // agent update arrives with a hub update. Empty in a development build.
        "agent_latest": crate::AGENT_VERSION,
        // Whether the navigation marks an update. It governs the mark alone: the
        // lookup runs either way, so the update page still answers when opened.
        "notice": app.db.get("update_notice").as_deref() != Some("off"),
    }))
}

/// Whether the cached lookup still answers. One that returned nothing is held
/// for [`RELEASES_RETRY`] instead, so a single unreachable moment does not hide
/// an update for the rest of the day.
pub(super) fn fresh_enough(cached: &crate::Releases, now: i64) -> bool {
    let holds = if cached.hub.is_empty() { RELEASES_RETRY } else { RELEASES_FRESH };
    // A negative age is a clock set back since the lookup, not a fresh one.
    cached.read_at != 0 && (0..holds).contains(&(now - cached.read_at))
}

/// Every live session, with the caller's own marked.
///
/// `id` is the stored SHA-256 of the session token rather than the token itself:
/// it identifies a row without being presentable as a cookie.
pub async fn sessions(_: Admin, State(app): State<Shared>, headers: HeaderMap) -> Response {
    let mine = current_session(&headers);
    match app.db.sessions() {
        Ok(rows) => Json(
            rows.into_iter()
                .map(|(hash, expires_at)| {
                    json!({
                        "current": mine.as_deref() == Some(hash.as_str()),
                        "created_at": issued_at(expires_at),
                        "id": hash,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => fail(e),
    }
}

/// Deleting a row that no longer exists is not an error: two panels open on the
/// same list both achieve the requested sign-out.
pub async fn delete_session(_: Admin, State(app): State<Shared>, Path(id): Path<String>) -> Response {
    match app.db.drop_session(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => fail(e),
    }
}

pub async fn settings(
    _: Admin,
    State(app): State<Shared>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Json<Value> {
    let mut out = serde_json::Map::new();
    // The address this request is taken for under the saved `origin_cdn_only`,
    // so the admin can see whether the switch recognizes them: behind a CDN the
    // proxy's own address means it does not.
    out.insert("your_address".into(), json!(client_ip(&app, &headers, peer.ip()).to_string()));
    for key in READABLE_SETTINGS {
        out.insert((*key).to_owned(), json!(app.db.get(key).unwrap_or_default()));
    }
    // The one readable key with a default that also rejects the empty string:
    // `setting_error` below refuses "" and `save_settings` writes nothing when any
    // key fails, so answering "" for a hub where this was never set would have
    // the entire settings form rejected, naming a field that was never edited.
    // `retention_days()` already holds the default `prune` and the data page read,
    // so it answers here as well.
    out.insert("retention_days".into(), json!(app.db.retention_days().to_string()));
    out.insert(
        "github_secret_set".into(),
        json!(app.db.get("github_client_secret").is_some_and(|v| !v.is_empty())),
    );
    // Read-only here. A window is opened and closed through its own route, so the
    // key is always one the hub generated, and `save_settings` continues to refuse
    // both stored names.
    out.insert("register_key".into(), json!(app.db.get("register_key").unwrap_or_default()));
    out.insert("register_left".into(), json!(register_left(&app)));
    crate::notify::settings(&app, &mut out);
    Json(Value::Object(out))
}

/// Why one setting cannot be stored, or `None` when it can.
///
/// Separate from the write below because every key is validated before any is
/// written: changing the password drops every session, and a 400 raised
/// afterwards -- on a later key, in whatever order the map iterates -- carries no
/// Set-Cookie, signing the admin out of every device through a password change
/// the UI reported as rejected.
fn setting_error(app: &App, key: &str, value: &Value) -> Option<String> {
    // Settings are stored as text. A caller sending the natural JSON type --
    // `{"public_page": false}`, `{"retention_days": 7}` -- is refused rather than
    // skipped, which would write nothing while the response reported success.
    let Some(value) = value.as_str() else { return Some(format!("设置 {key} 的值格式不对")) };
    match key {
        "theme" if !crate::frontend::selectable(app, value) => Some("主题没有安装".into()),
        // Housekeeping clamps whatever it reads, so an unparsable value would be
        // stored, echoed back, and silently mean the default indefinitely.
        "retention_days"
            if !value.parse::<i64>().is_ok_and(|d| (1..=db::MAX_RETENTION_DAYS).contains(&d)) =>
        {
            Some(format!("历史保留天数要在 1 到 {} 之间", db::MAX_RETENTION_DAYS))
        }
        // The hub fetches this URL itself, so it must be one: a scheme it cannot
        // speak turns every agent download into a 502 that says nothing about the
        // setting responsible.
        //
        // https only. What returns from this host is the agent binary, which
        // `install.sh` writes to /opt/monitor and starts on every node provisioned
        // here; over http:// anyone on the path between the hub and the mirror
        // chooses that binary, while the node still sees a valid TLS connection to
        // the hub.
        "github_proxy" if !(value.is_empty() || value.starts_with("https://")) => {
            Some("GitHub 代理必须以 https:// 开头：agent 程序经它下载，再安装到每个节点".into())
        }
        "favicon" | "touch_icon" if !value.is_empty() => {
            crate::frontend::site_icon(value).err().map(Into::into)
        }
        "admin_password" if value.len() < 12 => Some("密码至少 12 位".into()),
        "admin_password" => None,
        "origin_cdn_only" if !matches!(value, "" | "on" | "off") => {
            Some("源站只允许 CDN 访问：取值是 on 或 off".into())
        }
        k if k.starts_with("notify_") => crate::notify::setting_error(k, value),
        k if READABLE_SETTINGS.contains(&k) || k == "github_client_secret" => None,
        _ => Some(format!("没有这个设置项：{key}")),
    }
}

pub async fn save_settings(
    _: Admin,
    State(app): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some(caller) = current_session(&headers) else {
        return answer(StatusCode::UNAUTHORIZED, "登录已失效，请重新登录");
    };
    let Some(map) = body.as_object() else { return bad("设置格式不对") };
    let mut values = Vec::with_capacity(map.len());
    let mut password = None;
    for (key, value) in map {
        if let Some(message) = setting_error(&app, key, value) {
            return bad(&message);
        }
        let value = value.as_str().expect("setting_error validates the value type");
        if key == "admin_password" {
            password = Some(value);
        } else {
            values.push((key.as_str(), value));
        }
    }
    let password_hash = match password.map(hash_password).transpose() {
        Ok(hash) => hash,
        Err(e) => return fail(e),
    };
    let token = random_token();
    let token_hash = sha256(&token);
    match app.db.save_panel_settings(
        &caller,
        &values,
        password_hash.as_deref(),
        (&token_hash, session_expiry()),
    ) {
        Ok(Some(true)) => with_cookies(Json(json!({"ok": true})), [session_cookie(&app, &headers, &token)]),
        Ok(Some(false)) => Json(json!({"ok": true})).into_response(),
        Ok(None) => answer(StatusCode::UNAUTHORIZED, "登录已失效，请重新登录"),
        Err(e) => fail(e),
    }
}
