//! Optional management extension. Monitoring credentials never authorize these routes.
use crate::{
    api::{fail, Admin},
    auth::{random_token, sha256},
    db::Db,
    notify, Shared,
};
use anyhow::{Context, Result};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
};

mod enroll;
mod lifecycle;
#[cfg(test)]
mod lifecycle_tests;
mod models;
mod tasks;
#[cfg(test)]
mod tests;
mod traffic;

pub use self::{enroll::*, tasks::*};

/// What a component and this hub must agree on, as one number: bump it whenever a
/// request or report changes shape. While they differ the node keeps running and
/// reports presence, but exchanges no snapshots or work, so hub and components can
/// be upgraded in either order. Same constant in `management/protocol.py`.
const PROTOCOL: i64 = 1;

use models::{core_version, CoreSnapshot};

pub fn ensure(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS managed_node (
      node INTEGER PRIMARY KEY REFERENCES node(id) ON DELETE CASCADE,
      enrollment TEXT NOT NULL DEFAULT '', expires INTEGER NOT NULL DEFAULT 0,
      credential TEXT NOT NULL DEFAULT '', last_seen INTEGER NOT NULL DEFAULT 0,
      snapshot TEXT NOT NULL DEFAULT '');
      CREATE TABLE IF NOT EXISTS managed_operation (
      id TEXT PRIMARY KEY, node INTEGER NOT NULL REFERENCES managed_node(node) ON DELETE CASCADE,
      request_hash TEXT NOT NULL, payload TEXT NOT NULL, status TEXT NOT NULL,
      message TEXT NOT NULL DEFAULT '', created INTEGER NOT NULL, expires INTEGER NOT NULL,
      module TEXT NOT NULL DEFAULT '', action TEXT NOT NULL DEFAULT '');",
    )?;
    c.execute_batch("CREATE TABLE IF NOT EXISTS managed_maintenance (node INTEGER PRIMARY KEY REFERENCES managed_node(node) ON DELETE CASCADE, token TEXT NOT NULL, expires INTEGER NOT NULL);")?;
    c.execute_batch(traffic::SCHEMA)?;
    let columns = {
        let mut statement = c.prepare("PRAGMA table_info(managed_operation)")?;
        let result = statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        result
    };
    for (name, sql) in [
        ("module", "ALTER TABLE managed_operation ADD COLUMN module TEXT NOT NULL DEFAULT ''"),
        ("action", "ALTER TABLE managed_operation ADD COLUMN action TEXT NOT NULL DEFAULT ''"),
    ] {
        if !columns.iter().any(|column| column == name) {
            c.execute(sql, [])?;
        }
    }
    lifecycle::ensure(c)?;
    let node_columns = {
        let mut statement = c.prepare("PRAGMA table_info(managed_node)")?;
        let result = statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        result
    };
    for (name, sql) in [
        ("component", "ALTER TABLE managed_node ADD COLUMN component TEXT NOT NULL DEFAULT ''"),
        ("protocol", "ALTER TABLE managed_node ADD COLUMN protocol INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !node_columns.iter().any(|column| column == name) {
            c.execute(sql, [])?;
        }
    }
    for retired in ["subscription", "lifecycle_supported"] {
        if node_columns.iter().any(|column| column == retired) {
            c.execute(&format!("ALTER TABLE managed_node DROP COLUMN {retired}"), [])?;
        }
    }
    Ok(())
}

// Restore candidates lose authority before replacing the live DB. Never resurrect bindings.
pub fn reset_after_restore(c: &Connection) -> Result<()> {
    c.execute_batch("DROP TABLE IF EXISTS managed_port_traffic; DROP TABLE IF EXISTS managed_maintenance; DROP TABLE IF EXISTS managed_operation; DROP TABLE IF EXISTS managed_node;")?;
    ensure(c)?;
    lifecycle::reset_after_restore(c)?;
    Ok(())
}

/// Candidate versions remain read-only until the local lifecycle health check commits.
pub async fn lifecycle_barrier(
    State(app): State<Shared>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path();
    let writes = request.method() != axum::http::Method::GET && request.method() != axum::http::Method::HEAD;
    // Reads pass without touching the database: this runs for every request, and
    // the setting sits behind the writer lock that a VACUUM can hold for minutes.
    if ((writes && path != "/api/management-agent/v1/poll") || path == "/api/auth/github/callback")
        && app.db.get("hub_lifecycle_maintenance").as_deref() == Some("on")
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CACHE_CONTROL, "no-store")],
            "后台正在维护，请稍后重试",
        )
            .into_response();
    }
    next.run(request).await
}

fn now() -> i64 {
    Utc::now().timestamp()
}
fn enabled(db: &Db) -> Result<()> {
    if db.get("hub_lifecycle_maintenance").as_deref() == Some("on") {
        refuse!("后台正在维护，请稍后重试");
    }
    if db.get("management_enabled").as_deref() != Some("on") {
        refuse!("节点管理未启用");
    }
    Ok(())
}
fn key(db: &Db) -> Result<[u8; 32]> {
    let file = db.file();
    if file.is_empty() {
        refuse!("管理功能需要持久数据库");
    }
    let path = format!("{file}.management.key");
    if let Ok(meta) = fs::symlink_metadata(&path) {
        if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
            refuse!("管理密钥文件必须为普通文件且权限为 0600");
        }
    }
    if !std::path::Path::new(&path).exists() {
        let c = db.conn();
        let has: i64 =
            c.query_row("SELECT count(*) FROM managed_node WHERE snapshot!=''", [], |r| r.get(0))?;
        let ops: i64 = c.query_row("SELECT count(*) FROM managed_operation", [], |r| r.get(0))?;
        let cleanup: i64 =
            c.query_row("SELECT count(*) FROM managed_lifecycle WHERE cleanup_cipher!=''", [], |r| r.get(0))?;
        if has + ops + cleanup > 0 {
            refuse!("管理加密密钥丢失，请恢复配套密钥文件，禁止自动重建");
        }
        let mut bytes = [0u8; 32];
        SystemRandom::new().fill(&mut bytes).map_err(|_| anyhow::anyhow!("random failed"))?;
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        if let Some(parent) = std::path::Path::new(&path).parent() {
            fs::File::open(if parent.as_os_str().is_empty() { std::path::Path::new(".") } else { parent })?
                .sync_all()?;
        }
    }
    fs::read(path)?.try_into().map_err(|_| anyhow::anyhow!("invalid management key"))
}
fn crypt_key(bytes: &[u8; 32]) -> Result<aead::LessSafeKey> {
    Ok(aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, bytes).map_err(|_| anyhow::anyhow!("key invalid"))?,
    ))
}
fn seal(bytes: &[u8; 32], node: i64, v: &Value) -> Result<String> {
    let mut nonce = [0; 12];
    SystemRandom::new().fill(&mut nonce).map_err(|_| anyhow::anyhow!("random failed"))?;
    let mut data = serde_json::to_vec(v)?;
    crypt_key(bytes)?
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(node.to_be_bytes()),
            &mut data,
        )
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    let mut out = nonce.to_vec();
    out.extend(data);
    Ok(hex::encode(out))
}
fn open(bytes: &[u8; 32], node: i64, value: &str) -> Result<Value> {
    let mut data = hex::decode(value)?;
    if data.len() < 28 {
        anyhow::bail!("ciphertext truncated");
    }
    let nonce: [u8; 12] = data[..12].try_into()?;
    let plain = crypt_key(bytes)?
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(node.to_be_bytes()),
            &mut data[12..],
        )
        .map_err(|_| anyhow::anyhow!("management decrypt failed"))?;
    Ok(serde_json::from_slice(plain)?)
}
fn response(result: Result<Value>) -> Response {
    match result {
        Ok(v) => ([(header::CACHE_CONTROL, "no-store")], Json(v)).into_response(),
        Err(e) if e.downcast_ref::<BindingRevoked>().is_some() => (
            StatusCode::UNAUTHORIZED,
            [("x-node-manager-binding", "revoked"), ("cache-control", "no-store")],
            Json(json!({"error":"管理绑定已撤销或不存在"})),
        )
            .into_response(),
        Err(e) => fail(e),
    }
}
#[derive(Debug)]
struct BindingRevoked;
impl std::fmt::Display for BindingRevoked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("管理绑定已撤销或不存在")
    }
}
impl std::error::Error for BindingRevoked {}
fn token(h: &HeaderMap) -> Result<&str> {
    h.get(header::AUTHORIZATION)
        .and_then(|s| s.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .filter(|s| s.len() >= 32 && s.len() <= 128)
        .context(crate::Shown("管理凭据无效".into()))
}
fn identity(db: &Db, h: &HeaderMap) -> Result<i64> {
    let digest = sha256(token(h)?);
    db.conn()
        .query_row("SELECT node FROM managed_node WHERE credential=?1 AND credential!=''", [digest], |r| {
            r.get(0)
        })
        .optional()?
        .context(BindingRevoked)
}
fn valid_hex(s: &str, length: usize) -> bool {
    s.len() == length && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    enabled: bool,
}
pub async fn settings(_: Admin, State(app): State<Shared>) -> Response {
    response((|| {
        // Upgrades stay a command run on each node; the hub only says which nodes need it.
        let bound = {
            let c = app.db.conn();
            let mut statement = c.prepare(
                "SELECT node,component,last_seen FROM managed_node WHERE credential!='' ORDER BY node",
            )?;
            let rows = statement
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let current = component_digest(FILES);
        let outdated: Vec<_> = bound.iter().filter(|(_, c, _)| *c != current).collect();
        // The same 30 seconds as the node page: whether the component, not the probe, is reachable.
        let offline: Vec<i64> =
            outdated.iter().filter(|(_, _, seen)| *seen <= now() - 30).map(|(n, ..)| *n).collect();
        let outdated: Vec<i64> = outdated.iter().map(|(n, ..)| *n).collect();
        Ok(json!({"enabled":app.db.get("management_enabled").as_deref()==Some("on"),"site":app.site,
            "bound":bound.len(),"outdated":outdated,"offline":offline,"alerts":notify::management_delivered(&app)}))
    })())
}
pub async fn configure(_: Admin, State(app): State<Shared>, Json(input): Json<Setting>) -> Response {
    response((|| {
        if input.enabled {
            let url = reqwest::Url::parse(&app.site)
                .context(crate::Shown("请通过 --site 设置 HTTPS 公网地址".into()))?;
            if url.scheme() != "https"
                || url.host_str().is_none()
                || url.path() != "/"
                || url.query().is_some()
                || url.fragment().is_some()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                refuse!("公网地址必须为 HTTPS 根地址");
            }
            key(&app.db)?;
        }
        app.db.set("management_enabled", if input.enabled { "on" } else { "off" })?;
        Ok(json!({"enabled":input.enabled}))
    })())
}

#[derive(Deserialize)]
pub struct TrafficPort {
    port: u16,
}
pub async fn port_traffic(
    _: Admin,
    State(app): State<Shared>,
    Path(node): Path<i64>,
    Query(input): Query<TrafficPort>,
) -> Response {
    response(traffic::read(&app.db.conn(), node, input.port.into(), now()))
}
#[derive(Deserialize, Default)]
pub struct ExportFormat {
    #[serde(default)]
    format: String,
    port: Option<u16>,
}
fn render_configuration(value: Value, format: &str) -> Result<String> {
    let snap: Snapshot = serde_json::from_value(value)?;
    snap.validate()?;
    if snap.blocked {
        refuse!("节点配置待恢复，暂停下载配置");
    }
    if matches!(format, "sing-box" | "share") {
        let sb = snap.modules.get("sb").context(crate::Shown("SB 尚未接入".into()))?;
        return if format == "share" { models::share_links(sb) } else { models::sing_box(sb) };
    }
    if !matches!(format, "" | "surge") {
        refuse!("不支持的客户端配置格式");
    }
    if snap.resources.is_empty() {
        refuse!("Snell 尚无可下载配置");
    }
    let mut text = String::from("[Proxy]\n");
    for r in snap.resources {
        let host = if r.host.contains(':') { format!("[{}]", r.host) } else { r.host };
        text.push_str(&format!(
            "{}-{} = snell, {}, {}, psk={}, version=6, tfo=true, reuse=true, mode={}\n",
            r.name, r.port, host, r.public_port, r.psk, r.mode
        ));
    }
    Ok(text)
}
fn render_instance(mut value: Value, format: &str, port: u16) -> Result<String> {
    let resources = if matches!(format, "sing-box" | "share") {
        &mut value["modules"]["sb"]["resources"]
    } else {
        &mut value["resources"]
    };
    let rows = resources.as_array_mut().context(crate::Shown("模块尚无配置".into()))?;
    rows.retain(|r| r["port"].as_u64() == Some(u64::from(port)));
    if rows.len() != 1 {
        refuse!("实例不存在，请刷新配置");
    }
    let text = render_configuration(value, format)?;
    if format == "sing-box" {
        let client: Value = serde_json::from_str(&text)?;
        let outbound = client["outbounds"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["type"] != "selector"))
            .context(crate::Shown("实例配置不可用".into()))?;
        return Ok(serde_json::to_string_pretty(outbound)?);
    }
    Ok(text.strip_prefix("[Proxy]\n").unwrap_or(&text).trim_end().to_owned())
}

pub async fn export_configuration(
    _: Admin,
    State(app): State<Shared>,
    Path(node): Path<i64>,
    Query(format): Query<ExportFormat>,
) -> Response {
    let result = (|| -> Result<String> {
        lifecycle::guard(&app.db.conn(), node)?;
        let cipher: Option<String> = app
            .db
            .conn()
            .query_row("SELECT snapshot FROM managed_node WHERE node=?1 AND credential!=''", [node], |r| {
                r.get(0)
            })
            .optional()?;
        let cipher = cipher.filter(|v| !v.is_empty()).context(crate::Shown("尚无已确认配置".into()))?;
        let value = open(&key(&app.db)?, node, &cipher)?;
        match format.port {
            Some(port) => render_instance(value, &format.format, port),
            None => render_configuration(value, &format.format),
        }
    })();
    match result {
        Ok(text) => (
            [
                (header::CACHE_CONTROL, "no-store"),
                (header::REFERRER_POLICY, "no-referrer"),
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            ],
            text,
        )
            .into_response(),
        Err(error) => fail(error),
    }
}
pub fn admin_routes() -> Router<Shared> {
    Router::new()
        .route("/api/management", get(settings).post(configure))
        .route("/api/management/{node}", get(status).delete(unbind))
        .route("/api/management/{node}/abandon", post(abandon))
        .route("/api/management/{node}/enrollment", post(enrollment))
        .route("/api/management/{node}/operations", post(submit))
        .route("/api/management/{node}/lifecycle", post(lifecycle::start))
        .route("/api/management/{node}/export", get(export_configuration))
        .route("/api/management/{node}/traffic", get(port_traffic))
}
pub fn agent_routes() -> Router<Shared> {
    Router::new()
        .route("/api/management-agent/v1/enroll", post(enroll))
        .route("/api/management-agent/v1/lifecycle/finish", post(lifecycle::finish))
        .route("/api/management-agent/v1/maintenance", post(maintenance))
}
