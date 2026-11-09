//! Nodes: the views the panel and public page read, and the panel's writes,
//! automatic registration included.

use super::*;

// ---- read paths, shared between the panel and the public page ----

/// Everything a report may expose under `metrics`: the agent contract minus the
/// raw kernel counters, which are a wire-protocol detail disclosing a machine's
/// entire lifetime traffic, plus the four traffic figures `node_view` fills in.
/// The panel additionally sees `iface`.
pub(crate) const PUBLIC_METRICS: [&str; 18] = [
    "uptime",
    "cpu",
    "load",
    "mem_total",
    "mem_used",
    "swap_total",
    "swap_used",
    "disk_total",
    "disk_used",
    "net_rx",
    "net_tx",
    "tcp",
    "udp",
    "procs",
    "total_rx",
    "total_tx",
    "month_rx",
    "month_tx",
];

/// The addresses the panel shows for a node, each with where it comes from: at
/// most one per family, v4 first, the one the machine is reached by. The agent
/// reports its interfaces; `exits` are where the hub saw it: the address its
/// session arrived from, and the one a short second connection arrived from in
/// the other family, each in dotted form for IPv4.
///
/// Per family an address set by hand comes first, then a public one on the
/// interface. Failing both, where the interface holds only a private address of
/// that family -- NAT, or a proxy in front -- the public address the hub saw in
/// it, its exit, stands in. An exit in a family the interface does not hold is a
/// translator such as NAT64 or WARP and is left out.
///
/// Private addresses appear only when nothing public is known, as where hub and
/// node share a network and they are all there is. The session's own exit alone
/// is the fallback for an agent reporting no interface.
pub(super) fn addresses<'a>(
    exits: [&'a str; 2],
    (ipv4, ipv6): (&'a str, &'a str),
    (pin4, pin6): (&'a str, &'a str),
) -> Vec<(&'a str, &'static str)> {
    let public =
        |a: &str, v6: bool| a.parse::<IpAddr>().is_ok_and(|a| a.is_ipv6() == v6 && agent_ws::public(a));
    let family = |pin: &'a str, held: &'a str, v6: bool| {
        if !pin.is_empty() {
            Some((pin, "manual"))
        } else if public(held, v6) {
            Some((held, "interface"))
        } else if held.is_empty() {
            None
        } else {
            exits.into_iter().find(|ip| public(ip, v6)).map(|exit| (exit, "exit"))
        }
    };
    let shown: Vec<_> = [family(pin4, ipv4, false), family(pin6, ipv6, true)].into_iter().flatten().collect();
    if !shown.is_empty() {
        return shown;
    }
    let held: Vec<_> = [ipv4, ipv6].into_iter().filter(|a| !a.is_empty()).map(|a| (a, "interface")).collect();
    if !held.is_empty() {
        return held;
    }
    [exits[0]].into_iter().filter(|a| !a.is_empty()).map(|a| (a, "connection")).collect()
}

/// One node as the UI consumes it: stored config, live metrics and the hub's
/// accumulated traffic in a single object.
fn node_view(
    node: &Node,
    current: Option<&Agent>,
    traffic: &Traffic,
    full: bool,
    now: DateTime<Local>,
) -> Value {
    // The three capacities arrive in every `Metrics`. The stored figure follows the
    // reports a minute at a time and as the session ends, so it lags a disk mounted
    // while the agent runs -- the agent re-reads its mount table every sample so
    // that it appears -- by up to a minute. Using the report while a node is
    // connected keeps every consumer of this view on one number: the card reads the
    // live metrics and the detail page reads these, which would otherwise show the
    // same machine two different capacities. Offline, the stored figure is the last
    // one reported. No floor is applied: a host whose swap has just been disabled
    // reports zero and means it. A node connected but not yet reporting holds
    // `Null`, where `get` returns nothing and the stored figure stands.
    let live = |key: &str, stored: i64| {
        current.and_then(|a| a.metrics.get(key).and_then(serde_json::Value::as_i64)).unwrap_or(stored)
    };
    // The live entry while connected, the stored one afterwards. Zero means
    // connected but not yet reporting, which is not a timestamp, so it falls
    // back to the stored value and "offline since" survives the gap.
    let last_seen = current.map(|a| a.last_seen).filter(|t| *t > 0).unwrap_or(node.last_seen);
    let mut view = json!({
        "id": node.id,
        "name": node.name,
        // A country rather than an address: it indicates which region a node sits
        // in, which is what a status page conveys, without locating it. The
        // address it was derived from remains behind the panel.
        "country": if node.country_pin.is_empty() { &node.country } else { &node.country_pin },
        // Named by the operator for the status page to divide the list by, so
        // public like the node's name. Empty is ungrouped.
        "group": node.group,
        // Written by the operator for visitors, unlike `remark` below.
        "public_remark": node.public_remark,
        "sort": node.sort,
        "public": node.public,
        "online": current.is_some(),
        "last_seen": last_seen,
        // Counted on the hub's clock, for the same reason as `expires_in`: a page
        // subtracting `last_seen` from its own clock is off by as much as that
        // clock is, and one eight hours fast shows a node that dropped a minute
        // ago as offline for eight hours. Null for a node never seen.
        "last_seen_ago": (last_seen > 0).then(|| (now.timestamp() - last_seen).max(0)),
        "metrics": current.map(|a| a.metrics.clone()).unwrap_or(Value::Null),
        "os": node.os,
        "kernel": node.kernel,
        "arch": node.arch,
        "virt": node.virt,
        "cpu_name": node.cpu_name,
        "cpu_cores": node.cpu_cores,
        "mem_total": live("mem_total", node.mem_total),
        "swap_total": live("swap_total", node.swap_total),
        "disk_total": live("disk_total", node.disk_total),
        "agent_version": node.agent_version,
        "price": node.price,
        "currency": node.currency,
        "billing_cycle": node.billing_cycle,
        "expires_at": node.expires_at,
        // Counted on the hub's calendar, the one renewal follows. A page counting
        // on the visitor's clock would, with the hub on UTC and the visitor on
        // UTC+8, show every online node expired for eight hours each cycle
        // before the hub rolls its date forward.
        "expires_in": node.expires_at.as_deref().and_then(|d| d.parse::<NaiveDate>().ok()).map(|d| (d - now.date_naive()).num_days()),
        "traffic_limit": node.traffic_limit,
        "traffic_mode": node.traffic_mode,
        "traffic_reset_day": node.traffic_reset_day,
        "total_rx": traffic.total_rx,
        "total_tx": traffic.total_tx,
        "month_rx": traffic.month_rx,
        "month_tx": traffic.month_tx,
        "month_used": traffic.month_used(&node.traffic_mode),
        "month_start": traffic.month_start,
        // Of the same nature as the month and lifetime figures beside it, which
        // the public page already shows, so this one is public as well.
        "day_rx": traffic.day_rx,
        "day_tx": traffic.day_tx,
    });
    // An allowlist rather than a denylist, for the panel as well: the agent ships
    // from its own repository, so a field added there would otherwise reach
    // anonymous visitors the day it is released, and a node token in the wrong
    // hands could fill the panel's frame with whatever it sends. No address,
    // hostname or private note may ever reach a visitor.
    if let Some(m) = view["metrics"].as_object_mut() {
        m.retain(|k, _| PUBLIC_METRICS.contains(&k.as_str()) || (full && k == "iface"));
        // The same figures as the top-level ones, from the same row. Both official
        // themes refuse a node's live view without them.
        for (key, value) in agent_ws::INJECTED.into_iter().zip([
            traffic.total_rx,
            traffic.total_tx,
            traffic.month_rx,
            traffic.month_tx,
        ]) {
            m.insert(key.into(), json!(value));
        }
    }
    // Address and private notes never leave the panel. The token is not here at
    // all: the panel reads it from `node_token` as it shows the install command.
    if full {
        let held = (node.ipv4.as_str(), node.ipv6.as_str());
        let (pin4, pin6) = (node.ipv4_pin.as_str(), node.ipv6_pin.as_str());
        let exits = [node.ip.as_str(), node.ip_other.as_str()];
        let shown = addresses(exits, held, (pin4, pin6));
        // What each family shows with its own pin cleared and the other's kept,
        // for the edit form to offer as the fallback.
        let auto = |pins, v6: bool| {
            addresses(exits, held, pins)
                .into_iter()
                .find(|(a, _)| a.contains(':') == v6)
                .map_or("", |(a, _)| a)
        };
        view["hostname"] = json!(node.hostname);
        view["ip"] = json!(node.ip);
        view["ipv4"] = json!(node.ipv4);
        view["ipv6"] = json!(node.ipv6);
        view["ipv4_pin"] = json!(node.ipv4_pin);
        view["ipv6_pin"] = json!(node.ipv6_pin);
        view["addresses"] =
            shown.iter().map(|(address, source)| json!({"address": address, "source": source})).collect();
        view["ipv4_auto"] = json!(auto(("", pin6), false));
        view["ipv6_auto"] = json!(auto((pin4, ""), true));
        view["interval"] = json!(current.and_then(Agent::interval));
        view["country_pin"] = json!(node.country_pin);
        view["country_auto"] = json!(node.country);
        view["remark"] = json!(node.remark);
        view["notify"] = json!(node.notify);
    }
    view
}

pub(super) fn visible_nodes(app: &App, full: bool) -> Result<Vec<Value>, anyhow::Error> {
    // One traffic query and one lock for the whole list, since this is what every
    // visitor to the public page loads.
    let nodes = app.db.nodes()?;
    let traffic = app.db.all_traffic();
    let agents = app.agents.read().unwrap_or_else(|e| e.into_inner());
    let none = Traffic::default();
    let now = Local::now();
    Ok(nodes
        .iter()
        .filter(|n| full || n.public)
        .map(|n| node_view(n, agents.get(&n.id), traffic.get(&n.id).unwrap_or(&none), full, now))
        .collect())
}

pub async fn nodes(State(app): State<Shared>, headers: HeaderMap) -> Response {
    let full = authed(&app, &headers);
    if !full && !app.public_page() {
        return answer(StatusCode::UNAUTHORIZED, "需要登录后查看");
    }
    // The same rendered frame the browser streams receive, for the same reason:
    // otherwise every visitor would rebuild every node's row against the
    // connection the agents write through.
    //
    // Compressed likewise, once per frame: left to the compression layer, every
    // request deflates the whole list again, about 6 ms of CPU for a thousand
    // nodes, and this route answers anyone. Already encoded, the layer passes
    // it through without adding the Vary it adds to what it compresses.
    if !full && accepts_gzip(&headers) {
        return (
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CONTENT_ENCODING, "gzip"),
                (header::VARY, "accept-encoding"),
            ],
            live_gzip(&app),
        )
            .into_response();
    }
    let mut res =
        ([(header::CONTENT_TYPE, "application/json")], axum::body::Bytes::from(live_snapshot(&app, full)))
            .into_response();
    // Uncompressed by the hub and by Cloudflare, as the admin stream is: every
    // node's address and private notes beside strings an agent reports.
    if full {
        res.headers_mut()
            .insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store, no-transform"));
    }
    res
}

// ---- panel write paths ----

/// Names all three causes. `--site` is the one input to this decision that
/// nothing about the request reveals: a hub started with
/// `--site https://198.51.100.7` refuses every provisioning call from an
/// otherwise valid https domain entry, and a tunnelled panel is refused until
/// one is given. `main` warns about the first at startup; this is for whoever
/// reads the panel rather than the journal.
pub(super) const PROVISIONING_DENIED: &str = "请通过 HTTPS 域名访问面板后添加或安装节点；\
     从隧道或回环地址进面板时，给 hub 加 --site 指定节点可达的域名；\
     --site 必须是 https:// 加域名，不能是 IP、不能带路径";

/// Every browser sends `Origin` with these writes, so its absence points at a
/// proxy clearing it. The panel judges from its own address bar and offers the
/// button, which leaves this as the only account of why the hub refuses.
pub(super) const ORIGIN_MISSING: &str = "请求没有带 Origin 头。浏览器都会发送它，多半是反向代理清掉了\
     （例如 proxy_set_header Origin \"\"），去掉那一行后再试";

/// Whether this origin is the hub's own machine, which is what a tunnel into the
/// panel leaves in the address bar. Nothing between that browser and the hub is
/// in the clear -- it is the loopback interface, or the tunnel's own encryption
/// -- so the entry is sound; what it lacks is an address a node could use, which
/// is why it counts only alongside `--site`.
fn loopback_origin(origin: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(origin) else { return false };
    let Some(host) = url.host_str() else { return false };
    // host_str keeps the brackets an IPv6 literal is written with.
    let ip = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>();
    matches!(url.scheme(), "http" | "https")
        && (host == "localhost" || host.ends_with(".localhost") || ip.is_ok_and(|ip| ip.is_loopback()))
}

pub(crate) fn https_domain(site: &str) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(site).ok()?;
    (url.scheme() == "https"
        && url.domain().is_some_and(|d| d != "localhost" && !d.ends_with(".localhost"))
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none())
    .then_some(url)
}

/// Whether the browser sending this request is on an https domain entry, which
/// is the only address the panel may build install commands from.
///
/// The browser's own `Origin` answers it. Reconstructing the entry from `Host`
/// and `X-Forwarded-Proto` instead holds only where every proxy in front
/// forwards both, and the common ones do not: the aapanel and BT templates send
/// no `X-Forwarded-Proto`, a bare `proxy_pass` sends its own upstream address as
/// `Host` (as does Apache under `ProxyPreserveHost Off`), `$host` drops a port
/// that is not 443, and a TLS edge ahead of a plaintext hop leaves
/// `X-Forwarded-Proto: http`. Each of those refuses a panel that is in fact on
/// https, and none can be told apart here from a genuine plaintext entry.
/// `Origin` crosses all of them unchanged, and a page cannot forge its own.
///
/// `--site` is measured by the same rule, because it takes this origin's place
/// in the command: an IP or a path there is refused however the panel is
/// reached. It also answers for the one entry this origin cannot: a panel opened
/// over a tunnel reads `http://127.0.0.1:PORT`, which names no address a node
/// could reach, while `--site` names one and the tunnel carries the session
/// under its own encryption.
///
/// The error is the message the panel shows.
pub(super) fn provisioning_allowed(app: &App, headers: &HeaderMap) -> Result<(), &'static str> {
    if !app.site.is_empty() && https_domain(&app.site).is_none() {
        debug!("provisioning refused: --site {:?} is not an https domain entry", app.site);
        return Err(PROVISIONING_DENIED);
    }
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        debug!("provisioning refused: the request carries no readable Origin");
        return Err(ORIGIN_MISSING);
    };
    if https_domain(origin).is_none() && !(loopback_origin(origin) && !app.site.is_empty()) {
        debug!("provisioning refused: Origin {origin:?} is not an https domain entry");
        return Err(PROVISIONING_DENIED);
    }
    Ok(())
}

/// Range and sign limits every stored node must satisfy, or the reason it does
/// not. Shared because both writers must enforce them: a value one refuses must
/// not be reachable through the other.
fn node_limits(reset_day: Option<u32>, price: Option<f64>, limit: Option<i64>) -> Option<&'static str> {
    if reset_day.is_some_and(|d| !(1..=31).contains(&d)) {
        return Some("流量重置日要在 1 到 31 之间");
    }
    if price.is_some_and(|v| !v.is_finite() || v < 0.0) || limit.is_some_and(|v| v < 0) {
        return Some("价格和流量上限不能是负数");
    }
    None
}

/// A theme shows the group as a tab label or a section title, so it is held to a
/// length that fits one line on a 390 px phone.
pub(super) const MAX_GROUP: usize = 13;

/// Trims a group name, or refuses it. Refused rather than truncated: the panel
/// would otherwise report saved a name that is not the one stored.
fn group_error(group: &mut String) -> Option<&'static str> {
    *group = group.trim().to_owned();
    if group.chars().count() > MAX_GROUP || group.chars().any(char::is_control) {
        return Some("分组名最多 13 个字，不能含控制字符");
    }
    None
}

/// Node and probe names, as a panel row, a theme card and a chart legend show
/// them. Refused rather than truncated, as a group name is.
pub(super) const MAX_NAME: usize = 64;

/// Trims a node name, or refuses it.
fn name_error(name: &mut String) -> Option<&'static str> {
    *name = name.trim().to_owned();
    if name.is_empty() {
        return Some("请填写节点名称");
    }
    if name.chars().count() > MAX_NAME {
        return Some("节点名称最多 64 个字");
    }
    None
}

/// A public remark goes to every visitor in every frame, every two seconds, where
/// a node's public view is about 1 KB; a hundred CJK characters add 300 bytes.
pub(super) const MAX_PUBLIC_REMARK: usize = 100;

/// Normalizes the currency and billing cycle, or names the one that cannot be
/// stored. The currency is held to the ISO 4217 form of three letters, the only
/// one `Intl.NumberFormat` accepts. Hubs before 1.3.1 stored it unchecked, so a
/// theme still guards the call against a throw. Each cycle length is stored in a
/// single spelling, see `cycle_name`.
fn billing_error(currency: Option<&mut String>, cycle: Option<&mut String>) -> Option<&'static str> {
    if let Some(code) = currency {
        *code = code.trim().to_ascii_uppercase();
        if !(code.len() == 3 && code.bytes().all(|b| b.is_ascii_uppercase())) {
            return Some("货币要填三个字母的代码，如 USD、CNY、HKD");
        }
    }
    if let Some(cycle) = cycle.filter(|c| *c != "once") {
        let Some(months) = crate::cycle_months(cycle) else {
            return Some("付款周期要是整月，在 1 个月到 100 年之间");
        };
        *cycle = crate::cycle_name(months);
    }
    None
}

/// Normalizes a patch, or names the first value that cannot be stored. The one
/// check both the single and the batch write pass through, so the two accept
/// exactly the same values.
fn patch_error(node: &mut NodePatch) -> Option<&'static str> {
    if let Some(message) = node.name.as_mut().and_then(name_error) {
        return Some(message);
    }
    if let Some(group) = &mut node.group {
        if let Some(message) = group_error(group) {
            return Some(message);
        }
    }
    // Refused rather than truncated, as a group name is.
    if let Some(text) = &mut node.public_remark {
        *text = text.trim().to_owned();
        if text.chars().count() > MAX_PUBLIC_REMARK || text.chars().any(char::is_control) {
            return Some("公开备注最多 100 个字，不能含控制字符");
        }
    }
    node_limits(node.traffic_reset_day, node.price, node.traffic_limit)
        .or_else(|| billing_error(node.currency.as_mut(), node.billing_cycle.as_mut()))
        .or_else(|| pins(node))
}

/// Normalizes the values set by hand, or names the one that cannot stand. Each
/// takes the place of an automatic value, so it is held to what that value would
/// have to be: the country to the rule a looked-up one passes, as both reach the
/// status page; an address to its own family, stored canonical so the panel's
/// search matches however it was typed.
fn pins(node: &mut NodePatch) -> Option<&'static str> {
    if let Some(cc) = &mut node.country_pin {
        *cc = cc.trim().to_ascii_uppercase();
        if !cc.is_empty() && !(cc.len() == 2 && cc.bytes().all(|b| b.is_ascii_uppercase())) {
            return Some("国家要填两个字母的代码，留空则自动识别");
        }
    }
    for (pin, v6) in [(&mut node.ipv4_pin, false), (&mut node.ipv6_pin, true)] {
        let Some(pin) = pin else { continue };
        let typed = pin.trim();
        let parsed = if v6 {
            typed.parse::<Ipv6Addr>().map(IpAddr::V6)
        } else {
            typed.parse::<Ipv4Addr>().map(IpAddr::V4)
        };
        *pin = match parsed {
            Ok(ip) => ip.to_string(),
            Err(_) if typed.is_empty() => String::new(),
            Err(_) if v6 => return Some("IPv6 要填一个 IPv6 地址，或者留空"),
            Err(_) => return Some("IPv4 要填一个 IPv4 地址，或者留空"),
        };
    }
    None
}

pub async fn me(State(app): State<Shared>, headers: HeaderMap) -> Json<Value> {
    Json(json!({
        "authed": authed(&app, &headers),
        "github": app.db.get("github_client_id").is_some_and(|v| !v.is_empty()),
        "site_name": app.db.get("site_name").unwrap_or_else(|| "Monitor".into()),
        "public_page": app.public_page(),
        // How far back a chart may reach, so a theme offers only windows the hub
        // answers in full; `metrics` narrows a wider one without saying so.
        "history_days": app.db.retention_days(),
        // Whether this browser may provision is not answered here: a GET carries
        // no `Origin`, so the panel applies `provisioning_allowed`'s rule itself.
        //
        // The hub's own public URL when one was given, which is what belongs in an
        // install command and in the OAuth callback -- not whichever address this
        // browser used, which behind a proxy may be a loopback port. Empty by
        // default, in which case the browser's address is the only one available
        // and the panel falls back to its own origin.
        "site": app.site,
    }))
}

pub async fn create_node(
    _: Admin,
    State(app): State<Shared>,
    headers: HeaderMap,
    body: Result<Json<Node>, JsonRejection>,
) -> Response {
    if let Err(refusal) = provisioning_allowed(&app, &headers) {
        return answer(StatusCode::FORBIDDEN, refusal);
    }
    let Ok(Json(mut node)) = body else { return bad("节点数据格式不对") };
    if let Some(message) = name_error(&mut node.name)
        .or_else(|| node_limits(Some(node.traffic_reset_day), Some(node.price), Some(node.traffic_limit)))
        .or_else(|| group_error(&mut node.group))
        .or_else(|| billing_error(Some(&mut node.currency), Some(&mut node.billing_cycle)))
    {
        return bad(message);
    }
    let token = random_token();
    match app.db.create_node(&node, &token) {
        // Usable immediately: the install dialog reads the token as it opens, so
        // adding and deploying require no reissue in between.
        Ok(id) => {
            invalidate_snapshot(&app);
            Json(json!({"id": id})).into_response()
        }
        Err(e) => fail(e),
    }
}

// ---- automatic registration ----

/// How long a registration window stays open.
///
/// Provisioning a batch of machines takes minutes, and the window expires on its
/// own rather than depending on someone returning to close it.
pub(super) const REGISTER_WINDOW: i64 = 3600;

/// How many nodes one window may register.
///
/// Without it, whoever holds the key for the hour could fill the node table. A
/// hundred is well beyond a plausible batch and well short of a problem.
pub(super) const REGISTER_LIMIT: i64 = 100;

/// The header in which `install.sh` sends the token the machine already holds.
pub(super) const HELD_TOKEN: &str = "x-node-token";

/// Exchanges a registration key for a node token, so a batch of machines can be
/// installed with one command rather than one panel visit each.
///
/// No session stands behind this route: the caller is `install.sh` on a machine
/// that has never contacted the hub. A key issued by the panel, valid only within
/// [`REGISTER_WINDOW`], serves in place of a session.
///
/// `provisioning_allowed` does not guard it, because a shell script sends no
/// `Origin` and the proxy headers that remain cannot state what address the
/// caller used. Which addresses may be registered against is enforced in
/// `install.sh`, where that address is known: it refuses plaintext to anything
/// but loopback unless `--insecure` is given, the same switch under which the
/// binary it is about to run as root was already fetched in the clear. The
/// window this key belongs to is still opened from an https domain entry alone.
///
/// One request costs at most a lookup on the token's unique index, two setting
/// reads, a `COUNT`, and one transaction inserting the `node` and `traffic` rows
/// plus a `ping_node` row per `auto_join` probe, at most 64. It makes no outbound
/// request, and the router's 64 KiB body limit bounds the name. Requests in flight
/// are not gated: each step is a point read or one small transaction under the
/// database lock, as with the token lookup of the agent handshake, and an address
/// locked out below reaches none of them. 120 concurrent callers with the window
/// closed move the panel's median from 1.1 ms to 2.1-2.8 ms, with or without a
/// held token, on three cores.
pub async fn agent_register(
    State(app): State<Shared>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    // Plain text in, plain text out. The caller is a shell script, as with this
    // route's neighbours: `/install.sh` and `/agent/{arch}` return a script and a
    // binary. A bare token is one `$(curl ...)` away, requiring no JSON parser in
    // a POSIX `sh`.
    name: String,
) -> Response {
    let ip = client_ip(&app, &headers, peer.ip());
    // Counted separately from the sign-in page: a batch install started with a
    // stale key is a misconfigured deploy rather than an attack on the panel, and
    // a shared counter would lock the operator out of their own hub for LOCKOUT.
    if app.registrations.locked(ip) {
        return answer(StatusCode::TOO_MANY_REQUESTS, "too many attempts, try again later");
    }
    // A rerun on a registered machine sends the token it already holds and
    // receives it back while that token still opens a node, so the rerun adds no
    // second node. Ahead of the window and the key: it creates nothing, returns
    // only what the caller already holds -- which the 401 of the agent handshake
    // reveals as well -- and must still succeed once the window has closed. A
    // token whose node was deleted, or which was reissued, falls through.
    if let Some(held) = headers.get(HELD_TOKEN).and_then(|v| v.to_str().ok()).filter(|t| !t.is_empty()) {
        match app.db.node_by_token(held) {
            Ok(Some(_)) => return held.to_owned().into_response(),
            Ok(None) => {}
            // Read as "no node", a failed lookup would register a second node for
            // a machine whose node is intact.
            Err(e) => return script_fail(e),
        }
    }
    // One answer for both "no window is open" and "that key is wrong": the
    // difference is only useful to someone who has neither.
    let closed = || {
        answer(StatusCode::FORBIDDEN, "registration is closed; open a new window from the panel's node list")
    };
    let until = register_until(&app);
    let Some(key) = app.db.get("register_key").filter(|k| !k.is_empty() && Utc::now().timestamp() < until)
    else {
        return closed();
    };
    if agent_ws::bearer(&headers) != Some(key.as_str()) {
        // Only an incorrect key counts against the address. With the window
        // closed there is no secret to guess, and counting then would let anyone
        // lock an address they name in `X-Forwarded-For` out of the sign-in page.
        app.registrations.record_failure(ip);
        return closed();
    }
    match app.db.nodes_created_since(until - REGISTER_WINDOW) {
        Ok(n) if n >= REGISTER_LIMIT => {
            return answer(
                StatusCode::FORBIDDEN,
                "this window has registered enough nodes; open a new window from the panel's node list",
            )
        }
        Err(e) => return script_fail(e),
        Ok(_) => {}
    }

    // The name comes from a machine not yet vouched for, and is shown to every
    // visitor until the operator renames the node. Trimmed last: a name made of
    // markup characters and spaces must come out empty, not as a blank, and
    // `create_node` names an empty one after the node's id.
    let name = db::reported_text(&name, MAX_NAME).trim().to_owned();
    // Field defaults live in `Node`'s serde attributes and nowhere else.
    // `Node::default()` is a different set of values -- private, reset day 0 --
    // and a node registered here must match one added through the panel.
    let node = match serde_json::from_value::<Node>(json!({ "name": name })) {
        Ok(node) => node,
        Err(e) => return script_fail(e),
    };
    let token = random_token();
    match app.db.create_node(&node, &token) {
        Ok(_) => {
            app.registrations.clear(ip);
            invalidate_snapshot(&app);
            token.into_response()
        }
        Err(e) => script_fail(e),
    }
}

/// [`fail`] for `install.sh`, whose own output is English and which prints the
/// reply in one line after its own.
fn script_fail(e: impl Into<anyhow::Error>) -> Response {
    warn!("registration failed: {:#}", e.into());
    answer(StatusCode::INTERNAL_SERVER_ERROR, "the hub hit an internal error; its log has the details")
}

/// Opens a registration window with a fresh key. Any previous key stops working
/// the moment this returns.
pub async fn open_register(_: Admin, State(app): State<Shared>, headers: HeaderMap) -> Response {
    if let Err(refusal) = provisioning_allowed(&app, &headers) {
        return answer(StatusCode::FORBIDDEN, refusal);
    }
    let key = random_token();
    let until = (Utc::now().timestamp() + REGISTER_WINDOW).to_string();
    match app.db.set("register_key", &key).and_then(|()| app.db.set("register_until", &until)) {
        Ok(()) => Json(json!({"register_key": key, "register_left": REGISTER_WINDOW})).into_response(),
        Err(e) => fail(e),
    }
}

/// Seconds the registration window has left, 0 when none is open. What the
/// panel counts down from, rather than the stored deadline: compared with the
/// browser's clock, a deadline is off by as much as that clock is. A browser
/// eight hours fast -- Windows and Linux sharing a machine in UTC+8 -- would hide
/// the command of an open window, and one eight hours slow would show it for
/// eight hours after the key stopped working.
pub(super) fn register_left(app: &App) -> i64 {
    (register_until(app) - Utc::now().timestamp()).clamp(0, REGISTER_WINDOW)
}

/// The stored deadline, 0 when none was ever set. One reading for the gate in
/// `agent_register` and the panel's countdown, so the two cannot disagree.
fn register_until(app: &App) -> i64 {
    app.db.get("register_until").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0)
}

/// Closes the window early, before the hour elapses.
pub async fn close_register(_: Admin, State(app): State<Shared>) -> Response {
    match app.db.set("register_key", "").and_then(|()| app.db.set("register_until", "0")) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => fail(e),
    }
}

pub async fn update_node(
    _: Admin,
    State(app): State<Shared>,
    Path(id): Path<i64>,
    body: Result<Json<NodePatch>, JsonRejection>,
) -> Response {
    let Ok(Json(mut node)) = body else { return bad("节点数据格式不对") };
    if let Some(message) = patch_error(&mut node) {
        return bad(message);
    }
    match app.db.update_node(id, &node) {
        Ok(true) => {
            invalidate_snapshot(&app);
            Json(json!({"ok": true})).into_response()
        }
        Ok(false) => no_such_node(),
        Err(e) => fail(e),
    }
}

/// What a batch may set: the settings the panel applies across a selection.
/// An allowlist, so a field added to [`NodePatch`] later, possibly one that
/// describes a single machine, is refused here until it is listed.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BatchPatch {
    group: Option<String>,
    notify: Option<bool>,
}

#[derive(Deserialize)]
pub struct NodeBatch {
    pub(super) ids: Vec<i64>,
    #[serde(default)]
    pub(super) patch: BatchPatch,
}

/// Applies one patch to every selected node, all or none.
pub async fn update_nodes(
    _: Admin,
    State(app): State<Shared>,
    body: Result<Json<NodeBatch>, JsonRejection>,
) -> Response {
    let Ok(Json(NodeBatch { mut ids, patch })) = body else {
        return bad("批量修改只支持分组和通知");
    };
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return bad("没有选中节点");
    }
    let mut patch = NodePatch { group: patch.group, notify: patch.notify, ..Default::default() };
    if let Some(message) = patch_error(&mut patch) {
        return bad(message);
    }
    match app.db.update_nodes(&ids, &patch) {
        Ok(true) => {
            invalidate_snapshot(&app);
            Json(json!({"updated": ids.len()})).into_response()
        }
        Ok(false) => answer(StatusCode::NOT_FOUND, "有节点已被删除，没有做任何修改；刷新后重新选择"),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
pub struct Order {
    pub(super) ids: Vec<i64>,
}

/// The list must name every node exactly once, checked inside the transaction
/// that renumbers rather than here: re-reading the node list first would only
/// race the write it guards.
pub async fn reorder_nodes(_: Admin, State(app): State<Shared>, Json(order): Json<Order>) -> Response {
    match app.db.reorder_nodes(&order.ids) {
        Ok(()) => {
            invalidate_snapshot(&app);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => fail(e),
    }
}

pub async fn delete_node(_: Admin, State(app): State<Shared>, Path(id): Path<i64>) -> Response {
    match app.db.delete_node(id) {
        Ok(true) => {}
        Ok(false) => return no_such_node(),
        Err(e) => return fail(e),
    }
    // The token is checked only at the handshake, so deleting the row does not
    // end a connection already open on it; dropping the sender does. Dropped
    // after the delete, so the reconnect that follows finds no token to accept.
    // The same reasoning applies in `reset_token` below.
    app.agents.write().unwrap_or_else(|e| e.into_inner()).remove(&id);
    // Its held reading as well, which no node would book.
    app.readings.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    invalidate_snapshot(&app);
    Json(json!({"ok": true})).into_response()
}

/// Issues a fresh token, invalidating the old one immediately.
///
/// Always an explicit action: rotate a token believed to have leaked, then
/// reinstall the agent. Reading the install command does not pass through here.
pub async fn reset_token(_: Admin, State(app): State<Shared>, Path(id): Path<i64>) -> Response {
    let token = random_token();
    match app.db.reset_token(id, &token) {
        Ok(true) => {}
        Ok(false) => return no_such_node(),
        Err(e) => return fail(e),
    }
    // The token is checked only at the handshake, so a session opened with the
    // old one would continue reporting. Dropping the sender ends that loop; the
    // agent reconnects and is refused. Its own teardown leaves the entry
    // untouched, because the session tag no longer matches.
    app.agents.write().unwrap_or_else(|e| e.into_inner()).remove(&id);
    // The node shows offline at once rather than when the frame expires.
    invalidate_snapshot(&app);
    // The token alone: the panel builds the command, and one place needs to know
    // its form.
    Json(json!({"token": token})).into_response()
}

/// A node's token, for the install command the panel shows. Not part of the
/// node list, where it would share an answer with strings agents report: a
/// proxy compressing that answer would let one rogue agent guess it a character
/// at a time from the compressed length.
pub async fn node_token(_: Admin, State(app): State<Shared>, Path(id): Path<i64>) -> Response {
    match app.db.node(id) {
        Ok(Some(node)) => {
            ([(header::CACHE_CONTROL, "no-store")], Json(json!({"token": node.token}))).into_response()
        }
        Ok(None) => no_such_node(),
        Err(e) => fail(e),
    }
}

pub async fn patch_traffic(
    _: Admin,
    State(app): State<Shared>,
    Path(id): Path<i64>,
    Json(p): Json<TrafficPatch>,
) -> Response {
    if [p.total_rx, p.total_tx, p.month_rx, p.month_tx].into_iter().flatten().any(|v| v < 0) {
        return bad("流量不能是负数");
    }
    match app.db.set_traffic(id, &p) {
        Ok(true) => {
            invalidate_snapshot(&app);
            Json(json!({"ok": true})).into_response()
        }
        Ok(false) => no_such_node(),
        Err(e) => fail(e),
    }
}
