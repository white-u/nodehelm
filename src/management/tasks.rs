//! Status, operations submitted from the panel, the component poll and its
//! receipts, snapshot events and housekeeping.

use super::*;

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
pub(super) enum ApplyIntent {
    Create,
    Update,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    #[serde(default = "models::default_module", skip_serializing_if = "models::is_snell")]
    pub(super) module: String,
    pub(super) id: String,
    pub(super) action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) intent: Option<ApplyIntent>,
    pub(super) revision: String,
    pub(super) payload: Value,
}
fn core_lifecycle(action: &str) -> bool {
    matches!(action, "core_upgrade" | "core_rollback" | "core_uninstall")
}

pub(super) fn validate_request(op: &Operation) -> Result<()> {
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
pub(super) struct Resource {
    pub(super) name: String,
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) public_port: u16,
    #[serde(default)]
    pub(super) psk: String,
    pub(super) mode: String,
}
impl Resource {
    pub(super) fn validate(&self, secret: bool) -> Result<()> {
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
pub(super) struct Snapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) core: Option<CoreSnapshot>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(super) modules: std::collections::BTreeMap<String, models::ModuleSnapshot>,
    /// Sent by components up to 1.1.5 and kept in their stored snapshots; ignored.
    /// Kept so those reports still parse and can receive the component upgrade.
    #[serde(default, skip_serializing, rename = "port_monitor")]
    pub(super) _port_monitor: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    pub(super) runtime: std::collections::BTreeMap<String, String>,
    pub(super) installed: bool,
    pub(super) resources: Vec<Resource>,
    pub(super) revision: String,
    pub(super) blocked: bool,
}
impl Snapshot {
    pub(super) fn validate(&self) -> Result<()> {
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
    pub(super) snapshot: Snapshot,
    pub(super) result: Option<Receipt>,
    pub(super) lifecycle_result: Option<lifecycle::Progress>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    #[serde(default)]
    pub(super) code: Option<String>,
    pub(super) id: String,
    pub(super) status: String,
    pub(super) message: String,
    #[serde(default)]
    pub(super) revision: String,
}
/// A short, credential-bound lease prevents new work during explicit local maintenance.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Maintenance {
    pub(super) action: String,
    pub(super) token: String,
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
pub(super) fn snapshot_events(
    old: &Value,
    new: &Value,
    thresholds: &[u64],
) -> (Vec<String>, Vec<notify::IpAlert>) {
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
pub(super) const OPERATION_DAYS: i64 = 90;
pub fn prune_operations(db: &Db, now: i64) -> Result<usize> {
    let c = db.conn();
    let operations = c.execute(
        "DELETE FROM managed_operation WHERE created<?1 AND status IN
         ('succeeded','rolled_back','rejected','expired','checked','cancelled')",
        [now - OPERATION_DAYS * 86_400],
    )?;
    Ok(operations + traffic::prune(&c, now)?)
}
