//! Node-scoped uninstall orchestration. The node owns steps and their durable receipts.
use super::*;
use std::collections::BTreeMap;

pub(super) const ACTIVE: &str = "status IN ('running','paused','cleaning','finalizing')";
const CLEANUP_RETRY_SECONDS: i64 = 7 * 24 * 60 * 60;
const MODULES: [&str; 4] = ["snell", "sb", "fw", "pm"];

pub(super) fn ensure(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS managed_lifecycle (
          id TEXT PRIMARY KEY, node INTEGER NOT NULL, scope TEXT NOT NULL,
          revisions TEXT NOT NULL, request_hash TEXT NOT NULL, status TEXT NOT NULL,
          stage TEXT NOT NULL, message TEXT NOT NULL, attempt INTEGER NOT NULL,
          created INTEGER NOT NULL, updated INTEGER NOT NULL,
          cleanup_hash TEXT NOT NULL DEFAULT '', cleanup_cipher TEXT NOT NULL DEFAULT '',
          cleanup_expires INTEGER NOT NULL DEFAULT 0);
         CREATE UNIQUE INDEX IF NOT EXISTS managed_lifecycle_active
           ON managed_lifecycle(node) WHERE status IN ('running','paused','cleaning','finalizing');",
    )?;
    c.execute(
        "UPDATE managed_lifecycle SET cleanup_hash='',cleanup_cipher=''
               WHERE status='succeeded' AND cleanup_expires<=?1 AND cleanup_cipher!=''",
        [now()],
    )?;
    Ok(())
}

pub(super) fn reset_after_restore(c: &Connection) -> Result<()> {
    c.execute(
        "UPDATE managed_lifecycle SET status=CASE WHEN status='succeeded' THEN status ELSE 'revoked' END,
         cleanup_hash='',cleanup_cipher='',cleanup_expires=0,
         message=CASE WHEN status='succeeded' THEN message ELSE '后台恢复已撤销旧授权，请核实节点清理进度' END",
        [],
    )?;
    Ok(())
}

/// Ends an uninstall nobody can finish. The record stays, and clearing the hash
/// fences a cleanup helper that turns out to be alive.
pub(super) fn abandon(c: &Connection, node: i64) -> Result<()> {
    c.execute(
        &format!(
            "UPDATE managed_lifecycle SET status='revoked',cleanup_hash='',cleanup_cipher='',cleanup_expires=0,
             message='管理员确认节点已失效，已放弃卸载并撤销授权；VPS 实际状态未核实',updated=?2
             WHERE node=?1 AND {ACTIVE}"
        ),
        params![node, now()],
    )?;
    Ok(())
}

pub(super) fn active(c: &Connection, node: i64) -> Result<bool> {
    Ok(c.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM managed_lifecycle WHERE node=?1 AND {ACTIVE})"),
        [node],
        |r| r.get(0),
    )?)
}

pub(super) fn guard(c: &Connection, node: i64) -> Result<()> {
    if active(c, node)? {
        refuse!("此节点正在卸载或等待确认，请先完成卸载恢复");
    }
    Ok(())
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({"id":r.get::<_,String>(0)?, "scope":r.get::<_,String>(1)?,
        "status":r.get::<_,String>(2)?, "stage":r.get::<_,String>(3)?,
        "message":r.get::<_,String>(4)?, "attempt":r.get::<_,i64>(5)?,
        "created":r.get::<_,i64>(6)?, "updated":r.get::<_,i64>(7)?}))
}

const SELECT_JOB: &str =
    "SELECT id,scope,status,stage,message,attempt,created,updated FROM managed_lifecycle";

pub(super) fn latest(c: &Connection, node: i64) -> Result<Value> {
    let mut value = c
        .query_row(
            &format!("{SELECT_JOB} WHERE node=?1 ORDER BY created DESC,rowid DESC LIMIT 1"),
            [node],
            row,
        )
        .optional()?
        .unwrap_or(Value::Null);
    if matches!(value["status"].as_str(), Some("running" | "paused")) {
        let seen: Option<i64> = c
            .query_row("SELECT last_seen FROM managed_node WHERE node=?1", [node], |r| r.get(0))
            .optional()?;
        if seen.is_none_or(|time| time <= now() - 30) {
            value["underlying_status"] = value["status"].clone();
            value["status"] = json!("interrupted");
            value["message"] = json!("节点离线，卸载进度已保留，请恢复连接后继续");
        }
    }
    Ok(value)
}

fn job(c: &Connection, id: &str) -> Result<Value> {
    Ok(c.query_row(&format!("{SELECT_JOB} WHERE id=?1"), [id], row).optional()?.unwrap_or(Value::Null))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Start {
    id: String,
    scope: String,
    revisions: BTreeMap<String, String>,
    #[serde(default)]
    resume: bool,
    attempt: Option<i64>,
}

fn validate_start(input: &Start) -> Result<()> {
    if !valid_hex(&input.id, 32)
        || !matches!(input.scope.as_str(), "snell" | "sb" | "fw" | "pm" | "all")
        || input.revisions.len() != MODULES.len()
        || MODULES.iter().any(|module| input.revisions.get(*module).is_none_or(|r| !valid_hex(r, 64)))
        || (input.resume && input.attempt.is_none_or(|attempt| attempt < 1))
        || (!input.resume && input.attempt.is_some())
    {
        refuse!("卸载范围、操作编号或配置版本无效");
    }
    Ok(())
}

fn enabled_for_start(c: &Connection) -> Result<()> {
    let maintenance: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM setting WHERE key='hub_lifecycle_maintenance' AND value='on')",
        [],
        |r| r.get(0),
    )?;
    let enabled: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM setting WHERE key='management_enabled' AND value='on')",
        [],
        |r| r.get(0),
    )?;
    if maintenance {
        refuse!("后台正在维护，请稍后重试");
    }
    if !enabled {
        refuse!("节点管理未启用");
    }
    Ok(())
}

fn ready(
    c: &Connection,
    bytes: &[u8; 32],
    node: i64,
    revisions: &BTreeMap<String, String>,
    recovering: bool,
) -> Result<Value> {
    let row: Option<(i64, String, i64)> = c
        .query_row(
            "SELECT last_seen,snapshot,protocol FROM managed_node WHERE node=?1 AND credential!=''",
            [node],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((seen, cipher, protocol)) = row else {
        refuse!("请先接入此节点的管理组件");
    };
    if protocol != PROTOCOL && seen > now() - 30 {
        refuse!("管理组件与后台版本不一致，请先升级组件");
    }
    if seen <= now() - 30 || cipher.is_empty() {
        refuse!("节点管理组件离线，禁止启动或继续卸载");
    }
    let state = open(bytes, node, &cipher)?;
    if !recovering && state["blocked"] == true {
        refuse!("节点有未恢复操作，请先核实");
    }
    for module in MODULES {
        let selected = module_state(&state, module);
        if selected.is_null()
            || (!recovering && selected["blocked"] == true)
            || selected["revision"] != revisions[module]
        {
            refuse!("节点模块状态或配置版本已变化，请刷新后继续");
        }
    }
    let busy: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM managed_operation WHERE node=?1
          AND (status='delivered' OR (status='queued' AND expires>?2)))
          OR EXISTS(SELECT 1 FROM managed_maintenance WHERE node=?1 AND expires>?2)",
        params![node, now()],
        |r| r.get(0),
    )?;
    if busy {
        refuse!("此节点仍有管理操作或组件维护待确认");
    }
    Ok(state)
}

pub(super) async fn start(
    _: Admin,
    State(app): State<Shared>,
    Path(node): Path<i64>,
    headers: HeaderMap,
    Json(input): Json<Start>,
) -> Response {
    let Some(caller) = crate::auth::current_session(&headers) else {
        return crate::api::answer(StatusCode::UNAUTHORIZED, "登录已失效，请重新登录");
    };
    let result = (|| {
        validate_start(&input)?;
        let bytes = key(&app.db)?;
        let revisions = serde_json::to_string(&input.revisions)?;
        let request_hash = sha256(&format!("{}:{revisions}", input.scope));
        let mut c = app.db.conn();
        let tx = c.transaction()?;
        let authorized: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM session WHERE token_hash=?1 AND expires_at>?2)",
            params![caller, now()],
            |r| r.get(0),
        )?;
        if !authorized {
            return Ok(None);
        }
        let previous: Option<(i64,String,String,String,i64,String,bool)> = tx.query_row(
            "SELECT node,scope,request_hash,status,attempt,stage,cleanup_cipher!='' FROM managed_lifecycle WHERE id=?1", [&input.id],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional()?;
        if let Some((owner, scope, old_hash, status, attempt, stage, has_cleanup)) = previous {
            if owner != node || scope != input.scope || (!input.resume && old_hash != request_hash) {
                refuse!("卸载操作编号已用于不同节点或请求");
            }
            if input.resume {
                let expected = input.attempt.unwrap();
                if expected > attempt {
                    refuse!("卸载尝试编号无效，请刷新");
                }
                if expected == attempt && status == "paused" {
                    enabled_for_start(&tx)?;
                    let state = ready(&tx, &bytes, node, &input.revisions, true)?;
                    let status = if scope == "all" && stage == "cleanup" && has_cleanup {
                        if state["blocked"] == true || MODULES.iter().any(|m| !empty_module(&state, m)) {
                            refuse!("受管模块仍有待恢复状态，请先核实再继续组件清理");
                        }
                        "cleaning"
                    } else {
                        "running"
                    };
                    tx.execute(
                        "UPDATE managed_lifecycle SET status=?1,attempt=attempt+1,revisions=?2,
                        message='正在继续此节点的卸载',updated=?3 WHERE id=?4",
                        params![status, revisions, now(), input.id],
                    )?;
                }
            }
            let value = job(&tx, &input.id)?;
            tx.commit()?;
            return Ok(Some(json!({"lifecycle":value})));
        }
        if input.resume {
            refuse!("没有可继续的卸载操作");
        }
        enabled_for_start(&tx)?;
        guard(&tx, node)?;
        ready(&tx, &bytes, node, &input.revisions, false)?;
        tx.execute("INSERT INTO managed_lifecycle(id,node,scope,revisions,request_hash,status,stage,message,attempt,created,updated)
            VALUES(?1,?2,?3,?4,?5,'running','preflight','正在核实此节点的卸载范围',1,?6,?6)",
            params![input.id,node,input.scope,revisions,request_hash,now()])?;
        let value = job(&tx, &input.id)?;
        tx.commit()?;
        Ok(Some(json!({"lifecycle":value})))
    })();
    match result {
        Ok(Some(value)) => response(Ok(value)),
        Ok(None) => crate::api::answer(StatusCode::UNAUTHORIZED, "登录已失效，请重新登录"),
        Err(error) => response(Err(error)),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Progress {
    id: String,
    attempt: i64,
    status: String,
    stage: String,
    message: Option<String>,
}

fn empty_module(state: &Value, module: &str) -> bool {
    let selected = module_state(state, module);
    selected["installed"] == false
        && selected["blocked"] == false
        && selected["resources"].as_array().is_some_and(Vec::is_empty)
}

pub(super) fn progress(
    c: &Connection,
    bytes: &[u8; 32],
    node: i64,
    input: &Progress,
    state: &Value,
) -> Result<()> {
    if !valid_hex(&input.id, 32)
        || input.attempt < 1
        || !matches!(input.status.as_str(), "running" | "paused" | "succeeded" | "cleaning")
        || !matches!(
            input.stage.as_str(),
            "preflight" | "pm" | "instances" | "cores" | "cleanup" | "finished"
        )
        || input.message.as_deref().is_some_and(|code| {
            !matches!(
                code,
                "lifecycle_preflight" | "lifecycle_step" | "lifecycle_recovery" | "lifecycle_cleanup"
            )
        })
    {
        refuse!("节点卸载进度格式无效");
    }
    let old: Option<(String, String, i64)> = c
        .query_row(
            "SELECT scope,status,attempt FROM managed_lifecycle WHERE id=?1 AND node=?2",
            params![input.id, node],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((scope, status, attempt)) = old else {
        refuse!("卸载进度不属于此节点");
    };
    if input.attempt > attempt {
        refuse!("卸载进度的尝试编号无效");
    }
    if input.attempt < attempt || !matches!(status.as_str(), "running" | "paused" | "cleaning") {
        return Ok(());
    }
    if status == "paused" {
        return Ok(());
    }
    if status == "cleaning" && !matches!(input.status.as_str(), "cleaning" | "paused") {
        refuse!("节点已进入组件清理，请使用收尾执行器确认");
    }
    let message = match input.status.as_str() {
        "paused" => match input.message.as_deref() {
            Some("lifecycle_recovery") => "卸载已暂停，请先在节点恢复未完成操作",
            Some("lifecycle_cleanup") => "组件清理已暂停，请检查节点收尾记录后继续",
            _ => "卸载已暂停，请检查节点状态后继续",
        },
        "succeeded" => {
            if scope == "all" || !empty_module(state, &scope) {
                refuse!("模块卸载尚未得到空状态确认");
            }
            "已确认此节点所选模块卸载完成"
        }
        "cleaning" => {
            if scope != "all" || state["blocked"] == true || MODULES.iter().any(|m| !empty_module(state, m)) {
                refuse!("全部受管模块尚未清空，禁止进入组件清理");
            }
            let has_token: bool = c.query_row(
                "SELECT cleanup_hash!='' FROM managed_lifecycle WHERE id=?1",
                [&input.id],
                |r| r.get(0),
            )?;
            if !has_token {
                let restricted = random_token();
                c.execute(
                    "UPDATE managed_lifecycle SET cleanup_hash=?1,cleanup_cipher=?2 WHERE id=?3",
                    params![sha256(&restricted), seal(bytes, node, &json!(restricted))?, input.id],
                )?;
            }
            "业务模块已清空，等待确认管理组件实际移除"
        }
        _ => "正在卸载此节点的受管资源",
    };
    let stage = match input.status.as_str() {
        "succeeded" => "finished",
        "cleaning" => "cleanup",
        _ => &input.stage,
    };
    c.execute(
        "UPDATE managed_lifecycle SET status=?1,stage=?2,message=?3,updated=?4 WHERE id=?5",
        params![input.status, stage, message, now(), input.id],
    )?;
    Ok(())
}

/// A local terminal step is safe to discard only after this transaction has
/// persisted the same terminal result for this node and attempt.
pub(super) fn acknowledgement(c: &Connection, node: i64, input: &Progress) -> Result<Value> {
    if input.status != "succeeded" {
        return Ok(Value::Null);
    }
    let confirmed: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM managed_lifecycle WHERE id=?1 AND node=?2
         AND attempt=?3 AND scope!='all' AND status='succeeded')",
        params![input.id, node, input.attempt],
        |r| r.get(0),
    )?;
    Ok(if confirmed { json!({"id":input.id,"attempt":input.attempt}) } else { Value::Null })
}

pub(super) fn delivery(c: &Connection, bytes: &[u8; 32], node: i64, enabled: bool) -> Result<Value> {
    let row: Option<(String, String, i64, String, String)> = c
        .query_row(
            "SELECT id,scope,attempt,status,cleanup_cipher FROM managed_lifecycle
         WHERE node=?1 AND status IN ('running','cleaning')",
            [node],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((id, scope, attempt, status, cipher)) = row else {
        return Ok(Value::Null);
    };
    if status == "running" && !enabled {
        return Ok(Value::Null);
    }
    let mut value = json!({"id":id,"scope":scope,"attempt":attempt});
    if status == "cleaning" {
        value["cleanup_token"] = open(bytes, node, &cipher)?;
    }
    Ok(value)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Finish {
    id: String,
    phase: String,
}

pub(super) async fn finish(
    State(app): State<Shared>,
    headers: HeaderMap,
    Json(input): Json<Finish>,
) -> Response {
    response((|| {
        if !valid_hex(&input.id, 32) || !matches!(input.phase.as_str(), "cleaned" | "finished") {
            refuse!("卸载收尾请求无效");
        }
        let digest = sha256(token(&headers)?);
        let mut c = app.db.conn();
        let tx = c.transaction()?;
        let row: Option<(i64, String, i64)> = tx
            .query_row(
                "SELECT node,status,cleanup_expires FROM managed_lifecycle WHERE id=?1 AND scope='all'
             AND cleanup_hash=?2 AND cleanup_hash!=''",
                params![input.id, digest],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        // An explicit answer, so a returning cleanup helper can end instead of retrying forever.
        let Some((node, status, expires)) = row else {
            return Err(BindingRevoked.into());
        };
        if status == "succeeded" && expires <= now() {
            return Err(BindingRevoked.into());
        }
        if status == "succeeded" {
            return Ok(json!({"id":input.id,"status":"succeeded"}));
        }
        match (input.phase.as_str(), status.as_str()) {
            ("cleaned", "cleaning") => {
                tx.execute(
                    "UPDATE managed_node SET credential='',enrollment='',expires=0,protocol=0 WHERE node=?1",
                    [node],
                )?;
                tx.execute(
                    "UPDATE managed_lifecycle SET status='finalizing',stage='cleanup',
                    message='管理组件清理已确认，等待最终回执',updated=?1 WHERE id=?2",
                    params![now(), input.id],
                )?;
            }
            ("cleaned", "finalizing") => {}
            ("finished", "finalizing") => {
                tx.execute(
                    "UPDATE managed_lifecycle SET status='succeeded',stage='finished',
                    message='已确认此节点的受管业务和管理组件全部卸载',updated=?1,cleanup_expires=?2
                    WHERE id=?3",
                    params![now(), now() + CLEANUP_RETRY_SECONDS, input.id],
                )?;
            }
            _ => refuse!("卸载尚未到达此收尾阶段"),
        }
        let status: String =
            tx.query_row("SELECT status FROM managed_lifecycle WHERE id=?1", [&input.id], |r| r.get(0))?;
        tx.commit()?;
        Ok(json!({"id":input.id,"status":status}))
    })())
}

#[cfg(test)]
mod tests {
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
        credential: String,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!("node-lifecycle-test-{}", random_token()));
            fs::create_dir(&directory).unwrap();
            let db = Db::open(directory.join("hub.db").to_str().unwrap()).unwrap();
            let node = db
                .create_node(
                    &crate::db::Node { name: "managed".into(), ..Default::default() },
                    "monitor-only-token",
                )
                .unwrap();
            db.set("management_enabled", "on").unwrap();
            db.create_session(&sha256("lifecycle-admin"), now() + 3600).unwrap();
            let app = std::sync::Arc::new(crate::App::for_test(db));
            let f = Self { app, directory, node, credential: random_token() };
            f.bind(node, &f.credential);
            f
        }
        fn state(&self) -> State<Shared> {
            State(self.app.clone())
        }
        fn admin_headers(&self) -> HeaderMap {
            HeaderMap::from_iter([(header::COOKIE, "monitor_session=lifecycle-admin".parse().unwrap())])
        }
        fn snapshot(&self) -> Value {
            let module = json!({"installed":false,"resources":[],"revision":"b".repeat(64),"blocked":false});
            json!({"installed":false,"resources":[],"revision":"b".repeat(64),"blocked":false,
                "modules":{"sb":module,"fw":module,"pm":module}})
        }
        fn bind(&self, node: i64, credential: &str) {
            let cipher = seal(&key(&self.app.db).unwrap(), node, &self.snapshot()).unwrap();
            self.app
                .db
                .conn()
                .execute(
                    "INSERT INTO managed_node(node,credential,last_seen,snapshot,protocol)
                VALUES(?1,?2,?3,?4,?5)",
                    params![node, sha256(credential), now(), cipher, PROTOCOL],
                )
                .unwrap();
        }
        fn request(&self, id: &str, scope: &str, resume: bool, attempt: Option<i64>) -> Start {
            Start {
                id: id.repeat(32),
                scope: scope.into(),
                revisions: MODULES.into_iter().map(|module| (module.into(), "b".repeat(64))).collect(),
                resume,
                attempt,
            }
        }
        fn headers(&self, credential: &str) -> HeaderMap {
            let mut h = HeaderMap::new();
            h.insert(header::AUTHORIZATION, format!("Bearer {credential}").parse().unwrap());
            h
        }
        async fn start(&self, id: &str, scope: &str) -> Response {
            start(
                Admin,
                self.state(),
                Path(self.node),
                self.admin_headers(),
                Json(self.request(id, scope, false, None)),
            )
            .await
        }
        async fn poll(&self, state: Value, result: Value) -> Response {
            let input = serde_json::from_value(json!({"snapshot":state,"result":null,
                "lifecycle_result":result}))
            .unwrap();
            super::super::poll(self.state(), self.headers(&self.credential), Json(input)).await
        }
        fn progress(&self, id: &str, attempt: i64, status: &str, stage: &str) -> Value {
            json!({"id":id.repeat(32),"attempt":attempt,"status":status,"stage":stage})
        }
        async fn finish(&self, id: &str, token: &str, phase: &str) -> Response {
            finish(self.state(), self.headers(token), Json(Finish { id: id.repeat(32), phase: phase.into() }))
                .await
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
    async fn value(response: Response) -> Value {
        let status = response.status();
        let data = to_bytes(response.into_body(), 262144).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&data));
        serde_json::from_slice(&data).unwrap()
    }

    #[tokio::test]
    async fn start_requires_admin_online_capability_and_current_revisions() {
        let f = Fixture::new();
        let response=admin_routes().with_state(f.app.clone()).oneshot(Request::builder()
            .method("POST").uri(format!("/api/management/{}/lifecycle",f.node))
            .header(header::CONTENT_TYPE,"application/json").body(Body::from(json!({
                "id":"a".repeat(32),"scope":"all","revisions":f.request("a","all",false,None).revisions
            }).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        f.app.db.conn().execute("UPDATE managed_node SET protocol=0", []).unwrap();
        assert_eq!(f.start("a", "fw").await.status(), StatusCode::BAD_REQUEST);
        f.app
            .db
            .conn()
            .execute("UPDATE managed_node SET protocol=?1,last_seen=?2", [PROTOCOL, now() - 31])
            .unwrap();
        assert_eq!(f.start("a", "fw").await.status(), StatusCode::BAD_REQUEST);
        f.app.db.conn().execute("UPDATE managed_node SET last_seen=?1", [now()]).unwrap();
        let mut stale = f.request("a", "fw", false, None);
        stale.revisions.insert("pm".into(), "c".repeat(64));
        assert_eq!(
            start(Admin, f.state(), Path(f.node), f.admin_headers(), Json(stale)).await.status(),
            StatusCode::BAD_REQUEST
        );
        let mut blocked = f.snapshot();
        blocked["blocked"] = json!(true);
        value(f.poll(blocked, Value::Null).await).await;
        assert_eq!(f.start("a", "fw").await.status(), StatusCode::BAD_REQUEST);
        value(f.poll(f.snapshot(), Value::Null).await).await;
        f.app.db.conn().execute("DELETE FROM session", []).unwrap();
        assert_eq!(f.start("a", "fw").await.status(), StatusCode::UNAUTHORIZED);
        assert!(latest(&f.app.db.conn(), f.node).unwrap().is_null());
        f.app.db.create_session(&sha256("lifecycle-admin"), now() + 3600).unwrap();
        assert_eq!(value(f.start("a", "fw").await).await["lifecycle"]["attempt"], 1);
    }

    #[tokio::test]
    async fn duplicate_node_isolation_and_active_job_exclusion() {
        let f = Fixture::new();
        let other = f
            .app
            .db
            .create_node(&crate::db::Node { name: "other".into(), ..Default::default() }, "other-monitor")
            .unwrap();
        let other_credential = random_token();
        f.bind(other, &other_credential);
        let first = value(f.start("a", "snell").await).await;
        assert_eq!(value(f.start("a", "snell").await).await, first);
        assert_eq!(
            start(
                Admin,
                f.state(),
                Path(other),
                f.admin_headers(),
                Json(f.request("a", "snell", false, None))
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(f.start("b", "fw").await.status(), StatusCode::BAD_REQUEST);
        assert_eq!(unbind(Admin, f.state(), Path(f.node)).await.status(), StatusCode::BAD_REQUEST);
        assert_eq!(enrollment(Admin, f.state(), Path(f.node)).await.status(), StatusCode::BAD_REQUEST);
        let lease = Maintenance { action: "begin".into(), token: "e".repeat(64) };
        assert_eq!(
            maintenance(f.state(), f.headers(&f.credential), Json(lease)).await.status(),
            StatusCode::BAD_REQUEST
        );
        let op = |id: &str| Operation {
            module: "snell".into(),
            id: id.repeat(32),
            action: "install".into(),
            intent: None,
            revision: "b".repeat(64),
            payload: json!({}),
            then: None,
        };
        assert_eq!(
            submit(Admin, f.state(), Path(f.node), Json(op("b"))).await.status(),
            StatusCode::BAD_REQUEST
        );
        value(submit(Admin, f.state(), Path(other), Json(op("c"))).await).await;
        let denied = Progress {
            id: "a".repeat(32),
            attempt: 1,
            status: "paused".into(),
            stage: "preflight".into(),
            message: None,
        };
        let bytes = key(&f.app.db).unwrap();
        assert!(progress(&f.app.db.conn(), &bytes, other, &denied, &f.snapshot()).is_err());
        let encrypted_snapshot: String = f
            .app
            .db
            .conn()
            .query_row("SELECT snapshot FROM managed_node WHERE node=?1", [other], |r| r.get(0))
            .unwrap();
        assert_eq!(open(&bytes, other, &encrypted_snapshot).unwrap(), f.snapshot());
    }

    #[tokio::test]
    async fn pause_resume_uses_attempt_fence_and_old_receipts_do_not_advance() {
        let f = Fixture::new();
        value(f.start("a", "fw").await).await;
        value(f.poll(f.snapshot(), f.progress("a", 1, "paused", "instances")).await).await;
        assert!(value(f.poll(f.snapshot(), Value::Null).await).await["lifecycle"].is_null());
        let mut recovering = f.snapshot();
        recovering["blocked"] = json!(true);
        recovering["modules"]["fw"]["blocked"] = json!(true);
        value(f.poll(recovering, Value::Null).await).await;
        let resume = || f.request("a", "fw", true, Some(1));
        assert_eq!(
            value(start(Admin, f.state(), Path(f.node), f.admin_headers(), Json(resume())).await).await
                ["lifecycle"]["attempt"],
            2
        );
        assert_eq!(
            value(start(Admin, f.state(), Path(f.node), f.admin_headers(), Json(resume())).await).await
                ["lifecycle"]["attempt"],
            2
        );
        value(f.poll(f.snapshot(), f.progress("a", 2, "paused", "instances")).await).await;
        let old = value(start(Admin, f.state(), Path(f.node), f.admin_headers(), Json(resume())).await).await;
        assert_eq!(old["lifecycle"]["status"], "paused");
        assert_eq!(old["lifecycle"]["attempt"], 2);
        let resumed = value(
            start(
                Admin,
                f.state(),
                Path(f.node),
                f.admin_headers(),
                Json(f.request("a", "fw", true, Some(2))),
            )
            .await,
        )
        .await;
        assert_eq!(resumed["lifecycle"]["attempt"], 3);
        let poll = value(f.poll(f.snapshot(), f.progress("a", 1, "succeeded", "finished")).await).await;
        assert_eq!(poll["lifecycle"]["attempt"], 3);
        assert_eq!(latest(&f.app.db.conn(), f.node).unwrap()["status"], "running");
    }

    #[tokio::test]
    async fn module_success_requires_real_empty_snapshot_and_audit_survives_unbind() {
        let f = Fixture::new();
        let mut installed = f.snapshot();
        installed["modules"]["fw"]["installed"] = json!(true);
        value(f.poll(installed.clone(), Value::Null).await).await;
        value(f.start("a", "fw").await).await;
        assert_eq!(
            f.poll(installed, f.progress("a", 1, "succeeded", "finished")).await.status(),
            StatusCode::BAD_REQUEST
        );
        value(f.poll(f.snapshot(), f.progress("a", 1, "succeeded", "finished")).await).await;
        value(unbind(Admin, f.state(), Path(f.node)).await).await;
        let audit = value(status(Admin, f.state(), Path(f.node)).await).await;
        assert_eq!(audit["bound"], false);
        assert_eq!(audit["lifecycle"]["status"], "succeeded");
    }

    #[tokio::test]
    async fn module_receipt_is_acknowledged_only_after_durable_matching_completion() {
        let f = Fixture::new();
        value(f.start("a", "fw").await).await;
        let running = value(f.poll(f.snapshot(), f.progress("a", 1, "running", "preflight")).await).await;
        assert!(running["lifecycle_ack"].is_null());
        let done = value(f.poll(f.snapshot(), f.progress("a", 1, "succeeded", "finished")).await).await;
        assert_eq!(done["lifecycle_ack"], json!({"id":"a".repeat(32),"attempt":1}));
        assert_eq!(job(&f.app.db.conn(), &"a".repeat(32)).unwrap()["status"], "succeeded");
        let retried = value(f.poll(f.snapshot(), f.progress("a", 1, "succeeded", "finished")).await).await;
        assert_eq!(retried["lifecycle_ack"], done["lifecycle_ack"]);
        let input = |attempt| Progress {
            id: "a".repeat(32),
            attempt,
            status: "succeeded".into(),
            stage: "finished".into(),
            message: None,
        };
        assert!(acknowledgement(&f.app.db.conn(), f.node + 1, &input(1)).unwrap().is_null());
        assert!(acknowledgement(&f.app.db.conn(), f.node, &input(2)).unwrap().is_null());
        value(f.start("b", "pm").await).await;
        let paused = value(f.poll(f.snapshot(), f.progress("b", 1, "paused", "preflight")).await).await;
        assert!(paused["lifecycle_ack"].is_null());
        // A late local success while the Hub still requires resume is not an acknowledgement.
        let stale = value(f.poll(f.snapshot(), f.progress("b", 1, "succeeded", "finished")).await).await;
        assert!(stale["lifecycle_ack"].is_null());
    }

    #[tokio::test]
    async fn all_cleanup_uses_restricted_two_phase_idempotent_confirmation() {
        let f = Fixture::new();
        value(f.start("a", "all").await).await;
        assert_eq!(
            f.poll(f.snapshot(), f.progress("a", 1, "succeeded", "finished")).await.status(),
            StatusCode::BAD_REQUEST
        );
        let mut installed = f.snapshot();
        installed["modules"]["sb"]["installed"] = json!(true);
        assert_eq!(
            f.poll(installed, f.progress("a", 1, "cleaning", "cleanup")).await.status(),
            StatusCode::BAD_REQUEST
        );
        let reply = value(f.poll(f.snapshot(), f.progress("a", 1, "cleaning", "cleanup")).await).await;
        let restricted = reply["lifecycle"]["cleanup_token"].as_str().unwrap();
        assert!(valid_hex(restricted, 64));
        let cipher: String = f
            .app
            .db
            .conn()
            .query_row("SELECT cleanup_cipher FROM managed_lifecycle", [], |r| r.get(0))
            .unwrap();
        assert!(!cipher.contains(restricted));
        assert_eq!(f.finish("a", &f.credential, "cleaned").await.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(f.finish("a", restricted, "finished").await.status(), StatusCode::BAD_REQUEST);
        assert_eq!(f.finish("b", restricted, "cleaned").await.status(), StatusCode::UNAUTHORIZED);
        value(f.poll(f.snapshot(), f.progress("a", 1, "paused", "cleanup")).await).await;
        let resumed = value(
            start(
                Admin,
                f.state(),
                Path(f.node),
                f.admin_headers(),
                Json(f.request("a", "all", true, Some(1))),
            )
            .await,
        )
        .await;
        assert_eq!(resumed["lifecycle"]["status"], "cleaning");
        assert_eq!(resumed["lifecycle"]["attempt"], 2);
        // An active cleanup remains recoverable after a long offline interval.
        f.app.db.conn().execute("UPDATE managed_lifecycle SET cleanup_expires=1", []).unwrap();
        assert_eq!(value(f.finish("a", restricted, "cleaned").await).await["status"], "finalizing");
        assert_eq!(value(f.finish("a", restricted, "cleaned").await).await["status"], "finalizing");
        let revoked = f.poll(f.snapshot(), Value::Null).await;
        assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(revoked.headers()["x-node-manager-binding"], "revoked");
        let audit = value(status(Admin, f.state(), Path(f.node)).await).await;
        assert_eq!(audit["bound"], false);
        assert_eq!(audit["lifecycle"]["status"], "finalizing");
        assert!(!audit.to_string().contains(restricted));
        assert_eq!(enrollment(Admin, f.state(), Path(f.node)).await.status(), StatusCode::BAD_REQUEST);
        assert_eq!(value(f.finish("a", restricted, "finished").await).await["status"], "succeeded");
        assert_eq!(value(f.finish("a", restricted, "finished").await).await["status"], "succeeded");
        assert_eq!(value(f.finish("a", restricted, "cleaned").await).await["status"], "succeeded");
        f.app.db.conn().execute("UPDATE managed_lifecycle SET cleanup_expires=1", []).unwrap();
        assert_eq!(f.finish("a", restricted, "finished").await.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn abandoning_ends_the_job_fences_cleanup_and_keeps_the_record() {
        let f = Fixture::new();
        value(f.start("a", "all").await).await;
        let reply = value(f.poll(f.snapshot(), f.progress("a", 1, "cleaning", "cleanup")).await).await;
        let restricted = reply["lifecycle"]["cleanup_token"].as_str().unwrap();
        // Any uninstall also blocks hub restore; the job must stop counting as active.
        assert!(active(&f.app.db.conn(), f.node).unwrap());
        f.app.db.conn().execute("UPDATE managed_node SET last_seen=?1", [now() - 3_600]).unwrap();
        let name = Json(crate::management::Abandon { name: "managed".into() });
        value(crate::management::abandon(Admin, f.state(), Path(f.node), name).await).await;
        assert!(!active(&f.app.db.conn(), f.node).unwrap());
        assert_eq!(latest(&f.app.db.conn(), f.node).unwrap()["status"], "revoked");
        let late = f.finish("a", restricted, "cleaned").await;
        assert_eq!(late.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(late.headers()["x-node-manager-binding"], "revoked");
        assert!(f.app.db.delete_node(f.node).unwrap());
        let kept: i64 =
            f.app.db.conn().query_row("SELECT count(*) FROM managed_lifecycle", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 1);
    }

    #[tokio::test]
    async fn protocol_mismatch_keeps_job_and_restore_revokes_cleanup_authority() {
        let f = Fixture::new();
        value(f.start("a", "all").await).await;
        let body = axum::body::Bytes::from(json!({"snapshot":f.snapshot()}).to_string());
        let refused = super::super::poll_route(f.state(), f.headers(&f.credential), body.clone()).await;
        assert_eq!(refused.status(), StatusCode::CONFLICT);
        assert_eq!(latest(&f.app.db.conn(), f.node).unwrap()["status"], "running");
        let mut matching = f.headers(&f.credential);
        matching.insert("x-node-manager-protocol", PROTOCOL.to_string().parse().unwrap());
        let reply = value(super::super::poll_route(f.state(), matching, body).await).await;
        assert_eq!(reply["lifecycle"]["id"], "a".repeat(32));
        let reply = value(f.poll(f.snapshot(), f.progress("a", 1, "cleaning", "cleanup")).await).await;
        let restricted = reply["lifecycle"]["cleanup_token"].as_str().unwrap();
        reset_after_restore(&f.app.db.conn()).unwrap();
        assert_eq!(f.finish("a", restricted, "cleaned").await.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(latest(&f.app.db.conn(), f.node).unwrap()["status"], "revoked");
        value(f.poll(f.snapshot(), f.progress("a", 1, "running", "preflight")).await).await;
        assert_eq!(latest(&f.app.db.conn(), f.node).unwrap()["status"], "revoked");
    }
}
