//! Nodes: stored configuration, reported facts and order.

use super::*;

/// One node's stored configuration and last known facts.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Node {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    #[serde(default = "yes")]
    pub public: bool,
    #[serde(default)]
    pub sort: i64,
    #[serde(default)]
    pub price: f64,
    #[serde(default = "usd")]
    pub currency: String,
    #[serde(default = "monthly")]
    pub billing_cycle: String,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub remark: String,
    /// Set in the panel for visitors to read, beside `remark`, which they never
    /// see. Empty is none.
    #[serde(default)]
    pub public_remark: String,
    /// Monthly allowance in bytes; 0 means unmetered.
    #[serde(default)]
    pub traffic_limit: i64,
    /// How the allowance is counted: sum, max, up or down.
    #[serde(default = "sum")]
    pub traffic_mode: String,
    #[serde(default = "one")]
    pub traffic_reset_day: u32,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub kernel: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub virt: String,
    #[serde(default)]
    pub cpu_name: String,
    #[serde(default)]
    pub cpu_cores: i64,
    #[serde(default)]
    pub mem_total: i64,
    #[serde(default)]
    pub swap_total: i64,
    #[serde(default)]
    pub disk_total: i64,
    #[serde(default)]
    pub agent_version: String,
    #[serde(default)]
    pub ip: String,
    /// Likewise the address the hub saw, in the family `ip` is not; see the
    /// column.
    #[serde(default)]
    pub ip_other: String,
    /// Reported by the agent from its own interfaces, unlike `ip`, which is
    /// merely the address the agent's connection originated from.
    #[serde(default)]
    pub ipv4: String,
    #[serde(default)]
    pub ipv6: String,
    /// ISO 3166-1 alpha-2, uppercase, or empty when unknown; see
    /// `agent_ws::country_source` for the address it is looked up from. Public:
    /// it appears on the status page beside the node's name.
    #[serde(default)]
    pub country: String,
    /// Set in the panel: two uppercase letters, or empty for the looked-up
    /// `country`. What the status page shows is this when present.
    #[serde(default)]
    pub country_pin: String,
    /// Set in the panel; empty is ungrouped. Public, like the name.
    #[serde(default)]
    pub group: String,
    /// Set in the panel, in canonical form, for what neither agent nor hub can
    /// know: the home line behind a transparent proxy, or which of several public
    /// addresses to show. Each replaces the address shown for its family; empty
    /// is automatic. Panel only, like `ip`.
    #[serde(default)]
    pub ipv4_pin: String,
    #[serde(default)]
    pub ipv6_pin: String,
    /// Unix seconds of the node's last report, written once a minute alongside
    /// the metric row. Zero for a node that has never reported.
    #[serde(default)]
    pub last_seen: i64,
    /// Whether going offline and coming back are announced. See `notify`.
    #[serde(default)]
    pub notify: bool,
    #[serde(default)]
    pub down_since: i64,
    /// What the agent authenticates with. Readable so the panel can display an
    /// install command on demand; it never leaves the admin view.
    #[serde(default)]
    pub token: String,
}

fn yes() -> bool {
    true
}

/// Omitted settings stay unchanged. An explicit null clears the expiry date.
/// The position in the list is not among them: it changes only through
/// `reorder_nodes`, which takes the whole order at once.
#[derive(Deserialize, Default)]
pub struct NodePatch {
    pub name: Option<String>,
    pub public: Option<bool>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub billing_cycle: Option<String>,
    #[serde(default, deserialize_with = "expiry_patch")]
    pub expires_at: Option<Option<String>>,
    pub remark: Option<String>,
    pub public_remark: Option<String>,
    pub traffic_limit: Option<i64>,
    pub traffic_mode: Option<String>,
    pub traffic_reset_day: Option<u32>,
    pub notify: Option<bool>,
    pub country_pin: Option<String>,
    pub ipv4_pin: Option<String>,
    pub ipv6_pin: Option<String>,
    pub group: Option<String>,
}

fn expiry_patch<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

impl Db {
    // ---- nodes ----

    pub fn nodes(&self) -> Result<Vec<Node>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT * FROM node ORDER BY sort, id")?;
        let rows = stmt.query_map([], |r| Ok(row_to_node(r)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn node(&self, id: i64) -> Result<Option<Node>> {
        Ok(self
            .conn()
            .query_row("SELECT * FROM node WHERE id = ?1", [id], |r| Ok(row_to_node(r)))
            .optional()?)
    }

    /// Creates a node and returns its id.
    ///
    /// One transaction: `accumulate` reads the `traffic` row on every report, so
    /// a node lacking one cannot report. The node also takes every `auto_join`
    /// probe here, at most [`Self::MAX_PROBES_PER_NODE`] rows, a limit
    /// `save_ping_task` enforces.
    pub fn create_node(&self, n: &Node, token: &str) -> Result<i64> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let id = next_id(&tx, "node")?;
        tx.execute(
            // A new node belongs at the end. The caller sends sort 0, which would
            // tie with whatever the last reorder placed first.
            "INSERT INTO node (id, name, token, sort, public, price, currency, billing_cycle,
                               expires_at, remark, traffic_limit, traffic_mode, traffic_reset_day, created_at,
                               group_name)
             VALUES (?14,?1,?2,(SELECT COALESCE(MAX(sort),-1)+1 FROM node),?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                n.name,
                token,
                n.public,
                n.price,
                n.currency,
                n.billing_cycle,
                n.expires_at,
                n.remark,
                n.traffic_limit,
                n.traffic_mode,
                n.traffic_reset_day,
                Utc::now().timestamp(),
                n.group,
                id
            ],
        )?;
        // Only a registration without `--name` arrives unnamed; the panel
        // requires a name.
        if n.name.is_empty() {
            tx.execute("UPDATE node SET name = ?1 WHERE id = ?2", params![format!("节点 {id}"), id])?;
        }
        tx.execute("INSERT INTO traffic (node_id) VALUES (?1)", [id])?;
        tx.execute(
            "INSERT INTO ping_node (task_id, node_id) SELECT id, ?1 FROM ping_task WHERE auto_join",
            [id],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// How many nodes were created at or after `ts`. Bounds what one registration
    /// window can add; see `api::REGISTER_LIMIT`.
    pub fn nodes_created_since(&self, ts: i64) -> Result<i64> {
        Ok(self.conn().query_row("SELECT COUNT(*) FROM node WHERE created_at >= ?1", [ts], |r| r.get(0))?)
    }

    /// Records when the node last reported, with the capacities that report
    /// carried. Written with each metric row and once more as the session ends,
    /// so an offline node shows the disk it last had rather than the one it
    /// connected with. A capacity absent from `metrics` keeps its stored value.
    pub fn touch_seen(&self, id: i64, ts: i64, metrics: &serde_json::Value) -> Result<()> {
        let n = |k: &str| metrics.get(k).and_then(serde_json::Value::as_i64);
        self.conn()
            .prepare_cached(
                "UPDATE node SET last_seen=?2, mem_total=COALESCE(?3,mem_total),
                                 swap_total=COALESCE(?4,swap_total), disk_total=COALESCE(?5,disk_total)
                 WHERE id=?1",
            )?
            .execute(params![id, ts, n("mem_total"), n("swap_total"), n("disk_total")])?;
        Ok(())
    }

    /// False when no node has this id.
    pub fn update_node(&self, id: i64, n: &NodePatch) -> Result<bool> {
        self.update_nodes(&[id], n)
    }

    /// Applies one patch to every node in `ids` in a single transaction. False,
    /// with nothing written, when any of them no longer exists: a batch applied
    /// to part of what was selected would leave the panel to work out which part.
    pub fn update_nodes(&self, ids: &[i64], n: &NodePatch) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut update = tx.prepare(
                "UPDATE node SET name=COALESCE(?2,name), public=COALESCE(?3,public),
                                 price=COALESCE(?4,price), currency=COALESCE(?5,currency),
                                 billing_cycle=COALESCE(?6,billing_cycle),
                                 expires_at=CASE WHEN ?7 THEN ?8 ELSE expires_at END,
                                 remark=COALESCE(?9,remark), traffic_limit=COALESCE(?10,traffic_limit),
                                 traffic_mode=COALESCE(?11,traffic_mode),
                                 traffic_reset_day=COALESCE(?12,traffic_reset_day),
                                 notify=COALESCE(?13,notify), country_pin=COALESCE(?14,country_pin),
                                 ipv4_pin=COALESCE(?15,ipv4_pin), ipv6_pin=COALESCE(?16,ipv6_pin),
                                 group_name=COALESCE(?17,group_name),
                                 public_remark=COALESCE(?18,public_remark)
                 WHERE id=?1",
            )?;
            for id in ids {
                let found = update.execute(params![
                    id,
                    n.name,
                    n.public,
                    n.price,
                    n.currency,
                    n.billing_cycle,
                    n.expires_at.is_some(),
                    n.expires_at.as_ref().and_then(|v| v.as_deref()),
                    n.remark,
                    n.traffic_limit,
                    n.traffic_mode,
                    n.traffic_reset_day,
                    n.notify,
                    n.country_pin,
                    n.ipv4_pin,
                    n.ipv6_pin,
                    n.group,
                    n.public_remark
                ])?;
                // Dropping the transaction uncommitted rolls back the nodes
                // already updated.
                if found == 0 {
                    return Ok(false);
                }
            }
        }
        tx.commit()?;
        Ok(true)
    }

    pub fn set_expiry(&self, id: i64, date: &str) -> Result<()> {
        self.conn().execute("UPDATE node SET expires_at=?2 WHERE id=?1", params![id, date])?;
        Ok(())
    }

    pub fn set_down_since(&self, id: i64, ts: i64) -> Result<()> {
        self.conn().execute("UPDATE node SET down_since=?2 WHERE id=?1", params![id, ts])?;
        Ok(())
    }

    pub fn reorder_nodes(&self, ids: &[i64]) -> Result<()> {
        self.reorder("node", ids)
    }

    pub fn reorder_ping_tasks(&self, ids: &[i64]) -> Result<()> {
        self.reorder("ping_task", ids)
    }

    /// Renumbers `sort` from a list that must name every row exactly once, so a
    /// tab that missed an insert or a delete cannot renumber around it.
    fn reorder(&self, table: &str, ids: &[i64]) -> Result<()> {
        let unique: HashSet<_> = ids.iter().collect();
        if unique.len() != ids.len() {
            refuse!("排序里有重复的条目");
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let count: i64 = tx.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
        if count as usize != ids.len() {
            refuse!("列表已在别处改动，刷新后再排序");
        }
        let sql = format!("UPDATE {table} SET sort=?2 WHERE id=?1");
        for (sort, id) in ids.iter().enumerate() {
            if tx.execute(&sql, params![id, sort as i64])? != 1 {
                refuse!("列表已在别处改动，刷新后再排序");
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// False when no node has this id.
    pub fn delete_node(&self, id: i64) -> Result<bool> {
        let conn = self.conn();
        let managed: i64 = conn.query_row(
            "SELECT count(*) FROM managed_node WHERE node=?1 AND credential!=''",
            [id],
            |r| r.get(0),
        )?;
        if managed > 0 {
            refuse!("请先解绑该节点的管理组件，业务服务不会自动卸载");
        }
        let uninstalling: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM managed_lifecycle WHERE node=?1 AND status IN ('running','paused','cleaning','finalizing'))",
            [id], |r| r.get(0),
        )?;
        if uninstalling {
            refuse!("节点卸载尚未确认完成，请先恢复并完成清理");
        }
        // `ping_record` carries no foreign key -- it is WITHOUT ROWID and keyed
        // for the chart query -- so it is cleared explicitly; the other history
        // tables cascade.
        conn.execute("DELETE FROM ping_record WHERE node_id = ?1", [id])?;
        Ok(conn.execute("DELETE FROM node WHERE id = ?1", [id])? > 0)
    }

    /// Replaces a node's token, which immediately locks out the old one. False
    /// when no node has this id.
    pub fn reset_token(&self, id: i64, token: &str) -> Result<bool> {
        Ok(self.conn().execute("UPDATE node SET token=?2 WHERE id=?1", params![id, token])? > 0)
    }

    pub fn node_by_token(&self, token: &str) -> Result<Option<i64>> {
        Ok(self.conn().query_row("SELECT id FROM node WHERE token = ?1", [token], |r| r.get(0)).optional()?)
    }

    /// Stores the facts an agent sends on connect and again whenever they change,
    /// and reports whether the node still requires a country lookup for `source`,
    /// the address `agent_ws::country_source` chose. An empty `source` has no
    /// country and is never owed one. Capacities are not among them, even where an agent still
    /// sends them: they change with every mount, and [`Db::touch_seen`] keeps
    /// them from the reports.
    ///
    /// A new source invalidates the previous country, so the two move together in
    /// one statement: `SET` reads the row as it was, so the comparison is against
    /// the stored address rather than the one being written. The pair replaced
    /// moves to `country_prev_ip` / `country_prev` if it had an answer, and a
    /// source equal to that address takes its answer back without a lookup.
    pub fn save_facts(&self, id: i64, f: &serde_json::Value, ip: &str, source: &str) -> Result<bool> {
        // Six of these -- os, kernel, arch, virt, cpu_name, agent_version -- go
        // straight into the anonymous public frame, which is rebuilt and pushed to
        // every viewer every two seconds, so without a ceiling one node would
        // determine that frame's size. 128 rather than 64: a real PRETTY_NAME runs
        // to about 60 characters and a CPU model to about 50.
        let s = |k: &str| reported_text(f.get(k).and_then(|v| v.as_str()).unwrap_or(""), 128);
        let n = |k: &str| f.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        let conn = self.conn();
        conn.execute(
            "UPDATE node SET hostname=?2, os=?3, kernel=?4, arch=?5, virt=?6, cpu_name=?7,
                             cpu_cores=?8, agent_version=?9, ip=?10, ipv4=?11, ipv6=?12, country_ip=?13,
                             country=CASE WHEN country_ip=?13 THEN country
                                          WHEN country_prev_ip=?13 THEN country_prev ELSE '' END,
                             country_prev_ip=CASE WHEN country_ip=?13 OR country='' THEN country_prev_ip
                                                  ELSE country_ip END,
                             country_prev=CASE WHEN country_ip=?13 OR country='' THEN country_prev
                                               ELSE country END
             WHERE id=?1",
            params![
                id,
                s("hostname"),
                s("os"),
                s("kernel"),
                s("arch"),
                s("virt"),
                s("cpu_name"),
                n("cpu_cores"),
                s("agent_version"),
                ip,
                s("ipv4"),
                s("ipv6"),
                source
            ],
        )?;
        let blank: bool = conn.query_row("SELECT country = '' FROM node WHERE id=?1", [id], |r| r.get(0))?;
        Ok(blank && !source.is_empty())
    }

    /// Records where the hub saw a node over the address family its session is
    /// not in. Kept apart from `save_facts`, which an agent's hello rewrites: the
    /// two arrive on different connections, in no fixed order.
    pub fn set_ip_other(&self, id: i64, ip: &str) -> Result<()> {
        // Unchanged is not written: a reconnecting agent, or a token holder
        // repeating the request, would otherwise cost a write transaction each.
        self.conn().execute("UPDATE node SET ip_other=?2 WHERE id=?1 AND ip_other<>?2", params![id, ip])?;
        Ok(())
    }

    /// Whether the node still lacks a country for `source`: false once a lookup
    /// has landed, or once the node has moved to another address.
    pub fn country_owed(&self, id: i64, source: &str) -> Result<bool> {
        let owed = self
            .conn()
            .query_row(
                "SELECT country = '' FROM node WHERE id=?1 AND country_ip=?2",
                params![id, source],
                |r| r.get(0),
            )
            .optional()?;
        Ok(owed.unwrap_or(false))
    }

    /// Records the country a lookup returned, unless the node moved to another
    /// address while the lookup was outstanding. This is the same rule
    /// `save_facts` encodes in its `CASE`: the country belongs to the address it
    /// was asked about, so a late answer for an address the node has left is not
    /// an answer about the node. Kept apart from the panel's own writes:
    /// `update_node` never touches this column.
    pub fn set_country(&self, id: i64, cc: &str, source: &str) -> Result<()> {
        self.conn()
            .execute("UPDATE node SET country=?2 WHERE id=?1 AND country_ip=?3", params![id, cc, source])?;
        Ok(())
    }
}

fn row_to_node(r: &rusqlite::Row<'_>) -> Node {
    let s = |i: &str| r.get::<_, String>(i).unwrap_or_default();
    let n = |i: &str| r.get::<_, i64>(i).unwrap_or(0);
    Node {
        id: n("id"),
        name: s("name"),
        public: r.get::<_, bool>("public").unwrap_or(true),
        sort: n("sort"),
        price: r.get::<_, f64>("price").unwrap_or(0.0),
        currency: s("currency"),
        billing_cycle: s("billing_cycle"),
        expires_at: r.get::<_, Option<String>>("expires_at").unwrap_or(None),
        remark: s("remark"),
        public_remark: s("public_remark"),
        traffic_limit: n("traffic_limit"),
        traffic_mode: s("traffic_mode"),
        traffic_reset_day: n("traffic_reset_day") as u32,
        hostname: s("hostname"),
        os: s("os"),
        kernel: s("kernel"),
        arch: s("arch"),
        virt: s("virt"),
        cpu_name: s("cpu_name"),
        cpu_cores: n("cpu_cores"),
        mem_total: n("mem_total"),
        swap_total: n("swap_total"),
        disk_total: n("disk_total"),
        agent_version: s("agent_version"),
        ip: s("ip"),
        ip_other: s("ip_other"),
        ipv4: s("ipv4"),
        ipv6: s("ipv6"),
        country: s("country"),
        country_pin: s("country_pin"),
        group: s("group_name"),
        ipv4_pin: s("ipv4_pin"),
        ipv6_pin: s("ipv6_pin"),
        last_seen: n("last_seen"),
        notify: n("notify") != 0,
        down_since: n("down_since"),
        token: s("token"),
    }
}
