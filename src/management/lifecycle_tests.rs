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
            .create_node(&crate::db::Node { name: "test".into(), ..Default::default() }, "monitor-only-token")
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
                enrollment: v["enrollment"].as_str().unwrap().into(),
                credential: credential.clone(),
                boot_id: String::new(),
            })
        };
        assert_eq!(enroll(self.state(), input()).await.status(), StatusCode::OK);
        assert_eq!(
            enroll(self.state(), input()).await.status(),
            StatusCode::OK,
            "registration retries must preserve identity"
        );
        let stolen = Json(Enrollment {
            enrollment: v["enrollment"].as_str().unwrap().into(),
            credential: "e".repeat(64),
            boot_id: String::new(),
        });
        assert_eq!(enroll(self.state(), stolen).await.status(), StatusCode::BAD_REQUEST);
        credential
    }
}
#[tokio::test]
async fn enrollment_is_refused_on_a_machine_other_than_the_monitored_one() {
    let f = Fixture::new();
    body(configure(Admin, f.state(), Json(Setting { enabled: true })).await).await;
    let code = body(enrollment(Admin, f.state(), Path(f.node)).await).await["enrollment"]
        .as_str()
        .unwrap()
        .to_owned();
    let here = "11111111-2222-3333-4444-555555555555";
    let input = |boot: &str| {
        Json(Enrollment { enrollment: code.clone(), credential: "c".repeat(64), boot_id: boot.into() })
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let mut agent = crate::agent_ws::Agent::new(1, tx);
    agent.metrics = json!({"boot_id": format!("{here}/00000000000000ff")});
    f.app.agents.write().unwrap().insert(f.node, agent);
    // Same hostname or not, another machine has another boot id.
    let refused = enroll(f.state(), input("99999999-2222-3333-4444-555555555555")).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let text = String::from_utf8(to_bytes(refused.into_body(), 4096).await.unwrap().to_vec()).unwrap();
    assert!(text.contains("此命令属于节点「test」"), "{text}");
    let bound: String = f
        .app
        .db
        .conn()
        .query_row("SELECT credential FROM managed_node WHERE node=?1", [f.node], |r| r.get(0))
        .unwrap();
    assert!(bound.is_empty(), "nothing binds on a refusal");
    let accepted = body(enroll(f.state(), input(&format!("{here}\n"))).await).await;
    assert_eq!(accepted["name"], "test");
    // A disconnected agent's last report is not evidence: the check is skipped.
    f.app.agents.write().unwrap().remove(&f.node);
    assert_eq!(enroll(f.state(), input("another-machine")).await.status(), StatusCode::OK);
}
#[tokio::test]
async fn identity_proof_covers_this_hub_and_only_a_bound_node() {
    let f = Fixture::new();
    let nonce = "a".repeat(64);
    let ask = |node: i64, nonce: &str| identify(f.state(), Json(Identify { node, nonce: nonce.into() }));
    body(configure(Admin, f.state(), Json(Setting { enabled: true })).await).await;
    let unbound = ask(f.node, &nonce).await;
    assert_eq!(unbound.headers()["x-node-manager-binding"], "revoked");
    let credential = f.bind().await;
    let proof = body(ask(f.node, &nonce).await).await["proof"].as_str().unwrap().to_owned();
    assert_eq!(proof, identity_proof(&sha256(&credential), f.node, &nonce, "https://PANEL.example.com/"));
    assert_ne!(proof, identity_proof(&sha256(&credential), f.node, &nonce, "https://relay.example.com"));
    assert_ne!(proof, identity_proof(&sha256(&"e".repeat(64)), f.node, &nonce, "https://panel.example.com"));
    assert_eq!(ask(f.node, "short").await.status(), StatusCode::BAD_REQUEST);
}
#[tokio::test]
async fn a_credential_bound_to_one_node_never_binds_another() {
    let f = Fixture::new();
    let credential = f.bind().await;
    let other = f
        .app
        .db
        .create_node(&crate::db::Node { name: "other".into(), ..Default::default() }, "other-token")
        .unwrap();
    let code = body(enrollment(Admin, f.state(), Path(other)).await).await["enrollment"]
        .as_str()
        .unwrap()
        .to_owned();
    let input = |credential: &str| {
        Json(Enrollment { enrollment: code.clone(), credential: credential.into(), boot_id: String::new() })
    };
    assert_eq!(enroll(f.state(), input(&credential)).await.status(), StatusCode::BAD_REQUEST);
    let accepted = body(enroll(f.state(), input(&"f".repeat(64))).await).await;
    assert_eq!(accepted["node"], other, "the code alone names the node");
}
#[tokio::test]
async fn enrollment_retries_after_expiry_only_for_the_accepted_identity() {
    let f = Fixture::new();
    body(configure(Admin, f.state(), Json(Setting { enabled: true })).await).await;
    let registration = body(enrollment(Admin, f.state(), Path(f.node)).await).await;
    let code = registration["enrollment"].as_str().unwrap();
    let credential = "c".repeat(64);
    let input = |code: &str, credential: &str| {
        Json(Enrollment { enrollment: code.into(), credential: credential.into(), boot_id: String::new() })
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
    assert_eq!(enroll(f.state(), input("wrong-code", &credential)).await.status(), StatusCode::BAD_REQUEST);
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
        then: None,
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
        then: None,
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
    let half = events(&state(0, 0, "normal", p, "in_out"), &state(3 * gib, 2 * gib, "normal", p, "in_out"));
    assert_eq!(half, ["端口 12345：用量已达配额 50%"]);
    // Inbound traffic does not count against an outbound-only quota.
    assert!(
        events(&state(0, 0, "normal", p, "out_only"), &state(9 * gib, 0, "normal", p, "out_only")).is_empty()
    );
    // A jump past several steps reports the highest, once.
    let jump =
        events(&state(0, 0, "normal", p, "in_out"), &state(5 * gib, 4 * gib, "quota_warning", p, "in_out"));
    assert_eq!(jump, ["端口 12345：用量已达配额 80%"]);
    let same = state(5 * gib, 4 * gib, "quota_warning", p, "in_out");
    assert!(events(&same, &same).is_empty());
    // The quota itself is one message whether the counters or the node report it first.
    let full = events(&same, &state(5 * gib, 4 * gib, "quota_reached", p, "in_out"));
    assert_eq!(full, ["端口 12345：已达配额，匹配流量已被丢弃"]);
    let reached = state(5 * gib, 5 * gib, "quota_reached", p, "in_out");
    assert!(events(&reached, &reached).is_empty());
    // A new period starts counting again.
    let next = events(&reached, &state(3 * gib, 3 * gib, "normal", "2026-12-01", "in_out"));
    assert_eq!(next, ["端口 12345：用量已达配额 50%"]);
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
        poll(f.state(), headers, Json(Poll { snapshot: snapshot(), result: None, lifecycle_result: None }))
    };
    for (digest, current) in
        [(component_digest(FILES), true), ("0".repeat(64), false), ("not-a-digest".into(), false)]
    {
        body(report(digest).await).await;
        assert_eq!(body(status(Admin, f.state(), Path(f.node)).await).await["component_current"], current);
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
        poll(f.state(), headers(), Json(Poll { snapshot: snapshot(), result: None, lifecycle_result: None }))
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
        then: None,
    };
    body(submit(Admin, f.state(), Path(f.node), Json(op())).await).await;
    body(submit(Admin, f.state(), Path(f.node), Json(op())).await).await;
    let task = body(
        poll(f.state(), headers(), Json(Poll { snapshot: snapshot(), result: None, lifecycle_result: None }))
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
    let instance = ExportFormat { format: "surge".into(), port: 12345 };
    let response = export_configuration(Admin, f.state(), Path(f.node), Query(instance)).await;
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
        then: None,
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
        then: None,
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
                payload: json!({}),
                then: None,
            })
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
}
#[tokio::test]
async fn two_module_changes_queue_their_second_step_on_the_hub() {
    let f = Fixture::new();
    let credential = f.bind().await;
    let reported = |pm_revision: &str| -> Snapshot {
        let pm = json!({"installed":true,"resources":[],"revision":pm_revision,"blocked":false});
        let mut state = serde_json::to_value(snapshot()).unwrap();
        state["installed"] = json!(true);
        state["modules"] = json!({"pm": pm});
        serde_json::from_value(state).unwrap()
    };
    let poll_with = |state: Snapshot, result: Option<(&str, &str)>| {
        let result = result.map(|(id, status)| Receipt {
            code: None,
            id: id.repeat(32),
            status: status.into(),
            message: String::new(),
            revision: String::new(),
        });
        poll(
            f.state(),
            f.headers(&credential),
            Json(Poll { snapshot: state, result, lifecycle_result: None }),
        )
    };
    body(poll_with(reported(&"1".repeat(64)), None).await).await;
    let instance = json!({"name":"40001","host":"example.com","port":40001,"public_port":40001,"psk":"","mode":"default"});
    let rule = json!({"port":40001,"name":"40001","quota_gb":0,"quota_mode":"in_out","reset_day":0,"expiry_date":"","egress_mbps":0});
    let create = |id: &str, then: FollowUp| Operation {
        module: "snell".into(),
        id: id.repeat(32),
        action: "apply".into(),
        intent: Some(ApplyIntent::Create),
        revision: "b".repeat(64),
        payload: instance.clone(),
        then: Some(then),
    };
    let pm_rule = |payload: Value| FollowUp {
        module: "pm".into(),
        action: "apply".into(),
        intent: Some(ApplyIntent::Create),
        payload,
    };
    // Only the two known pairings, on the same port.
    let mut other_port = rule.clone();
    other_port["port"] = json!(40002);
    for then in [
        pm_rule(other_port),
        FollowUp {
            module: "fw".into(),
            action: "delete".into(),
            intent: None,
            payload: json!({"port":40001}),
        },
    ] {
        assert_eq!(
            submit(Admin, f.state(), Path(f.node), Json(create("a", then))).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    body(submit(Admin, f.state(), Path(f.node), Json(create("a", pm_rule(rule.clone())))).await).await;
    // Repeating the submission is harmless; changing its follow-up is not.
    body(submit(Admin, f.state(), Path(f.node), Json(create("a", pm_rule(rule.clone())))).await).await;
    let mut named = rule.clone();
    named["name"] = json!("other");
    assert_eq!(
        submit(Admin, f.state(), Path(f.node), Json(create("a", pm_rule(named)))).await.status(),
        StatusCode::BAD_REQUEST
    );
    let first = body(poll_with(reported(&"1".repeat(64)), None).await).await;
    assert_eq!(first["task"]["request"]["id"], "a".repeat(32));
    assert!(first["task"]["request"].get("then").is_none(), "the node never sees the follow-up");
    // The first receipt queues the rule at the revision reported with it, and the same poll delivers it.
    let second = body(poll_with(reported(&"2".repeat(64)), Some(("a", "succeeded"))).await).await;
    let request = &second["task"]["request"];
    assert_eq!((request["module"].as_str(), request["action"].as_str()), (Some("pm"), Some("apply")));
    assert_eq!(
        (request["intent"].as_str(), request["revision"].as_str()),
        (Some("create"), Some(&*"2".repeat(64)))
    );
    assert_eq!(request["payload"], rule);
    let state = body(status(Admin, f.state(), Path(f.node)).await).await;
    let follow = state["operations"].as_array().unwrap().iter().find(|o| o["id"] == request["id"]).unwrap();
    assert_eq!(follow["after"], "a".repeat(32));
    assert_eq!(follow["status"], "delivered");
    let id = request["id"].as_str().unwrap().to_owned();
    let receipt = Receipt {
        code: None,
        id: id.clone(),
        status: "succeeded".into(),
        message: String::new(),
        revision: String::new(),
    };
    body(
        poll(
            f.state(),
            f.headers(&credential),
            Json(Poll { snapshot: reported(&"3".repeat(64)), result: Some(receipt), lifecycle_result: None }),
        )
        .await,
    )
    .await;

    // Deleting: rule first, then the instance; a failed first step queues nothing.
    let delete = |id: &str| Operation {
        module: "pm".into(),
        id: id.repeat(32),
        action: "delete".into(),
        intent: None,
        revision: "3".repeat(64),
        payload: json!({"port":40001}),
        then: Some(FollowUp {
            module: "snell".into(),
            action: "delete".into(),
            intent: None,
            payload: json!({"port":40001}),
        }),
    };
    body(submit(Admin, f.state(), Path(f.node), Json(delete("c"))).await).await;
    body(poll_with(reported(&"3".repeat(64)), None).await).await;
    let failed = body(poll_with(reported(&"3".repeat(64)), Some(("c", "rejected"))).await).await;
    assert!(failed["task"].is_null());
    let state = body(status(Admin, f.state(), Path(f.node)).await).await;
    assert!(state["operations"].as_array().unwrap().iter().all(|o| o["after"] != "c".repeat(32)));
}
