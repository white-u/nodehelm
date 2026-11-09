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
    // The same fixture is asserted against modules.request in management/tests/test_transport.py.
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
