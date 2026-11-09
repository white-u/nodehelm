//! Enrolment and binding, the one-line `enroll.sh` and the component files it
//! installs.

use super::*;

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
    pub(super) enrollment: String,
    pub(super) credential: String,
    /// The kernel boot id of the machine running the command.
    pub(super) boot_id: String,
}
/// The kernel boot id the node's monitor agent reports now, if it is connected.
/// The agent sends it as `<boot id>/<interface digest>`. A disconnected agent's
/// last report is not used: the machine may since have been reinstalled.
fn live_boot_id(app: &crate::App, node: i64) -> Option<String> {
    let agents = app.agents.read().unwrap_or_else(|e| e.into_inner());
    let reported = agents.get(&node)?.metrics["boot_id"].as_str()?;
    Some(reported.split('/').next()?.to_owned()).filter(|id| !id.is_empty())
}
pub async fn enroll(State(app): State<Shared>, Json(input): Json<Enrollment>) -> Response {
    response((|| {
        enabled(&app.db)?;
        if !valid_hex(&input.credential, 64) || input.enrollment.len() > 128 || input.boot_id.len() > 64 {
            refuse!("登记凭据格式错误");
        }
        let c = app.db.conn();
        let code = sha256(&input.enrollment);
        let cred = sha256(&input.credential);
        // The code names the node: a node number typed alongside it could only be wrong.
        let node: i64 = c
            .query_row("SELECT node FROM managed_node WHERE enrollment=?1", [&code], |r| r.get(0))
            .optional()?
            .context(crate::Shown("登记已过期、已使用或节点不存在，请重新生成安装命令".into()))?;
        lifecycle::guard(&c, node)?;
        let name: String = c
            .query_row("SELECT name FROM node WHERE id=?1", [node], |r| r.get(0))
            .optional()?
            .unwrap_or_default();
        // A command copied to the wrong machine: refused before anything binds.
        // An anti-mistake check, not a security control; skipped while the node's
        // monitor agent is not connected.
        if live_boot_id(&app, node).is_some_and(|id| id != input.boot_id.trim()) {
            refuse!("此命令属于节点「{name}」，但本机不是该节点监控 Agent 所在的机器。请到本机对应的节点复制命令，或在「{name}」上执行");
        }
        // Credentials alone identify a node, so one already held elsewhere never binds a second.
        let changed = c.execute(
            "UPDATE managed_node SET credential=?1 WHERE node=?2 AND enrollment=?3
             AND ((credential='' AND expires>?4) OR credential=?1)
             AND NOT EXISTS(SELECT 1 FROM managed_node WHERE credential=?1 AND node!=?2)",
            params![cred, node, code, now()],
        )?;
        if changed != 1 {
            refuse!("登记已过期、已使用或节点不存在，请重新生成安装命令");
        }
        Ok(json!({"bound":true,"node":node,"name":name}))
    })())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identify {
    pub(super) node: i64,
    pub(super) nonce: String,
}
/// Lets a bound node check that an address it is moving to reaches this hub before
/// it sends its credential there. The proof is keyed by the credential's digest,
/// which only this hub and the node hold, and covers this hub's own address, so an
/// address that relays the question here is told apart from the hub itself.
pub async fn identify(State(app): State<Shared>, Json(input): Json<Identify>) -> Response {
    response((|| {
        enabled(&app.db)?;
        if !valid_hex(&input.nonce, 64) {
            refuse!("请求格式错误");
        }
        let digest: String = app
            .db
            .conn()
            .query_row(
                "SELECT credential FROM managed_node WHERE node=?1 AND credential!=''",
                [input.node],
                |r| r.get(0),
            )
            .optional()?
            .context(BindingRevoked)?;
        Ok(json!({"proof": identity_proof(&digest, input.node, &input.nonce, &app.site)}))
    })())
}
pub(super) fn identity_proof(digest: &str, node: i64, nonce: &str, site: &str) -> String {
    let site = site.trim_end_matches('/').to_ascii_lowercase();
    let key = hmac::Key::new(hmac::HMAC_SHA256, digest.as_bytes());
    hex::encode(hmac::sign(&key, format!("nodehelm-identify\n{node}\n{nonce}\n{site}").as_bytes()))
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
pub(super) const ABANDON_AFTER: i64 = 600;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Abandon {
    pub(super) name: String,
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

pub(super) const FILES: &[(&str, &str)] = &[
    ("install.py", include_str!("../../management/install.py")),
    ("agent.py", include_str!("../../management/agent.py")),
    ("lifecycle.py", include_str!("../../management/lifecycle.py")),
    ("controller.py", include_str!("../../management/controller.py")),
    ("protocol.py", include_str!("../../management/protocol.py")),
    ("storage.py", include_str!("../../management/storage.py")),
    ("modules.py", include_str!("../../management/modules.py")),
    ("core_driver.py", include_str!("../../management/core_driver.py")),
    ("snell.py", include_str!("../../management/snell.py")),
    ("sb.py", include_str!("../../management/sb.py")),
    ("fw.py", include_str!("../../management/fw.py")),
    ("pm.py", include_str!("../../management/pm.py")),
    ("menu.py", include_str!("../../management/menu.py")),
    ("core_assets.json", include_str!("../../management/core_assets.json")),
];
/// One value for a whole component release. `agent.py` computes the same from the
/// installed files, so a node reports what it actually runs.
pub(super) fn component_digest(files: &[(&str, &str)]) -> String {
    let mut files = files.to_vec();
    files.sort_by_key(|(name, _)| *name);
    let lines: String = files.iter().map(|(name, body)| format!("{name} {}\n", sha256(body))).collect();
    sha256(&lines)
}
/// The one-line enrollment, `curl -fsS <site>/management-files/enroll.sh | sh -s -- <code>`.
/// It does what the earlier long command did, with this hub's address and the
/// current `install.py` digest written in. All of it sits in `main`, called on the
/// last line, so a download cut short runs nothing. The code goes in as an argument,
/// never in the URL, so no proxy log records it; it lasts ten minutes and binds once.
const ENROLL: &str = r#"#!/bin/sh
# NodeHelm management enrollment, generated by the hub.
main() {
	set -eu
	if [ "$#" -ne 1 ]; then echo "用法：curl -fsS SITE/management-files/enroll.sh | sh -s -- <登记码>" >&2; exit 2; fi
	case "$1" in *[!0-9a-f]*) echo "登记码无效" >&2; exit 2 ;; esac
	[ "${#1}" -eq 64 ] || { echo "登记码无效" >&2; exit 2; }
	umask 077
	d=$(mktemp -d)
	trap 'rm -rf "$d"' EXIT
	curl --proto '=https' --tlsv1.2 -fsS 'SITE/management-files/install.py' -o "$d/install.py"
	printf '%s  %s\n' 'DIGEST' "$d/install.py" | sha256sum -c --quiet - || { echo "install.py 校验失败，已停止" >&2; exit 1; }
	printf '{"enrollment":"%s"}\n' "$1" | python3 "$d/install.py" --server 'SITE'
}
main "$@"
"#;

pub(super) fn enroll_script(site: &str) -> Option<String> {
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
