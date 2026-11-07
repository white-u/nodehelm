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

mod lifecycle;
mod models;
mod traffic;

/// What a component and this hub must agree on, as one number: bump it whenever a
/// request or report changes shape. While they differ the node keeps running and
/// reports presence, but exchanges no snapshots or work, so hub and components can
/// be upgraded in either order. Same constant in `management/model.py`.
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
pub async fn enrollment(_: Admin, State(app): State<Shared>, Path(node): Path<i64>) -> Response {
    response((|| {
        enabled(&app.db)?;
        if app.db.node(node)?.is_none() {
            refuse!("节点不存在");
        }
        let code = random_token();
        let c = app.db.conn();
        lifecycle::guard(&c, node)?;
        let bound: Option<String> = c
            .query_row("SELECT credential FROM managed_node WHERE node=?1", [node], |r| r.get(0))
            .optional()?;
        if bound.is_some_and(|s| !s.is_empty()) {
            refuse!("节点已绑定，请先解绑");
        }
        c.execute("INSERT INTO managed_node(node,enrollment,expires) VALUES(?1,?2,?3) ON CONFLICT(node) DO UPDATE SET enrollment=excluded.enrollment,expires=excluded.expires",params![node,sha256(&code),now()+600])?;
        Ok(json!({"node":node,"enrollment":code,"expires":now()+600,"site":app.site}))
    })())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Enrollment {
    node: i64,
    enrollment: String,
    credential: String,
}
pub async fn enroll(State(app): State<Shared>, Json(input): Json<Enrollment>) -> Response {
    response((|| {
        enabled(&app.db)?;
        if !valid_hex(&input.credential, 64) || input.enrollment.len() > 128 {
            refuse!("登记凭据格式错误");
        }
        let c = app.db.conn();
        lifecycle::guard(&c, input.node)?;
        let code = sha256(&input.enrollment);
        let cred = sha256(&input.credential);
        let changed=c.execute("UPDATE managed_node SET credential=?1 WHERE node=?2 AND enrollment=?3 AND ((credential='' AND expires>?4) OR credential=?1)",params![cred,input.node,code,now()])?;
        if changed != 1 {
            refuse!("登记已过期、已使用或节点不存在，请重新生成安装命令");
        }
        Ok(json!({"bound":true,"node":input.node}))
    })())
}
pub async fn unbind(_: Admin, State(app): State<Shared>, Path(node): Path<i64>) -> Response {
    response((|| {
        let mut c = app.db.conn();
        let tx = c.transaction()?;
        lifecycle::guard(&tx, node)?;
        let maintenance: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM managed_maintenance WHERE node=?1 AND expires>?2)",
            params![node, now()],
            |r| r.get(0),
        )?;
        if maintenance {
            refuse!("管理组件正在维护，暂不能撤销绑定");
        }
        let active: i64 = tx.query_row(
            "SELECT count(*) FROM managed_operation WHERE node=?1 AND status='delivered'",
            [node],
            |r| r.get(0),
        )?;
        if active > 0 {
            refuse!("有已发送操作待核实，不能解绑；请恢复管理连接或在 VPS 核实操作");
        }
        tx.execute("DELETE FROM managed_node WHERE node=?1", [node])?;
        tx.commit()?;
        Ok(json!({"message":"已撤销管理绑定，VPS 上业务服务保持运行"}))
    })())
}
/// How long a node must have been silent before an administrator can give up on it.
const ABANDON_AFTER: i64 = 600;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Abandon {
    name: String,
}
/// The way out for a VPS that no longer exists. Unbinding waits for the node to
/// confirm its work, which a destroyed machine never will, and that wait would
/// otherwise also block deleting the node and restoring the hub. Nothing on the
/// VPS changes: the old credential stops working, and what happened there stays
/// unknown, which the retained uninstall record says.
pub async fn abandon(
    _: Admin,
    State(app): State<Shared>,
    Path(node): Path<i64>,
    Json(input): Json<Abandon>,
) -> Response {
    response((|| {
        let name = app.db.node(node)?.context(crate::Shown("节点不存在".into()))?.name;
        if input.name != name {
            refuse!("节点名称不一致，未执行");
        }
        let mut c = app.db.conn();
        let tx = c.transaction()?;
        let seen: Option<(String, i64)> = tx
            .query_row("SELECT credential,last_seen FROM managed_node WHERE node=?1", [node], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let uninstalling = lifecycle::active(&tx, node)?;
        if !uninstalling && !seen.as_ref().is_some_and(|(credential, _)| !credential.is_empty()) {
            refuse!("此节点没有管理绑定");
        }
        if seen.is_some_and(|(_, last_seen)| last_seen > now() - ABANDON_AFTER) {
            refuse!("节点最近仍有上报，请等待离线超过 10 分钟后再确认");
        }
        let maintenance: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM managed_maintenance WHERE node=?1 AND expires>?2)",
            params![node, now()],
            |r| r.get(0),
        )?;
        if maintenance {
            refuse!("管理组件正在维护，请等待维护租约到期");
        }
        lifecycle::abandon(&tx, node)?;
        tx.execute("DELETE FROM managed_node WHERE node=?1", [node])?;
        tx.commit()?;
        tracing::warn!("management of node {node} abandoned by an administrator");
        Ok(json!({"message":"已放弃此节点的管理，VPS 上的实际状态未核实"}))
    })())
}
pub async fn status(_: Admin, State(app): State<Shared>, Path(node): Path<i64>) -> Response {
    response((|| {
        let row: Option<(String, i64, String, i64, String)> = app
            .db
            .conn()
            .query_row(
                "SELECT credential,last_seen,snapshot,protocol,component FROM managed_node WHERE node=?1",
                [node],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((credential, seen, snapshot, protocol, component)) = row else {
            let c = app.db.conn();
            return Ok(json!({"bound":false,"compatible":false,
                "lifecycle":lifecycle::latest(&c,node)?,"abandonable":lifecycle::active(&c,node)?}));
        };
        let mut snapshot =
            if snapshot.is_empty() { Value::Null } else { open(&key(&app.db)?, node, &snapshot)? };
        if let Some(items) = snapshot["resources"].as_array_mut() {
            for item in items {
                item.as_object_mut().map(|o| o.remove("psk"));
            }
        }
        if let Some(modules) = snapshot["modules"].as_object_mut() {
            for state in modules.values_mut() {
                if let Some(resources) = state["resources"].as_array_mut() {
                    for item in resources {
                        if let Some(map) = item.as_object_mut() {
                            for key in ["psk", "uuid", "private_key"] {
                                map.remove(key);
                            }
                        }
                    }
                }
            }
        }
        let c = app.db.conn();
        let mut stmt=c.prepare("SELECT id,status,message,created,module,action FROM managed_operation WHERE node=?1 ORDER BY created DESC,rowid DESC LIMIT 30")?;
        let rows=stmt.query_map([node],|r|Ok(json!({"id":r.get::<_,String>(0)?,"status":r.get::<_,String>(1)?,"message":r.get::<_,String>(2)?,"created":r.get::<_,i64>(3)?,"module":r.get::<_,String>(4)?,"action":r.get::<_,String>(5)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
        let abandonable =
            seen <= now() - ABANDON_AFTER && (!credential.is_empty() || lifecycle::active(&c, node)?);
        Ok(
            json!({"bound":!credential.is_empty(),"online":seen>now()-30,"last_seen":seen,"snapshot":snapshot,"operations":rows,"compatible":protocol==PROTOCOL,
                "lifecycle":lifecycle::latest(&c,node)?,"abandonable":abandonable,
                "component_current":component==component_digest(FILES)}),
        )
    })())
}
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ApplyIntent {
    Create,
    Update,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    #[serde(default = "models::default_module", skip_serializing_if = "models::is_snell")]
    module: String,
    id: String,
    action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    intent: Option<ApplyIntent>,
    revision: String,
    payload: Value,
}
fn core_lifecycle(action: &str) -> bool {
    matches!(action, "core_upgrade" | "core_rollback" | "core_uninstall")
}

fn validate_request(op: &Operation) -> Result<()> {
    if !valid_hex(&op.id, 32) || !valid_hex(&op.revision, 64) {
        refuse!("操作编号或配置版本无效");
    }
    if !matches!(op.module.as_str(), "snell" | "sb" | "fw" | "pm") {
        refuse!("不支持的管理模块");
    }
    if op.intent.is_some() && op.action != "apply" {
        refuse!("配置操作意图无效");
    }
    if op.module != "snell" && op.action == "apply" {
        return models::resource(&op.module, &op.payload, false);
    }
    match op.action.as_str() {
        "install" | "core_uninstall" if op.payload == json!({}) => (),
        "core_upgrade" | "core_rollback"
            if op.module != "pm"
                && op.payload.as_object().is_some_and(|o| o.len() == 1)
                && op.payload["version"].as_str().is_some_and(core_version) => {}
        "check"
            if op.module == "fw"
                && op.payload.as_object().is_some_and(|o| o.len() == 1)
                && op.payload["port"].as_u64().is_some_and(|n| (1..=65535).contains(&n)) => {}
        "reset"
            if op.module == "pm"
                && op.payload.as_object().is_some_and(|o| o.len() == 1)
                && op.payload["port"].as_u64().is_some_and(|n| (1024..=65535).contains(&n)) => {}
        "delete"
            if op.payload.as_object().is_some_and(|o| o.len() == 1)
                && op.payload["port"].as_u64().is_some_and(|n| (1..=65535).contains(&n)) => {}
        "apply" => {
            let r: Resource =
                serde_json::from_value(op.payload.clone()).context(crate::Shown("实例字段无效".into()))?;
            r.validate(false)?;
        }
        _ => refuse!("不支持的操作或参数"),
    }
    Ok(())
}
pub async fn submit(
    _: Admin,
    State(app): State<Shared>,
    Path(node): Path<i64>,
    Json(op): Json<Operation>,
) -> Response {
    response((|| {
        enabled(&app.db)?;
        validate_request(&op)?;
        let bytes = key(&app.db)?;
        let value = serde_json::to_value(&op)?;
        let hash = hex::encode(
            ring::hmac::sign(
                &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &bytes),
                &serde_json::to_vec(&value)?,
            )
            .as_ref(),
        );
        let encrypted = seal(&bytes, node, &value)?;
        let mut c = app.db.conn();
        let tx = c.transaction()?;
        let hub_maintenance: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM setting WHERE key='hub_lifecycle_maintenance' AND value='on')",
            [],
            |r| r.get(0),
        )?;
        if hub_maintenance {
            refuse!("后台正在维护，请稍后重试");
        }
        lifecycle::guard(&tx, node)?;
        let old: Option<(i64, String)> = tx
            .query_row("SELECT node,request_hash FROM managed_operation WHERE id=?1", [&op.id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        if let Some((old_node, old_hash)) = old {
            if old_node != node || old_hash != hash {
                refuse!("操作编号已用于不同请求");
            }
            return Ok(json!({"id":op.id}));
        }
        let maintenance: i64 = tx.query_row(
            "SELECT count(*) FROM managed_maintenance WHERE node=?1 AND expires>?2",
            params![node, now()],
            |r| r.get(0),
        )?;
        if maintenance != 0 {
            refuse!("管理组件正在维护，请稍后重试");
        }
        let row: Option<(i64, String, i64)> = tx
            .query_row(
                "SELECT last_seen,snapshot,protocol FROM managed_node WHERE node=?1 AND credential!=''",
                [node],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((seen, snapshot, protocol)) = row else {
            refuse!("请先绑定管理组件");
        };
        if protocol != PROTOCOL && seen >= now() - 30 {
            refuse!("管理组件与后台版本不一致，请先升级组件");
        }
        if seen < now() - 30 || snapshot.is_empty() {
            refuse!("管理组件离线，禁止提交写入");
        }
        let state = open(&bytes, node, &snapshot)?;
        if state["blocked"] == true {
            refuse!("VPS 有未恢复操作，请先处理");
        }
        let selected = if op.module == "snell" { &state } else { &state["modules"][&op.module] };
        if selected.is_null() {
            refuse!("此管理组件尚未支持所选模块，请升级组件后再操作");
        }
        if selected["blocked"] == true {
            refuse!("模块有未恢复操作");
        }
        if selected["revision"] != op.revision {
            refuse!("配置已变化，请刷新");
        }
        if core_lifecycle(&op.action) {
            let core = &selected["core"];
            if core.is_null() {
                refuse!("此管理组件尚未支持核心维护，请先升级组件");
            }
            if selected["installed"] != true {
                refuse!("请先安装所选模块");
            }
            if op.module != "pm" && core["version"].is_null() {
                refuse!("核心版本未知，请在 VPS 核实");
            }
            match op.action.as_str() {
                "core_upgrade"
                    if !core["candidates"]
                        .as_array()
                        .is_some_and(|items| items.contains(&op.payload["version"])) =>
                {
                    refuse!("所选版本不是当前可用的可信升级候选");
                }
                "core_rollback" if core["previous_version"] != op.payload["version"] => {
                    refuse!("所选版本不是当前可用的回退版本");
                }
                "core_uninstall" if !selected["resources"].as_array().is_some_and(Vec::is_empty) => {
                    refuse!("请先删除该模块的全部实例或规则");
                }
                _ => (),
            }
        }
        if let Some(intent) = op.intent {
            let exists = selected["resources"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["port"] == op.payload["port"]));
            match intent {
                ApplyIntent::Create if exists => refuse!("该端口已有实例，请换用端口或编辑现有实例"),
                ApplyIntent::Update if !exists => refuse!("实例不存在，请刷新配置"),
                _ => (),
            }
        }
        tx.execute("UPDATE managed_operation SET status='cancelled',message='发送前已过期',payload='' WHERE node=?1 AND status='queued' AND expires<=?2",params![node,now()])?;
        let active: i64 = tx.query_row(
            "SELECT count(*) FROM managed_operation WHERE node=?1 AND status IN ('queued','delivered')",
            [node],
            |r| r.get(0),
        )?;
        if active > 0 {
            refuse!("该 VPS 有操作正在执行或待核实");
        }
        tx.execute("INSERT INTO managed_operation(id,node,request_hash,payload,status,created,expires,module,action) VALUES(?1,?2,?3,?4,'queued',?5,?6,?7,?8)",params![op.id,node,hash,encrypted,now(),now()+300,op.module,op.action])?;
        tx.commit()?;
        Ok(json!({"id":op.id}))
    })())
}
#[derive(Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct Resource {
    name: String,
    host: String,
    port: u16,
    public_port: u16,
    #[serde(default)]
    psk: String,
    mode: String,
}
impl Resource {
    fn validate(&self, secret: bool) -> Result<()> {
        if self.port == 0
            || self.public_port == 0
            || self.name.is_empty()
            || self.name.chars().count() > 48
            || !self.name.chars().all(|c| c.is_alphanumeric() || " _-".contains(c))
        {
            refuse!("实例名称或端口无效");
        }
        if !models::host(&self.host) {
            refuse!("访问域名或 IP 无效");
        }
        if !matches!(self.mode.as_str(), "default" | "unshaped") {
            refuse!("不支持的 Snell 模式");
        }
        if (secret || !self.psk.is_empty())
            && (!(22..=128).contains(&self.psk.len())
                || !self.psk.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)))
        {
            refuse!("实例凭据无效");
        }
        Ok(())
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    core: Option<CoreSnapshot>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    modules: std::collections::BTreeMap<String, models::ModuleSnapshot>,
    /// Sent by components up to 1.1.5 and kept in their stored snapshots; ignored.
    /// Kept so those reports still parse and can receive the component upgrade.
    #[serde(default, skip_serializing, rename = "port_monitor")]
    _port_monitor: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    runtime: std::collections::BTreeMap<String, String>,
    installed: bool,
    resources: Vec<Resource>,
    revision: String,
    blocked: bool,
}
impl Snapshot {
    fn validate(&self) -> Result<()> {
        if let Some(core) = &self.core {
            core.validate("snell", self.installed)?;
        }
        if self.modules.len() > 3 {
            refuse!("模块列表无效");
        }
        for (name, state) in &self.modules {
            state.validate(name)?;
        }
        if !valid_hex(&self.revision, 64) || self.resources.len() > 64 || self.runtime.len() > 64 {
            refuse!("状态快照无效");
        }
        for (port, state) in &self.runtime {
            if port.parse::<u16>().is_err()
                || !matches!(
                    state.as_str(),
                    "active" | "inactive" | "failed" | "activating" | "deactivating" | "unknown"
                )
            {
                refuse!("运行状态无效");
            }
        }
        let mut ports = std::collections::HashSet::new();
        for r in &self.resources {
            r.validate(true)?;
            if !ports.insert(r.port) {
                refuse!("实例端口重复");
            }
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Poll {
    snapshot: Snapshot,
    result: Option<Receipt>,
    lifecycle_result: Option<lifecycle::Progress>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    #[serde(default)]
    code: Option<String>,
    id: String,
    status: String,
    message: String,
    #[serde(default)]
    revision: String,
}
/// A short, credential-bound lease prevents new work during explicit local maintenance.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Maintenance {
    action: String,
    token: String,
}
pub async fn maintenance(
    State(app): State<Shared>,
    headers: HeaderMap,
    Json(input): Json<Maintenance>,
) -> Response {
    response((|| {
        let node = identity(&app.db, &headers)?;
        if !valid_hex(&input.token, 64) || !matches!(input.action.as_str(), "begin" | "end" | "status") {
            refuse!("维护请求无效");
        }
        let mut c = app.db.conn();
        let tx = c.transaction()?;
        let authorized: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM managed_node WHERE node=?1 AND credential=?2)",
            params![node, sha256(token(&headers)?)],
            |r| r.get(0),
        )?;
        if !authorized {
            refuse!("管理绑定已变化");
        }
        let lifecycle_busy = lifecycle::active(&tx, node)?;
        let busy: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM managed_operation WHERE node=?1 AND (status='delivered' OR (status='queued' AND expires>?2)))",params![node,now()], |r|r.get(0))?;
        let lease: Option<(String, i64)> = tx
            .query_row("SELECT token,expires FROM managed_maintenance WHERE node=?1", [node], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let digest = sha256(&input.token);
        if input.action == "begin" {
            if busy || lifecycle_busy {
                refuse!("仍有未确认管理任务，禁止维护");
            }
            if lease.as_ref().is_some_and(|(t, e)| *e > now() && t != &digest) {
                refuse!("已有维护操作，请等待原操作完成");
            }
            tx.execute("INSERT INTO managed_maintenance(node,token,expires) VALUES(?1,?2,?3) ON CONFLICT(node) DO UPDATE SET token=excluded.token,expires=excluded.expires",params![node,digest,now()+900])?;
        } else if input.action == "end" {
            if lease.as_ref().is_some_and(|(t, _)| t != &digest) {
                refuse!("维护凭据不匹配");
            }
            tx.execute("DELETE FROM managed_maintenance WHERE node=?1 AND token=?2", params![node, digest])?;
        }
        let expires: i64 = tx.query_row(
            "SELECT COALESCE((SELECT expires FROM managed_maintenance WHERE node=?1),0)",
            [node],
            |r| r.get(0),
        )?;
        let last_seen: i64 =
            tx.query_row("SELECT last_seen FROM managed_node WHERE node=?1", [node], |r| r.get(0))?;
        let owned: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM managed_maintenance WHERE node=?1 AND token=?2 AND expires>?3)",
            params![node, digest, now()],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(
            json!({"busy":busy || lifecycle_busy,"maintenance":expires>now(),"expires":expires,"owned":owned,"last_seen":last_seen}),
        )
    })())
}
fn reported_component(headers: &HeaderMap) -> &str {
    headers
        .get("x-node-manager-component")
        .and_then(|value| value.to_str().ok())
        .filter(|value| valid_hex(value, 64))
        .unwrap_or_default()
}
/// The route: the body is parsed only once both sides agree on what it means.
pub async fn poll_route(State(app): State<Shared>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let reported = headers
        .get("x-node-manager-protocol")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);
    if reported != PROTOCOL {
        let seen = (|| -> Result<()> {
            let node = identity(&app.db, &headers)?;
            app.db.conn().execute(
                "UPDATE managed_node SET last_seen=?1,protocol=?2,component=?3 WHERE node=?4",
                params![now(), reported, reported_component(&headers), node],
            )?;
            Ok(())
        })();
        return match seen {
            Ok(()) => (
                StatusCode::CONFLICT,
                [(header::CACHE_CONTROL, "no-store")],
                "管理组件与后台协议不一致，请升级",
            )
                .into_response(),
            Err(error) => response(Err(error)),
        };
    }
    match Json::<Poll>::from_bytes(&body) {
        Ok(input) => poll(State(app), headers, input).await,
        Err(rejection) => rejection.into_response(),
    }
}
pub async fn poll(State(app): State<Shared>, headers: HeaderMap, Json(input): Json<Poll>) -> Response {
    response((|| {
        let node = identity(&app.db, &headers)?;
        input.snapshot.validate()?;
        let bytes = key(&app.db)?;
        let plain = serde_json::to_value(&input.snapshot)?;
        let snapshot = seal(&bytes, node, &plain)?;
        let component = reported_component(&headers);
        let is_enabled = app.db.get("management_enabled").as_deref() == Some("on")
            && app.db.get("hub_lifecycle_maintenance").as_deref() != Some("on");
        let thresholds = notify::pm_thresholds(&app);
        let mut c = app.db.conn();
        let tx = c.transaction()?;
        let authorized: i64 = tx.query_row(
            "SELECT count(*) FROM managed_node WHERE node=?1 AND credential=?2",
            params![node, sha256(token(&headers)?)],
            |r| r.get(0),
        )?;
        if authorized != 1 {
            refuse!("管理绑定已变化");
        }
        let previous: String =
            tx.query_row("SELECT snapshot FROM managed_node WHERE node=?1", [node], |r| r.get(0))?;
        // The first report after binding has nothing to compare against, and an
        // unreadable one must not cost the poll that replaces it.
        let old = if previous.is_empty() { None } else { open(&bytes, node, &previous).ok() };
        let (mut notes, ip_alerts) =
            old.as_ref().map_or_else(Default::default, |old| snapshot_events(old, &plain, &thresholds));
        if let Some(old) = &old {
            traffic::record(&tx, node, old, &plain, now())?;
        }
        if let Some(result) = &input.result {
            if !valid_hex(&result.id, 32)
                || !matches!(
                    result.status.as_str(),
                    "succeeded" | "rolled_back" | "recovery_failed" | "rejected" | "expired" | "checked"
                )
            {
                refuse!("操作结果无效");
            }
            let delivered: Option<(String, String, String, String)> = tx
                .query_row(
                    "SELECT action,payload,message,module FROM managed_operation WHERE id=?1 AND node=?2 AND status='delivered'",
                    params![result.id, node],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            if let Some((action, payload, reported, module)) = delivered {
                // Keep uncertain operations delivered, preventing subsequent writes or unbinding.
                let terminal = if result.status == "recovery_failed" { "delivered" } else { &result.status };
                let message = match (result.status.as_str(), action.as_str()) {
                    ("checked", _) => "目标 TCP 可达，UDP 和公网入口需单独验证",
                    ("succeeded", "core_upgrade") => "已升级核心，公网连通性需另行验证",
                    ("succeeded", "core_rollback") => "已回退核心，公网连通性需另行验证",
                    ("succeeded", "core_uninstall") => "已卸载模块",
                    ("succeeded", _) => "已应用，公网连通性需另行验证",
                    ("rolled_back", action) if core_lifecycle(action) => {
                        "核心操作未完成，已保留或恢复原配置，请检查实例运行状态"
                    }
                    ("rolled_back", _) => "应用失败，已恢复原配置",
                    ("recovery_failed", _) => "恢复失败，请在 VPS 执行 recover",
                    ("expired", _) => "操作已过期，未执行",
                    _ => "请求被拒绝，请刷新配置并检查 VPS 状态",
                };
                let detail = match result.code.as_deref() {
                    Some("core_version") => "核心版本已变化或不在可信候选中，请刷新后检查",
                    Some("core_in_use") => "请先删除该模块的全部实例或规则",
                    Some("core_download") => "核心下载、归档读取或完整性校验失败，请在 VPS 核实",
                    Some("core_runtime") => "核心或业务实例健康检查失败，请在 VPS 核实",
                    Some("port_in_use") => "监听端口已被占用",
                    Some("instance_exists") => "该端口已有实例，请换用端口或编辑现有实例",
                    Some("instance_missing") => "实例不存在，请刷新配置",
                    Some("unsupported_module") => "管理组件不支持所选模块，请升级组件",
                    Some("target_loop") => "转发目标不能指向自身同一监听端口",
                    Some("target_dns") => "Reality 目标未解析到公网地址，请检查域名与 VPS DNS",
                    Some("config_changed") => "配置或实例运行状态已变化，请在 VPS 核实",
                    Some("pm_port") => "请先启动本后台创建的业务实例，再添加 PM 规则",
                    Some("pm_dependency") => "仍有关联 PM 规则或无法确认端口策略，请在 VPS 核实",
                    Some("pm_nft") => "nftables 不可用或规则检查失败，请查看 VPS 日志",
                    Some("pm_conflict") => "检测到已存在的 PM 表或单元，请在 VPS 核实归属",
                    Some("pm_timer") => "PM 定时单元未正常启用，请查看 VPS 日志",
                    Some("pm_drift") => "PM 实际规则与确认状态不一致，请在 VPS 核实",
                    Some("revision_changed") => "配置版本已变化，请刷新后编辑",
                    Some("target_h2") => "Reality 目标未协商 HTTP/2",
                    Some("target_tls") => "Reality 目标 TLS 1.3 验证失败，请检查目标与网络",
                    Some("invalid_config") => "SB 核心配置校验失败",
                    Some("target_unreachable") => "目标 TCP 连接失败，请检查目标服务与网络",
                    _ => "",
                };
                let mut message =
                    if detail.is_empty() { message.to_owned() } else { format!("{message}：{detail}") };
                if (module.as_str(), action.as_str(), result.status.as_str()) == ("pm", "delete", "succeeded")
                {
                    if let Some(port) = open(&bytes, node, &payload)?["payload"]["port"].as_i64() {
                        traffic::forget(&tx, node, port)?;
                    }
                }
                if matches!(action.as_str(), "core_upgrade" | "core_rollback") {
                    let request = open(&bytes, node, &payload)?;
                    if let Some(version) = request["payload"]["version"].as_str().filter(|v| core_version(v))
                    {
                        message.push_str(&format!("（目标版本 {version}）"));
                    }
                }
                // A failed recovery is reported again on every poll until it is resolved.
                if matches!(result.status.as_str(), "rolled_back" | "recovery_failed") && reported != message
                {
                    let module = if module.is_empty() { "snell" } else { module.as_str() };
                    notes.push(format!("{module} {action}：{message}"));
                }
                tx.execute("UPDATE managed_operation SET status=?1,message=?2,payload=CASE WHEN ?1='delivered' THEN payload ELSE '' END WHERE id=?3",params![terminal,message,result.id])?;
            }
            let _ = (&result.message, &result.revision); // untrusted details are not logged or displayed
        }
        if let Some(progress) = &input.lifecycle_result {
            lifecycle::progress(&tx, &bytes, node, progress, &plain)?;
        }
        tx.execute(
            "UPDATE managed_node SET snapshot=?1,last_seen=?2,protocol=?3,component=?4 WHERE node=?5",
            params![snapshot, now(), PROTOCOL, component, node],
        )?;
        let lifecycle_ack = match &input.lifecycle_result {
            Some(progress) => lifecycle::acknowledgement(&tx, node, progress)?,
            None => Value::Null,
        };
        let lifecycle_active = lifecycle::active(&tx, node)?;
        tx.execute("UPDATE managed_operation SET status='cancelled',message='发送前已过期',payload='' WHERE node=?1 AND status='queued' AND expires<=?2",params![node,now()])?;
        let hub_maintenance: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM setting WHERE key='hub_lifecycle_maintenance' AND value='on')",
            [],
            |r| r.get(0),
        )?;
        let maintenance_active: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM managed_maintenance WHERE node=?1 AND expires>?2)",
            params![node, now()],
            |r| r.get(0),
        )?;
        let job:Option<(String,String,i64)>=tx.query_row("SELECT id,payload,expires FROM managed_operation WHERE node=?1 AND (status='delivered' OR (status='queued' AND ?2)) ORDER BY created LIMIT 1",params![node,is_enabled && !maintenance_active && !hub_maintenance && !lifecycle_active],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let mut task = Value::Null;
        if let Some((id, payload, expires)) = job.filter(|_| !lifecycle_active) {
            let request = open(&bytes, node, &payload)?;
            tx.execute("UPDATE managed_operation SET status='delivered' WHERE id=?1", [id])?;
            task = json!({"request":request,"expires":expires});
        }
        let lifecycle =
            lifecycle::delivery(&tx, &bytes, node, is_enabled && !maintenance_active && !hub_maintenance)?;
        tx.commit()?;
        // Naming the node reads the database, so the writer lock goes first.
        drop(c);
        notify::management(&app, node, notes, ip_alerts);
        Ok(json!({"task":task,"enabled":is_enabled,"lifecycle":lifecycle,"lifecycle_ack":lifecycle_ack}))
    })())
}
/// What became worth telling an administrator between two reports. The page reads
/// on demand, so nobody is looking when a quota runs out. Fixed wording, ports and
/// rule names only: a report carries client secrets beside these fields.
///
/// Usage is compared against `thresholds` (ascending percents ending at 100) in
/// both reports, so each step sounds once per billing period; the 100 % step is
/// the quota-reached message.
fn snapshot_events(old: &Value, new: &Value, thresholds: &[u64]) -> (Vec<String>, Vec<notify::IpAlert>) {
    let (mut events, mut ips) = (Vec::new(), Vec::new());
    if new["blocked"] == true && old["blocked"] != true {
        events.push("配置漂移或有未恢复事务，已暂停写入，请在 VPS 核实".to_owned());
    }
    let Some(usage) = new["modules"]["pm"]["usage"].as_object() else { return (events, ips) };
    for (port, state) in usage {
        let (rule, before) = (rule_of(new, port), &old["modules"]["pm"]["usage"][port.as_str()]);
        let alert = state["alert"].as_str().unwrap_or_default();
        let what = match alert {
            "expired" => Some("已到期，匹配流量已被阻断"),
            _ => None,
        };
        if let Some(what) = what.filter(|_| before["alert"].as_str() != Some(alert)) {
            events.push(format!("PM 端口 {port}：{what}"));
        }
        let step = threshold(state, &rule, thresholds);
        let was = if before["period_start"] == state["period_start"] {
            threshold(before, &rule_of(old, port), thresholds)
        } else {
            0
        };
        if step > was {
            events.push(if step >= 100 {
                format!("PM 端口 {port}：已达配额，匹配流量已被丢弃")
            } else {
                format!("PM 端口 {port}：用量已达配额 {step}%")
            });
        }
        let level = state["ip_level"].as_u64().unwrap_or(0);
        if level > 0 && state["ip_alert"] != before["ip_alert"] {
            ips.push(notify::IpAlert {
                port: port.clone(),
                name: rule["name"].as_str().unwrap_or_default().to_owned(),
                count: state["ip_count"].as_u64().unwrap_or(0),
                max: rule["ip_limit"]["max_ips"].as_u64().unwrap_or(0),
                block: rule["ip_limit"]["block"] == true,
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
/// Bound nodes whose component stopped polling though nothing explains it: not
/// one uninstalling, which stops its own agent, nor one under local maintenance.
pub fn silent_components(db: &Db, now: i64) -> Result<Vec<i64>> {
    let c = db.conn();
    let mut statement = c.prepare(&format!(
        "SELECT node FROM managed_node m WHERE credential!='' AND last_seen>0 AND last_seen<=?1
         AND NOT EXISTS(SELECT 1 FROM managed_lifecycle l WHERE l.node=m.node AND l.{})
         AND NOT EXISTS(SELECT 1 FROM managed_maintenance w WHERE w.node=m.node AND w.expires>?2)",
        lifecycle::ACTIVE
    ))?;
    let nodes = statement.query_map(params![now - 30, now], |r| r.get(0))?;
    Ok(nodes.collect::<std::result::Result<_, _>>()?)
}
/// Finished operations outlive their usefulness: the page lists the latest 30.
const OPERATION_DAYS: i64 = 90;
pub fn prune_operations(db: &Db, now: i64) -> Result<usize> {
    let c = db.conn();
    let operations = c.execute(
        "DELETE FROM managed_operation WHERE created<?1 AND status IN
         ('succeeded','rolled_back','rejected','expired','checked','cancelled')",
        [now - OPERATION_DAYS * 86_400],
    )?;
    Ok(operations + traffic::prune(&c, now)?)
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

const FILES: &[(&str, &str)] = &[
    ("install.py", include_str!("../../management/install.py")),
    ("agent.py", include_str!("../../management/agent.py")),
    ("lifecycle.py", include_str!("../../management/lifecycle.py")),
    ("controller.py", include_str!("../../management/controller.py")),
    ("model.py", include_str!("../../management/model.py")),
    ("storage.py", include_str!("../../management/storage.py")),
    ("snell.py", include_str!("../../management/snell.py")),
    ("modules_model.py", include_str!("../../management/modules_model.py")),
    ("services.py", include_str!("../../management/services.py")),
    ("pm.py", include_str!("../../management/pm.py")),
    ("menu.py", include_str!("../../management/menu.py")),
    ("core_assets.json", include_str!("../../management/core_assets.json")),
];
/// One value for a whole component release. `agent.py` computes the same from the
/// installed files, so a node reports what it actually runs.
fn component_digest(files: &[(&str, &str)]) -> String {
    let mut files = files.to_vec();
    files.sort_by_key(|(name, _)| *name);
    let lines: String = files.iter().map(|(name, body)| format!("{name} {}\n", sha256(body))).collect();
    sha256(&lines)
}
/// The one-line enrollment, `curl -fsS <site>/management-files/enroll.sh | sh -s -- <node> <code>`.
/// It does what the earlier long command did, with this hub's address and the
/// current `install.py` digest written in. All of it sits in `main`, called on the
/// last line, so a download cut short runs nothing. The code goes in as an argument,
/// never in the URL, so no proxy log records it; it lasts ten minutes and binds once.
const ENROLL: &str = r#"#!/bin/sh
# NodeHelm management enrollment, generated by the hub.
main() {
	set -eu
	if [ "$#" -ne 2 ]; then echo "用法：curl -fsS SITE/management-files/enroll.sh | sh -s -- <节点编号> <登记码>" >&2; exit 2; fi
	case "$1" in '' | *[!0-9]*) echo "节点编号无效" >&2; exit 2 ;; esac
	case "$2" in *[!0-9a-f]*) echo "登记码无效" >&2; exit 2 ;; esac
	[ "${#2}" -eq 64 ] || { echo "登记码无效" >&2; exit 2; }
	umask 077
	d=$(mktemp -d)
	trap 'rm -rf "$d"' EXIT
	curl --proto '=https' --tlsv1.2 -fsS 'SITE/management-files/install.py' -o "$d/install.py"
	printf '%s  %s\n' 'DIGEST' "$d/install.py" | sha256sum -c --quiet - || { echo "install.py 校验失败，已停止" >&2; exit 1; }
	printf '{"enrollment":"%s"}\n' "$2" | python3 "$d/install.py" --server 'SITE' --node "$1"
}
main "$@"
"#;

fn enroll_script(site: &str) -> Option<String> {
    // Written into single-quoted shell words, so nothing that could end one.
    let site = site.trim_end_matches('/');
    let safe = site.strip_prefix("https://").is_some_and(|rest| {
        !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_alphanumeric() || b".-:[]/".contains(&c))
    });
    let digest = FILES.iter().find(|(name, _)| *name == "install.py").map(|(_, body)| sha256(body))?;
    safe.then(|| ENROLL.replace("SITE", site).replace("DIGEST", &digest))
}

pub async fn files(State(app): State<Shared>, Path(name): Path<String>) -> Response {
    if name == "enroll.sh" {
        return match enroll_script(&app.site) {
            Some(script) => (
                [(header::CACHE_CONTROL, "no-store"), (header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                script,
            )
                .into_response(),
            None => (StatusCode::NOT_FOUND, "后台尚未配置 HTTPS 公网地址").into_response(),
        };
    }
    if name == "manifest.json" {
        let entries: serde_json::Map<String, Value> =
            FILES.iter().map(|(name, body)| (name.to_string(), json!(sha256(body)))).collect();
        return ([(header::CACHE_CONTROL, "no-store")], Json(entries)).into_response();
    }
    match FILES.iter().find(|(n, _)| *n == name) {
        Some((_, body)) => (
            [(header::CACHE_CONTROL, "no-store"), (header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            *body,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn enroll_script_carries_the_site_and_digest_and_checks_its_arguments() {
        let script = enroll_script("https://panel.example.com/").unwrap();
        let digest = sha256(FILES.iter().find(|(n, _)| *n == "install.py").unwrap().1);
        assert!(script.contains(&format!("'{digest}'")) && !script.contains("DIGEST"));
        assert!(script.contains("'https://panel.example.com/management-files/install.py'"));
        assert!(script.trim_end().ends_with("main \"$@\""), "a cut download runs nothing");
        assert!(!FILES.iter().any(|(n, _)| *n == "enroll.sh"), "not part of the component digest");
        for site in
            ["", "http://panel.example.com", "https://a'b.example.com", "https://a b.example.com", "https://"]
        {
            assert!(enroll_script(site).is_none(), "{site}");
        }
        let run = |args: &[&str]| {
            std::process::Command::new("sh").arg("-c").arg(&script).arg("sh").args(args).output().unwrap()
        };
        assert!(std::process::Command::new("sh").args(["-n", "-c", &script]).status().unwrap().success());
        let code = "a".repeat(64);
        for args in [&[][..], &["x", &code], &["2", "ABC"], &["2", &"a".repeat(63)], &["2", &code, "extra"]] {
            let out = run(args);
            assert_eq!(out.status.code(), Some(2), "{args:?}");
        }
    }
    #[test]
    fn old_operation_schema_migrates_without_inventing_history() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE managed_operation(id TEXT PRIMARY KEY,node INTEGER,request_hash TEXT,payload TEXT,status TEXT,message TEXT,created INTEGER,expires INTEGER); INSERT INTO managed_operation VALUES('old',1,'hash','','succeeded','kept',1,2);").unwrap();
        ensure(&c).unwrap();
        ensure(&c).unwrap();
        let row: (String, String, String) = c
            .query_row("SELECT module,action,message FROM managed_operation WHERE id='old'", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(row, ("".into(), "".into(), "kept".into()));
    }
    #[test]
    fn authenticated_ciphertext_cannot_move_between_nodes() {
        let key = [7; 32];
        let value = json!({"psk":"secret"});
        let encrypted = seal(&key, 1, &value).unwrap();
        assert!(!encrypted.contains("secret"));
        assert_eq!(open(&key, 1, &encrypted).unwrap(), value);
        assert!(open(&key, 2, &encrypted).is_err());
        assert!(open(&[8; 32], 1, &encrypted).is_err());
    }
    #[test]
    fn operation_rejects_unknown_actions_and_parameters() {
        let mut op = Operation {
            module: "snell".into(),
            id: "a".repeat(32),
            action: "install".into(),
            intent: None,
            revision: "b".repeat(64),
            payload: json!({}),
        };
        assert!(validate_request(&op).is_ok());
        op.payload = json!({"url":"https://evil.example"});
        assert!(validate_request(&op).is_err());
        op.action = "exec".into();
        assert!(validate_request(&op).is_err());
    }
    #[test]
    fn export_input_rejects_config_injection() {
        let r = Resource {
            name: "x\n[General]".into(),
            host: "example.com".into(),
            port: 12345,
            public_port: 12345,
            psk: "a".repeat(32),
            mode: "default".into(),
        };
        assert!(r.validate(true).is_err());
        let mut r = Resource { name: "demo".into(), ..r };
        for invalid in ["node..example.com", "example.com:443"] {
            r.host = invalid.into();
            assert!(r.validate(true).is_err());
        }
        r.host = "2001:db8::1".into();
        assert!(r.validate(true).is_ok());
    }
    #[test]
    fn hub_agrees_with_the_agent_on_every_operation() {
        // The same fixture is asserted against model.request in management/tests/test_transport.py.
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../management/tests/operations.json")).unwrap();
        for case in cases {
            let op = Operation {
                module: case["module"].as_str().unwrap().into(),
                id: "a".repeat(32),
                action: case["action"].as_str().unwrap().into(),
                intent: None,
                revision: "b".repeat(64),
                payload: case["payload"].clone(),
            };
            assert_eq!(validate_request(&op).is_ok(), case["valid"].as_bool().unwrap(), "{case}");
        }
    }

    #[test]
    fn restore_revokes_bindings_and_operations() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE node(id INTEGER PRIMARY KEY); INSERT INTO node VALUES(1);").unwrap();
        ensure(&c).unwrap();
        c.execute_batch(
            "ALTER TABLE managed_node ADD COLUMN subscription TEXT NOT NULL DEFAULT '';
             ALTER TABLE managed_node ADD COLUMN lifecycle_supported INTEGER NOT NULL DEFAULT 1;
             INSERT INTO managed_node(node,credential,subscription) VALUES(1,'secret','link');",
        )
        .unwrap();
        ensure(&c).unwrap();
        // Retired columns go away and the row survives without claiming a protocol.
        let protocol: i64 = c.query_row("SELECT protocol FROM managed_node", [], |r| r.get(0)).unwrap();
        assert_eq!(protocol, 0);
        assert!(c.prepare("SELECT subscription FROM managed_node").is_err());
        assert!(c.prepare("SELECT lifecycle_supported FROM managed_node").is_err());
        assert_eq!(
            c.query_row("SELECT credential FROM managed_node", [], |r| r.get::<_, String>(0)).unwrap(),
            "secret"
        );
        reset_after_restore(&c).unwrap();
        assert_eq!(c.query_row("SELECT count(*) FROM managed_node", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    struct Fixture {
        app: Shared,
        directory: std::path::PathBuf,
        node: i64,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!("node-manager-test-{}", random_token()));
            fs::create_dir(&directory).unwrap();
            let db = Db::open(directory.join("hub.db").to_str().unwrap()).unwrap();
            let node = db
                .create_node(
                    &crate::db::Node { name: "test".into(), ..Default::default() },
                    "monitor-only-token",
                )
                .unwrap();
            let mut app = crate::App::for_test(db);
            app.site = "https://panel.example.com".into();
            Self { app: std::sync::Arc::new(app), directory, node }
        }
        fn state(&self) -> State<Shared> {
            State(self.app.clone())
        }
        fn headers(&self, credential: &str) -> HeaderMap {
            let mut h = HeaderMap::new();
            h.insert(header::AUTHORIZATION, format!("Bearer {credential}").parse().unwrap());
            h
        }
        async fn bind(&self) -> String {
            assert_eq!(
                configure(Admin, self.state(), Json(Setting { enabled: true })).await.status(),
                StatusCode::OK
            );
            let v = body(enrollment(Admin, self.state(), Path(self.node)).await).await;
            let credential = "d".repeat(64);
            let input = || {
                Json(Enrollment {
                    node: self.node,
                    enrollment: v["enrollment"].as_str().unwrap().into(),
                    credential: credential.clone(),
                })
            };
            assert_eq!(enroll(self.state(), input()).await.status(), StatusCode::OK);
            assert_eq!(
                enroll(self.state(), input()).await.status(),
                StatusCode::OK,
                "registration retries must preserve identity"
            );
            let stolen = Json(Enrollment {
                node: self.node,
                enrollment: v["enrollment"].as_str().unwrap().into(),
                credential: "e".repeat(64),
            });
            assert_eq!(enroll(self.state(), stolen).await.status(), StatusCode::BAD_REQUEST);
            credential
        }
    }
    #[tokio::test]
    async fn enrollment_retries_after_expiry_only_for_the_accepted_identity() {
        let f = Fixture::new();
        body(configure(Admin, f.state(), Json(Setting { enabled: true })).await).await;
        let registration = body(enrollment(Admin, f.state(), Path(f.node)).await).await;
        let code = registration["enrollment"].as_str().unwrap();
        let credential = "c".repeat(64);
        let input = |code: &str, credential: &str| {
            Json(Enrollment { node: f.node, enrollment: code.into(), credential: credential.into() })
        };
        // A never-accepted registration still expires normally.
        f.app.db.conn().execute("UPDATE managed_node SET expires=0 WHERE node=?1", [f.node]).unwrap();
        assert_eq!(enroll(f.state(), input(code, &credential)).await.status(), StatusCode::BAD_REQUEST);
        f.app
            .db
            .conn()
            .execute("UPDATE managed_node SET expires=?1 WHERE node=?2", params![now() + 600, f.node])
            .unwrap();
        body(enroll(f.state(), input(code, &credential)).await).await;
        // Simulate a lost response / interrupted local binding write, then a long outage.
        f.app.db.conn().execute("UPDATE managed_node SET expires=0 WHERE node=?1", [f.node]).unwrap();
        body(enroll(f.state(), input(code, &credential)).await).await;
        assert_eq!(
            enroll(f.state(), input("wrong-code", &credential)).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(enroll(f.state(), input(code, &"d".repeat(64))).await.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            f.app
                .db
                .conn()
                .query_row("SELECT credential FROM managed_node WHERE node=?1", [f.node], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            sha256(&credential)
        );
        // Clearing authority (all-node cleanup/restore) and explicit revoke both prevent revival.
        f.app
            .db
            .conn()
            .execute("UPDATE managed_node SET credential='',enrollment='',expires=0 WHERE node=?1", [f.node])
            .unwrap();
        assert_eq!(enroll(f.state(), input(code, &credential)).await.status(), StatusCode::BAD_REQUEST);
        body(unbind(Admin, f.state(), Path(f.node)).await).await;
        assert_eq!(enroll(f.state(), input(code, &credential)).await.status(), StatusCode::BAD_REQUEST);
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
    async fn body(response: Response) -> Value {
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 262144).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&bytes));
        serde_json::from_slice(&bytes).unwrap()
    }
    fn snapshot() -> Snapshot {
        Snapshot {
            core: None,
            modules: Default::default(),
            _port_monitor: None,
            runtime: Default::default(),
            installed: false,
            resources: vec![],
            revision: "b".repeat(64),
            blocked: false,
        }
    }
    #[test]
    fn instance_configuration_is_scoped_and_has_no_wrapper() {
        let mut value = serde_json::to_value(snapshot()).unwrap();
        value["resources"] = json!([
            {"name":"one","port":12345,"host":"example.com","public_port":12345,"psk":"a".repeat(32),"mode":"default"},
            {"name":"two","port":23456,"host":"example.org","public_port":23456,"psk":"b".repeat(32),"mode":"default"}
        ]);
        let text = render_instance(value.clone(), "surge", 12345).unwrap();
        assert!(text.starts_with("one-12345 = snell,"));
        assert!(!text.contains("[Proxy]") && !text.contains("example.org"));
        assert!(render_instance(value.clone(), "surge", 9999).is_err());
        value["blocked"] = json!(true);
        assert!(render_instance(value, "surge", 12345).is_err());
    }
    #[test]
    fn sb_share_export_is_scoped_to_the_selected_instance() {
        let mut value = serde_json::to_value(snapshot()).unwrap();
        value["modules"] = json!({"sb": {
            "installed":true,"blocked":false,"revision":"c".repeat(64),"resources":[
                {"name":"one","port":12345,"host":"one.example.com","public_port":12345,
                 "protocol":"ss2022","sni":"","psk":"AQEBAQEBAQEBAQEBAQEBAQ=="},
                {"name":"two","port":23456,"host":"two.example.com","public_port":23456,
                 "protocol":"ss2022","sni":"","psk":"AgICAgICAgICAgICAgICAg=="}
            ]
        }});
        let text = render_instance(value.clone(), "share", 12345).unwrap();
        assert!(text.starts_with("ss://2022-blake3-aes-128-gcm:"));
        assert!(text.contains("@one.example.com:12345#one"));
        assert!(!text.contains("two.example.com") && !text.contains("AgICAg"));
        assert_eq!(text.lines().count(), 1);
        assert!(render_instance(value.clone(), "share", 9999).is_err());
        value["blocked"] = json!(true);
        assert!(render_instance(value, "share", 12345).is_err());
    }
    #[tokio::test]
    async fn maintenance_lease_is_owned_expiring_and_blocks_new_work() {
        let f = Fixture::new();
        let cred = f.bind().await;
        let request =
            |action: &str, token: &str| Json(Maintenance { action: action.into(), token: token.into() });
        let token = "b".repeat(64);
        let other = "c".repeat(64);
        let v = body(maintenance(f.state(), f.headers(&cred), request("begin", &token)).await).await;
        assert_eq!(v["owned"], true);
        assert_eq!(v["busy"], false);
        assert_ne!(unbind(Admin, f.state(), Path(f.node)).await.status(), StatusCode::OK);
        assert_eq!(
            maintenance(f.state(), f.headers(&cred), request("begin", &token)).await.status(),
            StatusCode::OK
        );
        assert_ne!(
            maintenance(f.state(), f.headers(&cred), request("begin", &other)).await.status(),
            StatusCode::OK
        );
        assert_ne!(
            maintenance(f.state(), f.headers(&cred), request("end", &other)).await.status(),
            StatusCode::OK
        );
        let v = body(
            poll(
                f.state(),
                f.headers(&cred),
                Json(Poll { snapshot: snapshot(), result: None, lifecycle_result: None }),
            )
            .await,
        )
        .await;
        assert!(v["task"].is_null());
        let op = Operation {
            module: "snell".into(),
            id: "f".repeat(32),
            action: "install".into(),
            intent: None,
            revision: snapshot().revision,
            payload: json!({}),
        };
        assert_ne!(submit(Admin, f.state(), Path(f.node), Json(op)).await.status(), StatusCode::OK);
        body(maintenance(f.state(), f.headers(&cred), request("end", &token)).await).await;
        body(maintenance(f.state(), f.headers(&cred), request("end", &token)).await).await;
        f.app.db.conn().execute("INSERT INTO managed_operation(id,node,request_hash,payload,status,created,expires) VALUES('pending',?1,'','','delivered',0,0)",[f.node]).unwrap();
        assert_ne!(
            maintenance(f.state(), f.headers(&cred), request("begin", &token)).await.status(),
            StatusCode::OK
        );
        f.app.db.conn().execute("DELETE FROM managed_operation", []).unwrap();
        body(maintenance(f.state(), f.headers(&cred), request("begin", &token)).await).await;
        f.app.db.conn().execute("UPDATE managed_maintenance SET expires=0", []).unwrap();
        assert_eq!(
            maintenance(f.state(), f.headers(&cred), request("begin", &other)).await.status(),
            StatusCode::OK
        );
    }
    #[tokio::test]
    async fn dead_node_with_unconfirmed_work_can_be_abandoned() {
        let f = Fixture::new();
        let credential = f.bind().await;
        let poll_once = || {
            poll(
                f.state(),
                f.headers(&credential),
                Json(Poll { snapshot: snapshot(), result: None, lifecycle_result: None }),
            )
        };
        body(poll_once().await).await;
        let op = Operation {
            module: "snell".into(),
            id: "a".repeat(32),
            action: "install".into(),
            intent: None,
            revision: "b".repeat(64),
            payload: json!({}),
        };
        body(submit(Admin, f.state(), Path(f.node), Json(op)).await).await;
        body(poll_once().await).await;
        // The VPS is gone with the task delivered: every other way out stays closed.
        assert_eq!(unbind(Admin, f.state(), Path(f.node)).await.status(), StatusCode::BAD_REQUEST);
        assert!(f.app.db.delete_node(f.node).is_err());
        let name = || Json(Abandon { name: "test".into() });
        // Still reporting recently: a brief outage cannot be mistaken for a dead node.
        assert_eq!(abandon(Admin, f.state(), Path(f.node), name()).await.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body(status(Admin, f.state(), Path(f.node)).await).await["abandonable"], false);
        f.app.db.conn().execute("UPDATE managed_node SET last_seen=?1", [now() - ABANDON_AFTER - 1]).unwrap();
        assert_eq!(body(status(Admin, f.state(), Path(f.node)).await).await["abandonable"], true);
        let wrong = Json(Abandon { name: "other".into() });
        assert_eq!(abandon(Admin, f.state(), Path(f.node), wrong).await.status(), StatusCode::BAD_REQUEST);
        body(abandon(Admin, f.state(), Path(f.node), name()).await).await;
        // Revocation is explicit, so a VPS that does return can unlink itself.
        let revoked = poll_once().await;
        assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(revoked.headers()["x-node-manager-binding"], "revoked");
        assert_eq!(body(status(Admin, f.state(), Path(f.node)).await).await["bound"], false);
        assert_eq!(abandon(Admin, f.state(), Path(f.node), name()).await.status(), StatusCode::BAD_REQUEST);
        body(enrollment(Admin, f.state(), Path(f.node)).await).await;
        assert!(f.app.db.delete_node(f.node).unwrap());
    }
    #[test]
    fn snapshot_events_report_transitions_once_and_no_secrets() {
        let steps = [50, 80, 100];
        let state = |blocked: bool, alert: &str| {
            json!({"blocked":blocked,"resources":[{"psk":"secret-value"}],
                "modules":{"pm":{"usage":{"12345":{"alert":alert}}}}})
        };
        let events = |old: &Value, new: &Value| snapshot_events(old, new, &steps).0;
        assert!(events(&state(false, "normal"), &state(false, "normal")).is_empty());
        assert!(events(&state(false, "normal"), &state(false, "expired"))[0].contains("到期"));
        let raised = events(&state(false, "normal"), &state(true, "expired"));
        assert_eq!(raised.len(), 2);
        assert!(raised[1].contains("12345") && !raised.concat().contains("secret-value"));
        assert!(events(&state(true, "expired"), &state(true, "expired")).is_empty());
    }
    #[test]
    fn quota_thresholds_sound_once_per_period_and_merge_with_the_quota() {
        let gib = 1u64 << 30;
        let state = |rx: u64, tx: u64, alert: &str, period: &str, mode: &str| {
            json!({"modules":{"pm":{
                "resources":[{"port":12345,"name":"PM","quota_gb":10,"quota_mode":mode,"psk":"secret-value"}],
                "usage":{"12345":{"rx":rx,"tx":tx,"alert":alert,"period_start":period}}}}})
        };
        let events = |old: &Value, new: &Value| snapshot_events(old, new, &[50, 80, 100]).0;
        let p = "2026-11-01";
        assert!(events(&state(0, 0, "normal", p, "in_out"), &state(2 * gib, 2 * gib, "normal", p, "in_out"))
            .is_empty());
        let half =
            events(&state(0, 0, "normal", p, "in_out"), &state(3 * gib, 2 * gib, "normal", p, "in_out"));
        assert_eq!(half, ["PM 端口 12345：用量已达配额 50%"]);
        // Inbound traffic does not count against an outbound-only quota.
        assert!(events(&state(0, 0, "normal", p, "out_only"), &state(9 * gib, 0, "normal", p, "out_only"))
            .is_empty());
        // A jump past several steps reports the highest, once.
        let jump = events(
            &state(0, 0, "normal", p, "in_out"),
            &state(5 * gib, 4 * gib, "quota_warning", p, "in_out"),
        );
        assert_eq!(jump, ["PM 端口 12345：用量已达配额 80%"]);
        let same = state(5 * gib, 4 * gib, "quota_warning", p, "in_out");
        assert!(events(&same, &same).is_empty());
        // The quota itself is one message whether the counters or the node report it first.
        let full = events(&same, &state(5 * gib, 4 * gib, "quota_reached", p, "in_out"));
        assert_eq!(full, ["PM 端口 12345：已达配额，匹配流量已被丢弃"]);
        let reached = state(5 * gib, 5 * gib, "quota_reached", p, "in_out");
        assert!(events(&reached, &reached).is_empty());
        // A new period starts counting again.
        let next = events(&reached, &state(3 * gib, 3 * gib, "normal", "2026-12-01", "in_out"));
        assert_eq!(next, ["PM 端口 12345：用量已达配额 50%"]);
        assert!(!jump.concat().contains("secret-value"));
    }
    #[test]
    fn ip_alerts_follow_the_component_sequence() {
        let state = |level: u64, seq: u64| {
            json!({"modules":{"pm":{
                "resources":[{"port":12345,"name":"PM","quota_gb":0,"quota_mode":"in_out",
                    "ip_limit":{"max_ips":3,"cooldown_min":30,"whitelist":[]}}],
                "usage":{"12345":{"rx":0,"tx":0,"alert":"normal","period_start":"",
                    "ip_count":4,"ip_level":level,"ip_alert":seq,"ip_list":["203.0.113.1"]}}}}})
        };
        let alerts = |old: &Value, new: &Value| snapshot_events(old, new, &[100]).1;
        assert!(alerts(&state(0, 0), &state(0, 0)).is_empty());
        let raised = alerts(&state(0, 0), &state(1, 1));
        assert_eq!((raised[0].port.as_str(), raised[0].max, raised[0].count), ("12345", 3, 4));
        assert!(alerts(&state(1, 1), &state(1, 1)).is_empty(), "the same alert is not repeated");
        assert_eq!(alerts(&state(1, 1), &state(2, 2))[0].level, 2);
        assert!(alerts(&state(1, 1), &state(0, 1)).is_empty(), "and a reset is silent");
    }
    #[tokio::test]
    async fn component_report_compares_with_the_embedded_release() {
        // The same fixture is asserted in management/tests/test_transport.py.
        assert_eq!(
            component_digest(&[("b.json", "y"), ("a.py", "x")]),
            "9729e99f400a67b08485e1f9a207d5df22e349dd671e85a8233b7ec9b1488101"
        );
        let f = Fixture::new();
        let credential = f.bind().await;
        let report = |digest: String| {
            let mut headers = f.headers(&credential);
            headers.insert("x-node-manager-component", digest.parse().unwrap());
            poll(
                f.state(),
                headers,
                Json(Poll { snapshot: snapshot(), result: None, lifecycle_result: None }),
            )
        };
        for (digest, current) in
            [(component_digest(FILES), true), ("0".repeat(64), false), ("not-a-digest".into(), false)]
        {
            body(report(digest).await).await;
            assert_eq!(
                body(status(Admin, f.state(), Path(f.node)).await).await["component_current"],
                current
            );
            let listed = body(settings(Admin, f.state()).await).await;
            assert_eq!(listed["bound"], 1);
            assert_eq!(listed["outdated"], if current { json!([]) } else { json!([f.node]) });
            assert_eq!(listed["offline"], json!([]), "the component just polled");
        }
        f.app.db.conn().execute("UPDATE managed_node SET last_seen=?1", [now() - 31]).unwrap();
        assert_eq!(body(settings(Admin, f.state()).await).await["offline"], json!([f.node]));
    }
    #[tokio::test]
    async fn only_unexplained_silence_and_old_finished_operations_are_acted_on() {
        let f = Fixture::new();
        f.bind().await;
        let c = || f.app.db.conn();
        c().execute("UPDATE managed_node SET last_seen=?1", [now()]).unwrap();
        assert!(silent_components(&f.app.db, now()).unwrap().is_empty());
        c().execute("UPDATE managed_node SET last_seen=?1", [now() - 60]).unwrap();
        assert_eq!(silent_components(&f.app.db, now()).unwrap(), vec![f.node]);
        c().execute("INSERT INTO managed_maintenance VALUES(?1,'lease',?2)", params![f.node, now() + 60])
            .unwrap();
        assert!(silent_components(&f.app.db, now()).unwrap().is_empty());
        let old = now() - (OPERATION_DAYS + 1) * 86_400;
        for (id, status, created) in
            [("done", "succeeded", old), ("recent", "succeeded", now()), ("pending", "delivered", old)]
        {
            c().execute(
                "INSERT INTO managed_operation(id,node,request_hash,payload,status,created,expires) VALUES(?1,?2,'','',?3,?4,0)",
                params![id, f.node, status, created],
            )
            .unwrap();
        }
        assert_eq!(prune_operations(&f.app.db, now()).unwrap(), 1);
        let left: i64 = c().query_row("SELECT count(*) FROM managed_operation", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 2);
    }
    #[tokio::test]
    async fn anonymous_and_monitor_tokens_cannot_manage() {
        let f = Fixture::new();
        let response = admin_routes()
            .with_state(f.app.clone())
            .oneshot(Request::builder().uri("/api/management").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(identity(&f.app.db, &f.headers("monitor-only-token-xxxxxxxxxxxxxxxx")).is_err());
    }
    #[tokio::test]
    async fn revoked_identity_has_an_explicit_signal_without_claiming_cleanup_success() {
        let f = Fixture::new();
        let credential = f.bind().await;
        let input = || Json(Maintenance { action: "status".into(), token: "0".repeat(64) });
        let bound = maintenance(f.state(), f.headers(&credential), input()).await;
        assert_eq!(bound.status(), StatusCode::OK);
        assert!(!bound.headers().contains_key("x-node-manager-binding"));
        let invalid = maintenance(
            f.state(),
            f.headers(&credential),
            Json(Maintenance { action: "invalid".into(), token: "0".repeat(64) }),
        )
        .await;
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        assert!(!invalid.headers().contains_key("x-node-manager-binding"));
        body(unbind(Admin, f.state(), Path(f.node)).await).await;
        let revoked = maintenance(f.state(), f.headers(&credential), input()).await;
        assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(revoked.headers()["x-node-manager-binding"], "revoked");
        assert_eq!(revoked.headers()[header::CACHE_CONTROL], "no-store");
        let data = to_bytes(revoked.into_body(), 4096).await.unwrap();
        assert!(!String::from_utf8_lossy(&data).contains("成功"));
        // Storage errors cannot be mistaken for a verified revocation.
        f.app.db.conn().execute_batch("DROP TABLE managed_node;").unwrap();
        let failed = maintenance(f.state(), f.headers(&credential), input()).await;
        assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!failed.headers().contains_key("x-node-manager-binding"));
    }

    #[tokio::test]
    async fn binding_delivery_receipt_export_and_revocation() {
        let f = Fixture::new();
        let credential = f.bind().await;
        let headers = || f.headers(&credential);
        assert!(f.app.db.delete_node(f.node).is_err(), "bound nodes cannot disappear through the old API");
        body(
            poll(
                f.state(),
                headers(),
                Json(Poll { snapshot: snapshot(), result: None, lifecycle_result: None }),
            )
            .await,
        )
        .await;
        let op = || Operation {
            module: "snell".into(),
            id: "a".repeat(32),
            action: "install".into(),
            intent: None,
            revision: "b".repeat(64),
            payload: json!({}),
        };
        body(submit(Admin, f.state(), Path(f.node), Json(op())).await).await;
        body(submit(Admin, f.state(), Path(f.node), Json(op())).await).await;
        let task = body(
            poll(
                f.state(),
                headers(),
                Json(Poll { snapshot: snapshot(), result: None, lifecycle_result: None }),
            )
            .await,
        )
        .await;
        assert_eq!(task["task"]["request"]["id"], "a".repeat(32));
        assert_eq!(unbind(Admin, f.state(), Path(f.node)).await.status(), StatusCode::BAD_REQUEST);
        let mut applied = snapshot();
        applied.installed = true;
        applied.revision = "c".repeat(64);
        applied.resources.push(Resource {
            name: "demo".into(),
            host: "example.com".into(),
            port: 12345,
            public_port: 12345,
            psk: "s".repeat(32),
            mode: "default".into(),
        });
        body(
            poll(
                f.state(),
                headers(),
                Json(Poll {
                    snapshot: applied,
                    lifecycle_result: None,
                    result: Some(Receipt {
                        code: None,
                        id: "a".repeat(32),
                        status: "succeeded".into(),
                        message: "untrusted secret output".into(),
                        revision: "c".repeat(64),
                    }),
                }),
            )
            .await,
        )
        .await;
        let state = body(status(Admin, f.state(), Path(f.node)).await).await;
        assert!(state["snapshot"]["resources"][0].get("psk").is_none());
        assert!(!state.to_string().contains("untrusted"));
        let cipher: String =
            f.app.db.conn().query_row("SELECT snapshot FROM managed_node", [], |r| r.get(0)).unwrap();
        assert!(!cipher.contains(&"s".repeat(32)));
        let response =
            export_configuration(Admin, f.state(), Path(f.node), Query(ExportFormat::default())).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let text = String::from_utf8(to_bytes(response.into_body(), 65536).await.unwrap().to_vec()).unwrap();
        assert!(text.contains("version=6"));
        assert!(text.contains(&"s".repeat(32)));
        body(unbind(Admin, f.state(), Path(f.node)).await).await;
        assert!(identity(&f.app.db, &headers()).is_err());
        assert!(f.app.db.delete_node(f.node).unwrap());
    }
    #[tokio::test]
    async fn pm_traffic_follows_reports_and_ends_with_the_rule() {
        let f = Fixture::new();
        let credential = f.bind().await;
        let report = |rx: u64, tx: u64| -> Snapshot {
            // `port_monitor` as components up to 1.1.5 still send it.
            serde_json::from_value(json!({
                "core":null,"port_monitor":{"read_only":true,"status":"not_installed","rows":[]},
                "runtime":{},"installed":false,"resources":[],
                "revision":"b".repeat(64),"blocked":false,
                "modules":{"pm":{"installed":true,"blocked":false,"revision":"a".repeat(64),
                    "resources":[{"port":12345,"name":"PM","quota_gb":0,"quota_mode":"in_out","reset_day":0,
                        "egress_mbps":0,"expiry_date":""}],
                    "usage":{"12345":{"rx":rx,"tx":tx,"sampled_at":1,"period_start":"","alert":"normal"}}}}
            }))
            .unwrap()
        };
        let send = |snapshot: Snapshot, result: Option<Receipt>| {
            poll(f.state(), f.headers(&credential), Json(Poll { snapshot, result, lifecycle_result: None }))
        };
        let read = || async {
            body(port_traffic(Admin, f.state(), Path(f.node), Query(TrafficPort { port: 12345 })).await).await
        };
        body(send(report(100, 10), None).await).await;
        body(send(report(150, 30), None).await).await;
        assert_eq!(read().await["hours"][0].as_array().unwrap()[1..], [json!(50), json!(20)]);
        let op = Operation {
            module: "pm".into(),
            id: "a".repeat(32),
            action: "delete".into(),
            intent: None,
            revision: "a".repeat(64),
            payload: json!({"port":12345}),
        };
        body(submit(Admin, f.state(), Path(f.node), Json(op)).await).await;
        let task = body(send(report(150, 30), None).await).await;
        assert_eq!(task["task"]["request"]["id"], "a".repeat(32));
        let receipt = Receipt {
            code: None,
            id: "a".repeat(32),
            status: "succeeded".into(),
            message: String::new(),
            revision: "c".repeat(64),
        };
        body(send(report(150, 30), Some(receipt)).await).await;
        assert_eq!(read().await["hours"], json!([]));
    }
    #[tokio::test]
    async fn missing_encryption_key_never_regenerates_over_secrets() {
        let f = Fixture::new();
        let credential = f.bind().await;
        body(
            poll(
                f.state(),
                f.headers(&credential),
                Json(Poll { snapshot: snapshot(), result: None, lifecycle_result: None }),
            )
            .await,
        )
        .await;
        fs::remove_file(format!("{}.management.key", f.app.db.file())).unwrap();
        assert!(key(&f.app.db).is_err());
    }
    #[tokio::test]
    async fn another_protocol_reports_presence_but_exchanges_no_work() {
        let f = Fixture::new();
        let credential = f.bind().await;
        let snap = || {
            let mut s = snapshot();
            s.installed = true;
            s
        };
        let exchange = |protocol: Option<i64>| {
            let mut headers = f.headers(&credential);
            if let Some(protocol) = protocol {
                headers.insert("x-node-manager-protocol", protocol.to_string().parse().unwrap());
            }
            let input = json!({"snapshot":serde_json::to_value(snap()).unwrap()}).to_string();
            poll_route(f.state(), headers, axum::body::Bytes::from(input))
        };
        body(exchange(Some(PROTOCOL)).await).await;
        let op = |id: &str| Operation {
            module: "snell".into(),
            id: id.repeat(32),
            action: "apply".into(),
            intent: None,
            revision: snap().revision,
            payload: json!({"name":"Demo","host":"example.com","port":12345,"public_port":12345,"mode":"default"}),
        };
        body(submit(Admin, f.state(), Path(f.node), Json(op("a"))).await).await;

        // Neither an older component nor one without the header can claim the queued job.
        for protocol in [None, Some(PROTOCOL + 1)] {
            let refused = exchange(protocol).await;
            assert_eq!(refused.status(), StatusCode::CONFLICT);
        }
        let state = body(status(Admin, f.state(), Path(f.node)).await).await;
        assert_eq!(state["compatible"], false);
        assert_eq!(state["operations"][0]["status"], "queued");
        let blocked = submit(Admin, f.state(), Path(f.node), Json(op("b"))).await;
        assert_eq!(blocked.status(), StatusCode::BAD_REQUEST);

        let upgraded = body(exchange(Some(PROTOCOL)).await).await;
        assert_eq!(upgraded["task"]["request"]["id"], "a".repeat(32));
        let state = body(status(Admin, f.state(), Path(f.node)).await).await;
        assert_eq!(state["compatible"], true);
    }
    #[tokio::test]
    async fn disabling_does_not_hide_results_but_rejects_new_writes() {
        let f = Fixture::new();
        let credential = f.bind().await;
        body(configure(Admin, f.state(), Json(Setting { enabled: false })).await).await;
        let p = body(
            poll(
                f.state(),
                f.headers(&credential),
                Json(Poll { snapshot: snapshot(), result: None, lifecycle_result: None }),
            )
            .await,
        )
        .await;
        assert_eq!(p["enabled"], false);
        assert_eq!(
            submit(
                Admin,
                f.state(),
                Path(f.node),
                Json(Operation {
                    module: "snell".into(),
                    id: "a".repeat(32),
                    action: "install".into(),
                    intent: None,
                    revision: "b".repeat(64),
                    payload: json!({})
                })
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
}
