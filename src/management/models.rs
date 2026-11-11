//! Fixed module schemas.
use anyhow::Result;
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

pub fn default_module() -> String {
    "snell".into()
}
pub fn is_snell(v: &String) -> bool {
    v == "snell"
}
pub fn host(value: &str) -> bool {
    value.parse::<std::net::IpAddr>().is_ok()
        || (!value.is_empty()
            && value.len() <= 253
            && value.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            }))
}
fn port(value: &Value) -> bool {
    value.as_u64().is_some_and(|n| (1..=65535).contains(&n))
}
fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}
/// One module's own field rules, given whether the value is a component's snapshot.
type Fields = (&'static [&'static str], fn(&Value, bool) -> Result<()>);

/// Checks an SB, FW or PM resource; Snell's is the `Resource` struct in tasks.rs.
/// What these share is checked here, and each module's own rules in its
/// function below, so changing one leaves the others.
pub fn resource(module: &str, value: &Value, snapshot: bool) -> Result<()> {
    let Some(map) = value.as_object() else { refuse!("模块配置格式错误") };
    let (fields, check): Fields = match (module, snapshot) {
        ("sb", false) => (&["host", "public_port", "protocol", "sni", "psk"], sb),
        ("sb", true) => (
            &[
                "host",
                "public_port",
                "protocol",
                "sni",
                "psk",
                "uuid",
                "private_key",
                "public_key",
                "short_id",
            ],
            sb,
        ),
        ("fw", _) => (&["target_host", "target_port"], fw),
        ("pm", _) => (&["quota_gb", "quota_mode", "reset_day", "expiry_date", "egress_mbps", "ip_limit"], pm),
        _ => refuse!("不支持的模块"),
    };
    if map.keys().any(|k| !["port", "name"].contains(&k.as_str()) && !fields.contains(&k.as_str())) {
        refuse!("模块包含不支持字段");
    }
    let name = string(value, "name");
    if !port(&value["port"])
        || name.is_empty()
        || name.chars().count() > 48
        || !name.chars().all(|c| c.is_alphanumeric() || " _-".contains(c))
    {
        refuse!("名称或监听端口无效");
    }
    check(value, snapshot)
}

fn sb(value: &Value, snapshot: bool) -> Result<()> {
    if !host(string(value, "host")) || !port(&value["public_port"]) {
        refuse!("客户端访问地址或端口无效");
    }
    let psk = string(value, "psk");
    match string(value, "protocol") {
        "ss2022" => {
            if (snapshot || !psk.is_empty()) && STANDARD.decode(psk).ok().is_none_or(|s| s.len() != 16) {
                refuse!("SS2022 密钥须为 16 字节 Base64，留空生成或保留");
            }
        }
        "reality" => {
            let sni = string(value, "sni");
            if !host(sni) || !sni.contains('.') || sni.parse::<std::net::IpAddr>().is_ok() || !psk.is_empty()
            {
                refuse!("Reality 目标域名无效");
            }
            if snapshot {
                for key in ["private_key", "public_key"] {
                    if URL_SAFE_NO_PAD.decode(string(value, key)).ok().is_none_or(|v| v.len() != 32) {
                        refuse!("Reality 密钥格式错误");
                    }
                }
                let uuid = string(value, "uuid");
                if uuid.len() != 36
                    || uuid.bytes().enumerate().any(|(i, c)| {
                        if [8, 13, 18, 23].contains(&i) {
                            c != b'-'
                        } else {
                            !c.is_ascii_hexdigit()
                        }
                    })
                {
                    refuse!("Reality UUID 格式错误");
                }
                let sid = string(value, "short_id");
                if sid.len() != 8 || !sid.bytes().all(|c| c.is_ascii_hexdigit()) {
                    refuse!("Reality short ID 格式错误");
                }
            }
        }
        _ => refuse!("singbox 仅支持 Reality 或 SS2022"),
    }
    Ok(())
}

fn fw(value: &Value, _snapshot: bool) -> Result<()> {
    let Ok(ip) = string(value, "target_host").parse::<std::net::Ipv4Addr>() else {
        refuse!("目标必须为 IPv4 地址")
    };
    if ip.is_unspecified() || ip.is_multicast() || ip.is_link_local() || !port(&value["target_port"]) {
        refuse!("转发目标或端口无效");
    }
    Ok(())
}

fn pm(value: &Value, _snapshot: bool) -> Result<()> {
    if value["port"].as_u64().is_none_or(|n| n < 1024)
        || value["quota_gb"].as_u64().is_none_or(|n| n > 8589934591)
        || value["reset_day"].as_u64().is_none_or(|n| n > 31)
        || value["egress_mbps"].as_u64().is_none_or(|n| n > 10000)
        || !matches!(string(value, "quota_mode"), "in_out" | "out_only")
        || value["expiry_date"].as_str().is_none_or(|d| !d.is_empty() && !pm_date(d))
    {
        refuse!("流量统计端口或策略值无效");
    }
    if let Some(limit) = value.get("ip_limit") {
        ip_limit(limit)?;
    }
    Ok(())
}

fn valid_ip_or_network(value: &str) -> bool {
    let (address, prefix) = value.split_once('/').map_or((value, None), |(ip, prefix)| (ip, Some(prefix)));
    let Ok(ip) = address.parse::<std::net::IpAddr>() else { return false };
    prefix.is_none_or(|prefix| {
        !prefix.is_empty()
            && prefix.bytes().all(|c| c.is_ascii_digit())
            && prefix.parse::<u8>().is_ok_and(|n| n <= if ip.is_ipv4() { 32 } else { 128 })
    })
}

/// Present only when on, so a rule without it keeps the revision older components
/// computed; `block` likewise appears only as true. The component stores the
/// whitelist normalized.
fn ip_limit(value: &Value) -> Result<()> {
    let Some(map) = value.as_object() else { refuse!("IP 告警设置格式错误") };
    let whitelist = value["whitelist"].as_array();
    let block = map.get("block");
    if map.len() != 3 + usize::from(block.is_some())
        || block.is_some_and(|b| b != true)
        || value["max_ips"].as_u64().is_none_or(|n| !(1..=65535).contains(&n))
        || value["cooldown_min"].as_u64().is_none_or(|n| !(1..=525600).contains(&n))
        || whitelist.is_none_or(|ips| {
            ips.len() > 32 || ips.iter().any(|ip| ip.as_str().is_none_or(|s| !valid_ip_or_network(s)))
        })
    {
        refuse!("IP 上限须为 1–65535，冷却 1–525600 分钟，白名单最多 32 项 IP 地址或 CIDR 网段");
    }
    Ok(())
}

fn pm_date(value: &str) -> bool {
    value.len() == 10
        && chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .is_ok_and(|d| chrono::Datelike::year(&d) >= 1 && d.format("%Y-%m-%d").to_string() == value)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PmUsage {
    rx: u64,
    tx: u64,
    sampled_at: u64,
    period_start: String,
    alert: String,
    /// The IP check: `ip_count` and `ip_list` for every recent rule, `ip_level`
    /// and `ip_alert` only for a rule with an IP limit. `ip_alert` counts alerts
    /// raised; a change with `ip_level` above zero is a new one to send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_level: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_alert: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_list: Option<Vec<String>>,
}

pub fn core_version(value: &str) -> bool {
    let mut parts = value.split('.');
    (0..3).all(|_| {
        parts.next().is_some_and(|part| {
            (1..=5).contains(&part.len())
                && (part.len() == 1 || !part.starts_with('0'))
                && part.bytes().all(|c| c.is_ascii_digit())
        })
    }) && parts.next().is_none()
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoreSnapshot {
    version: Option<String>,
    sha256: Option<String>,
    previous_version: Option<String>,
    candidates: Option<Vec<String>>,
}

impl CoreSnapshot {
    pub fn validate(&self, module: &str, installed: bool) -> Result<()> {
        let Some(candidates) = &self.candidates else {
            refuse!("核心版本快照缺少候选列表");
        };
        if self.version.as_deref().is_some_and(|version| !core_version(version))
            || self.previous_version.as_deref().is_some_and(|version| !core_version(version))
            || self
                .sha256
                .as_ref()
                .is_some_and(|hash| hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()))
            || candidates.len() > 16
            || candidates.iter().any(|version| !core_version(version))
            || ((!installed || module == "pm" || self.version.is_none())
                && (self.previous_version.is_some() || !candidates.is_empty()))
            || ((!installed || module == "pm") && (self.version.is_some() || self.sha256.is_some()))
        {
            refuse!("核心版本快照无效");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleSnapshot {
    pub installed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core: Option<CoreSnapshot>,
    pub resources: Vec<Value>,
    pub revision: String,
    pub blocked: bool,
    #[serde(default)]
    pub runtime: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub usage: BTreeMap<String, PmUsage>,
}
impl ModuleSnapshot {
    pub fn validate(&self, module: &str) -> Result<()> {
        if !matches!(module, "sb" | "fw" | "pm")
            || self.resources.len() > 64
            || self.revision.len() != 64
            || !self.revision.bytes().all(|c| c.is_ascii_hexdigit())
            || self.runtime.len() > 64
            || self.usage.len() > 64
            || (module != "pm" && !self.usage.is_empty())
            || (module == "pm"
                && (!self.runtime.is_empty() || (!self.installed && !self.resources.is_empty())))
        {
            refuse!("模块快照无效");
        }
        if let Some(core) = &self.core {
            core.validate(module, self.installed)?;
        }
        let mut ports = HashSet::new();
        for r in &self.resources {
            resource(module, r, true)?;
            if !ports.insert(r["port"].as_u64()) {
                refuse!("模块端口重复");
            }
        }
        for (port, usage) in &self.usage {
            let key = port.parse::<u64>().ok();
            if !ports.contains(&key)
                || key.is_none_or(|p| p.to_string() != *port)
                || usage.rx > i64::MAX as u64
                || usage.tx > i64::MAX as u64
                || usage.sampled_at == 0
                || usage.ip_level.is_some_and(|l| l > 2)
                || usage.ip_list.as_ref().is_some_and(|ips| {
                    ips.len() > 15 || ips.iter().any(|ip| ip.parse::<std::net::IpAddr>().is_err())
                })
                || (!usage.period_start.is_empty() && !pm_date(&usage.period_start))
                || !matches!(
                    usage.alert.as_str(),
                    "normal" | "quota_warning" | "quota_reached" | "expired" | "maintenance_pending"
                )
            {
                refuse!("流量统计用量快照无效");
            }
        }
        for (port, state) in &self.runtime {
            if port.parse::<u16>().ok().is_none_or(|p| p == 0)
                || !matches!(
                    state.as_str(),
                    "active" | "inactive" | "failed" | "activating" | "deactivating" | "unknown"
                )
            {
                refuse!("模块运行状态无效");
            }
        }
        Ok(())
    }
}

// Encode URI components, including Base64 key punctuation. SIP002 requires
// plaintext, percent-encoded userinfo for AEAD-2022 rather than Base64 userinfo.
fn uri_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(encoded, "%{byte:02X}").expect("write to string");
        }
    }
    encoded
}

pub fn share_links(state: &ModuleSnapshot) -> Result<String> {
    state.validate("sb")?;
    if state.blocked || state.resources.is_empty() {
        refuse!("singbox 尚无可下载配置");
    }
    Ok(state
        .resources
        .iter()
        .map(|resource| {
            let host = string(resource, "host");
            let host = if host.contains(':') { format!("[{host}]") } else { host.to_owned() };
            let port = &resource["public_port"];
            let name = uri_component(string(resource, "name"));
            if resource["protocol"] == "ss2022" {
                return format!(
                    "ss://2022-blake3-aes-128-gcm:{}@{host}:{port}#{name}",
                    uri_component(string(resource, "psk"))
                );
            }
            format!(
                "vless://{}@{host}:{port}?encryption=none&security=reality&sni={}&fp=chrome&pbk={}&sid={}&type=tcp&flow=xtls-rprx-vision#{name}",
                string(resource, "uuid"),
                uri_component(string(resource, "sni")),
                uri_component(string(resource, "public_key")),
                uri_component(string(resource, "short_id")),
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn fields_reject_commands_and_bad_addresses() {
        let mut v = json!({"name":"fw","port":23456,"target_host":"127.0.0.1","target_port":12345});
        assert!(resource("fw", &v, false).is_ok());
        v["command"] = json!("id");
        assert!(resource("fw", &v, false).is_err());
        assert!(!host("foo..bar"));
        assert!(!host("x\nother"));
        assert!(host("example.com"));
    }
    #[test]
    fn pm_policy_and_usage_validate() {
        let policy = json!({"port":12345,"name":"PM","quota_gb":0,"quota_mode":"in_out","reset_day":31,"egress_mbps":0,"expiry_date":"2028-02-29"});
        assert!(resource("pm", &policy, false).is_ok());
        let mut invalid = policy.clone();
        invalid["port"] = json!(22);
        assert!(resource("pm", &invalid, false).is_err());
        invalid["port"] = json!(12345);
        invalid["expiry_date"] = json!("2026-02-29");
        assert!(resource("pm", &invalid, false).is_err());
        let mut snapshot: ModuleSnapshot = serde_json::from_value(json!({
            "installed":true,"blocked":false,"revision":"a".repeat(64),"resources":[policy],
            "usage":{"12345":{"rx":1,"tx":2,"sampled_at":1,"period_start":"2028-02-29","alert":"quota_warning"}}
        })).unwrap();
        assert!(snapshot.validate("pm").is_ok());
        let usage = snapshot.usage.remove("12345").unwrap();
        snapshot.usage.insert("12346".into(), usage);
        assert!(snapshot.validate("pm").is_err());
        let mut limited = policy.clone();
        limited["ip_limit"] = json!({"max_ips":3,"cooldown_min":30,"whitelist":["203.0.113.7"]});
        assert!(resource("pm", &limited, true).is_ok());
        let usage = |list: Value| {
            serde_json::from_value::<ModuleSnapshot>(json!({
                "installed":true,"blocked":false,"revision":"a".repeat(64),"resources":[limited.clone()],
                "usage":{"12345":{"rx":1,"tx":2,"sampled_at":1,"period_start":"","alert":"normal",
                    "ip_count":4,"ip_level":1,"ip_alert":1,"ip_list":list}}
            }))
            .unwrap()
            .validate("pm")
        };
        assert!(usage(json!(["203.0.113.1", "2001:db8::1"])).is_ok());
        assert!(usage(json!(["not an address"])).is_err());
        assert!(usage(json!(vec!["203.0.113.1"; 16])).is_err());
    }
    #[test]
    fn ip_note_names_levels_by_count() {
        let alert = crate::notify::IpAlert {
            port: "12345".into(),
            name: "PM".into(),
            count: 17,
            max: 3,
            block: true,
            level: 2,
            ips: (1..=15).map(|n| format!("203.0.113.{n}")).collect(),
        };
        let places = [("203.0.113.1".to_owned(), "中国, 上海, 某运营商".to_owned())].into();
        let note = crate::notify::ip_note("hk", &alert, &places);
        assert_eq!(note.title, "🔴 异常接入报警 · hk");
        assert!(note.message.contains("在线 17 IP · 等级 报警（>5）"));
        assert!(note.message.contains("已开启阻断：>6 的后续连接阻断 30 分钟"));
        let warning = crate::notify::IpAlert { level: 1, ips: vec![], block: false, ..alert.clone() };
        let message = crate::notify::ip_note("hk", &warning, &places).message;
        assert!(message.contains("等级 告警（>3）") && message.contains("未开启阻断，只提醒"));
        assert!(note.message.contains("1. 203.0.113.1 - 中国, 上海, 某运营商"));
        assert!(note.message.contains("2. 203.0.113.2 - (查询失败)"));
        assert!(note.message.contains("... 仅显示前 15 条"));
        let mut local = alert;
        local.ips = ["127.0.0.1", "10.0.0.2", "100.64.1.1", "fd00::1", "fe80::1", "::1", "100.128.0.1"]
            .map(String::from)
            .to_vec();
        let message = crate::notify::ip_note("hk", &local, &std::collections::HashMap::new()).message;
        assert_eq!(message.matches("本机／内网地址").count(), 6, "{message}");
        assert!(message.contains("100.128.0.1 - (查询失败)"), "the shared range ends at 100.127");
    }
    #[test]
    fn share_links_encode_names_keys_and_ipv6_and_never_export_private_keys() {
        let private = URL_SAFE_NO_PAD.encode([2; 32]);
        let mut state: ModuleSnapshot = serde_json::from_value(json!({
            "installed":true,"blocked":false,"revision":"a".repeat(64),"resources":[
                {"name":"现实 入口","port":443,"host":"2001:db8::1","public_port":8443,
                 "protocol":"reality","sni":"example.com","psk":"",
                 "uuid":"11111111-1111-4111-8111-111111111111","public_key":URL_SAFE_NO_PAD.encode([1;32]),
                 "private_key":private,"short_id":"1234abcd"},
                {"name":"SS 入口","port":20000,"host":"example.org","public_port":30000,
                 "protocol":"ss2022","sni":"","psk":STANDARD.encode([251;16])}
            ]
        }))
        .unwrap();
        let text = share_links(&state).unwrap();
        let links: Vec<_> = text.lines().collect();
        assert_eq!(links.len(), 2);
        assert!(links[0].starts_with("vless://11111111-1111-4111-8111-111111111111@[2001:db8::1]:8443?"));
        let url = reqwest::Url::parse(links[0]).unwrap();
        let query: BTreeMap<_, _> = url.query_pairs().collect();
        assert_eq!(query["sni"], "example.com");
        assert_eq!(query["pbk"], URL_SAFE_NO_PAD.encode([1; 32]));
        assert_eq!(query["sid"], "1234abcd");
        assert_eq!(query["flow"], "xtls-rprx-vision");
        assert!(!text.contains(&private) && !text.contains("private_key"));
        assert!(links[0].contains("#%E7%8E%B0%E5%AE%9E%20%E5%85%A5%E5%8F%A3"));
        assert!(links[1].starts_with("ss://2022-blake3-aes-128-gcm:"));
        assert!(links[1].contains("%2B") && links[1].contains("%2F") && links[1].contains("%3D"));
        assert!(links[1].ends_with("@example.org:30000#SS%20%E5%85%A5%E5%8F%A3"));
        state.blocked = true;
        assert!(share_links(&state).is_err());
        state.blocked = false;
        state.resources.clear();
        assert!(share_links(&state).is_err());
    }
}
