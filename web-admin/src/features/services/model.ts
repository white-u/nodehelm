import { api, type Node } from "../../lib/api.ts"

export type Module = "snell" | "sb" | "fw" | "pm"
export type ApplyIntent = "create" | "update"
export type Resource = { name: string; port: number; host?: string; public_port?: number; mode?: string; psk?: string; protocol?: string; sni?: string; target_host?: string; target_port?: number }
/** The original PM's limit: over `max_ips` warns, over + 2 alarms, past + 3 blocks with `block`. */
export type IpLimit = { max_ips: number; cooldown_min: number; whitelist: string[]; block?: true }
export type PmPolicy = Resource & { quota_gb: number; quota_mode: "in_out" | "out_only"; reset_day: number; expiry_date: string; egress_mbps: number; ip_limit?: IpLimit }
export type PmUsage = { rx: number; tx: number; sampled_at: number; period_start: string; alert: string; ip_count?: number; ip_level?: number; ip_list?: string[] }
/** The PM form: the IP limit flattened as the count that warns (the limit + 1),
 *  0 meaning off, and the whitelist as typed. */
export type PmDraft = Omit<PmPolicy, "ip_limit"> & { max_ips: number; cooldown_min: number; whitelist: string; block: boolean }
export type CoreAction = "core_upgrade" | "core_rollback" | "core_uninstall"
export type Core = { version: string | null; sha256: string | null; previous_version: string | null; candidates: string[] }
export type Snapshot = { core?: Core; usage?: Record<string, PmUsage>; runtime?: Record<string, string>; installed: boolean; blocked: boolean; revision: string; resources: Resource[] }
export type RootSnapshot = Snapshot & { modules?: Record<string, Snapshot> }
export type Lifecycle = { id: string; scope: Module | "all"; status: string; underlying_status?: string; stage: string; message: string; attempt: number }
export const lifecycleActive = (job?: Lifecycle) => !!job && !["succeeded", "cancelled", "failed", "revoked"].includes(job.status)
export const lifecycleLabels: Record<string, string> = { running: "正在卸载", paused: "卸载已暂停", interrupted: "节点离线，结果未确认", cleaning: "正在清理管理组件", finalizing: "正在确认清理结果", succeeded: "卸载已确认完成", revoked: "恢复备份后原卸载授权已失效" }
export type Status = { lifecycle?: Lifecycle; compatible?: boolean; abandonable?: boolean; component_current?: boolean; bound: boolean; online?: boolean; last_seen?: number; snapshot?: RootSnapshot; operations?: { id: string; status: string; message: string; created: number; module?: string; action?: string; after?: string }[] }
export const names: Record<Module, string> = { snell: "Snell", sb: "singbox", fw: "realm 转发", pm: "端口流量统计" }
export const runtimeLabels: Record<string, string> = { active: "运行中", inactive: "已停止", failed: "服务失败", activating: "启动中", deactivating: "停止中", unknown: "运行状态未知" }
export const runtimeDots: Record<string, string> = { active: "bg-emerald-500", activating: "bg-amber-500", deactivating: "bg-amber-500", failed: "bg-destructive" }
/** How long a success note stays before it clears itself. */
export const feedbackSeconds = 6
export const actionLabels: Record<string, string> = { install: "安装", apply: "保存应用", delete: "删除", reset: "重置用量", check: "目标 TCP 检查", core_upgrade: "升级核心", core_rollback: "回退核心", core_uninstall: "卸载模块" }
export const labels: Record<string, string> = { checked: "目标检查完成", queued: "等待发送", delivered: "执行中／待核实", succeeded: "已完成", rolled_back: "操作未完成", rejected: "请求被拒绝", cancelled: "未发送，已取消", expired: "过期未执行" }
export const sniChoices = ["addons.mozilla.org", "www.microsoft.com", "www.amazon.com", "swdist.apple.com", "updates.cdn-apple.com"]
export function quote(s: string) { return "'" + s.replaceAll("'", "'\\''") + "'" }
export function defaultHost(node?: Node): string {
  // A present effective-address list is authoritative, including IPv6-only nodes.
  const candidates = node?.addresses !== undefined ? node.addresses.map(item => item.address)
    : node?.ipv4_pin ? [node.ipv4_pin] : node?.ipv4_auto ? [node.ipv4_auto] : [node?.ip]
  return candidates.find(value => {
    if (!value || !/^\d{1,3}(\.\d{1,3}){3}$/.test(value)) return false
    const [a, b, c, d] = value.split(".").map(Number)
    return [a, b, c, d].every(part => part <= 255) && a > 0 && a < 224 && a !== 10 && a !== 127
      && !(a === 169 && b === 254) && !(a === 172 && b >= 16 && b <= 31) && !(a === 192 && b === 168)
      && !(a === 100 && b >= 64 && b <= 127) && !(a === 198 && (b === 18 || b === 19))
  }) ?? ""
}
export function newResource(module: Module, host: string): Resource {
  if (module === "fw") return { name: "转发", port: 10086, target_host: "", target_port: 10086 }
  if (module === "sb") return { name: "", port: 443, host, public_port: 443, protocol: "reality", sni: "", psk: "" }
  return { name: "", port: 10086, host, public_port: 10086, mode: "default", psk: "" }
}
export function editable(module: Module, item: Resource): Resource {
  // Do not round-trip snapshot-only public keys or metadata into a write request.
  if (module === "fw") return { name: item.name, port: item.port, target_host: item.target_host, target_port: item.target_port }
  const common = { name: item.name, port: item.port, host: item.host, public_port: item.public_port, psk: "" }
  return module === "sb" ? { ...common, protocol: item.protocol, sni: item.sni } : { ...common, mode: item.mode }
}

export function instancePayload(module: Module, draft: Resource): Resource {
  if (module !== "snell" && module !== "sb") return { ...draft }
  return { ...draft, name: draft.name.trim() || String(draft.port), ...(module === "sb" ? { sni: draft.protocol === "reality" ? draft.sni?.trim() ?? "" : "" } : {}) }
}

export async function readManagement<T>(path: string): Promise<T> {
  const controller = new AbortController()
  const timeout = setTimeout(() => controller.abort(), 15_000)
  try {
    return await api<T>(path, { signal: controller.signal, cache: "no-store" })
  } catch (e) {
    if (controller.signal.aborted) throw new Error("读取超时，请刷新重试")
    throw e
  } finally { clearTimeout(timeout) }
}

export type NodeStatus = { status?: Status; error?: string; loading: boolean; updatedAt?: number; refreshed?: boolean }

/** Outcome takes precedence over action: a failed deletion must never look successful. */
export function serviceFeedbackClass(status: string, action = "") {
  if (["failed", "rejected", "rolled_back", "recovery_failed"].includes(status)) return "text-destructive"
  if (["paused", "interrupted", "expired", "cancelled", "revoked", "warning"].includes(status)) return "text-amber-700 dark:text-amber-400"
  if (status === "succeeded") {
    if (["delete", "core_uninstall", "uninstall", "revoke"].includes(action)) return "text-orange-700 dark:text-orange-400"
    if (["refresh", "check"].includes(action)) return "text-blue-700 dark:text-blue-400"
    return "text-emerald-700 dark:text-emerald-400"
  }
  return "text-blue-700 dark:text-blue-400"
}
export const modules = Object.keys(names) as Module[]
export function pmDraft({ ip_limit, ...rule }: PmPolicy): PmDraft {
  return { ...rule, max_ips: ip_limit?.max_ips ?? 0, cooldown_min: ip_limit?.cooldown_min ?? 30, whitelist: ip_limit?.whitelist.join("\n") ?? "", block: !!ip_limit?.block }
}
/** A new rule: metering only, with the IP alert on at 3, 5 and blocking from the 6th. */
export function newPmDraft(port: number, name: string): PmDraft {
  return pmDraft({ port, name, quota_gb: 0, quota_mode: "in_out", reset_day: 0, expiry_date: "", egress_mbps: 0, ip_limit: { max_ips: 3, cooldown_min: 30, whitelist: [], block: true } })
}
const ipv4 = /^(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(\.(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}$/
function ipAddress(value: string) {
  if (ipv4.test(value)) return true
  if (!value.includes(":") || !/^[0-9a-fA-F:.]+$/.test(value)) return false
  try { new URL(`http://[${value}]/`); return true } catch { return false }
}
/** An IP limit of 0 is left out, so the rule keeps the shape older components
 *  accept. The whitelist is checked here, before anything is submitted: the
 *  instance form applies it only after the instance exists. */
export function pmPayload({ max_ips, cooldown_min, whitelist, block, ...rule }: PmDraft): PmPolicy {
  if (!max_ips) return rule
  if (!Number.isInteger(max_ips) || max_ips < 1 || max_ips > 65535) throw new Error("告警阈值须为 1–65535，0 关闭")
  const ips = whitelist.split(/[\s,，]+/).filter(Boolean)
  const invalid = ips.find(item => {
    const [ip, prefix, extra] = item.split("/")
    return !ipAddress(ip) || extra !== undefined || (prefix !== undefined &&
      (!/^\d{1,3}$/.test(prefix) || Number(prefix) > (ip.includes(":") ? 128 : 32)))
  })
  if (invalid) throw new Error(`IP 白名单中的 ${invalid} 不是有效的 IP 地址或 CIDR 网段`)
  if (ips.length > 32) throw new Error("IP 白名单最多 32 个")
  return { ...rule, ip_limit: { max_ips, cooldown_min, whitelist: ips, ...(block ? { block: true as const } : {}) } }
}
/** The original PM levels by count: over the limit warns, over limit + 2 alarms, over limit + 3 blocks. */
export function ipThresholds(max: number, block?: boolean) {
  return `>${max} 告警 · >${max + 2} 报警${block ? ` · ${ipBlock(max)}` : ""}`
}
export function ipBlock(max: number) { return `>${max + 3} 阻断后续连接` }
export function pmSummary(rule: PmPolicy) {
  return [rule.quota_gb ? `配额 ${rule.quota_gb} GiB` : "只计量", rule.reset_day ? `每月 ${rule.reset_day} 日重置` : "", rule.expiry_date ? `${rule.expiry_date} 到期` : "",
    rule.egress_mbps ? `出站超速 ${rule.egress_mbps} Mbps 丢包` : "", rule.ip_limit ? `IP ${ipThresholds(rule.ip_limit.max_ips, rule.ip_limit.block)}` : ""].filter(Boolean).join("／")
}
/** Why a port cannot take a managed PM rule now, or "" when it can. */
export function pmBlocked(snapshot: RootSnapshot | undefined, port: number) {
  const pm = snapshot?.modules?.pm
  if (!pm?.installed) return "启用流量统计后可同时添加"
  if (pm.blocked) return "流量统计状态需要核实"
  if (port < 1024) return "只统计 1024 以上的端口"
  if (pm.resources.some(rule => rule.port === port)) return "该端口已有流量统计规则"
  return ""
}
/** A second operation submitted once the first succeeds: PM after a new
 *  instance, or the instance after its PM rule is deleted. One per node at a
 *  time, as the hub accepts one operation per node. */
/** The second step of a two-module change, which the hub queues once the first succeeds. */
export type FollowUp = {
  module: Module; action: string; payload: object; intent?: ApplyIntent
  steps: [string, string]; done: string
  /** Kept for "添加流量统计" when the PM step does not go through. */
  draft?: PmDraft
}
export function moduleView(status: Status | undefined, module: Module) {
  return module === "snell" ? status?.snapshot : status?.snapshot?.modules?.[module]
}

export const pmAlerts: Record<string, string> = { normal: "正常", quota_warning: "配额预警", quota_reached: "配额已满", expired: "已到期 · 已阻断", maintenance_pending: "策略待应用" }
export const gib = (bytes: number) => (bytes / 1073741824).toFixed(3)

export type PmTrafficHour = [number, number, number]
export type PmTrafficBar = { key: string; label: string; rx: number; tx: number; state: "recorded" | "missing" | "future"; partial?: boolean }
/** UTC matches PM billing. Missing rows are unknown, never invented zero readings. */
export function pmTrafficBars(hours: PmTrafficHour[], now: number, day?: string): PmTrafficBar[] {
  const hourNow = Math.floor(now / 3600) * 3600
  const today = Math.floor(now / 86400) * 86400
  const start = day ? Date.parse(`${day}T00:00:00Z`) / 1000 : today - 6 * 86400
  const step = day ? 3600 : 86400
  return Array.from({ length: day ? 24 : 7 }, (_, index) => {
    const at = start + index * step
    const date = new Date(at * 1000)
    const own = hours.filter(([time]) => time >= at && time < at + step && time <= hourNow)
    return {
      key: day ? String(index) : date.toISOString().slice(0, 10),
      label: day ? `${String(index).padStart(2, "0")}:00` : `${date.getUTCMonth() + 1}/${date.getUTCDate()}`,
      rx: own.reduce((sum, row) => sum + row[1], 0), tx: own.reduce((sum, row) => sum + row[2], 0),
      state: at > now ? "future" : own.length ? "recorded" : "missing",
      partial: at <= now && at + step > now,
    }
  })
}

