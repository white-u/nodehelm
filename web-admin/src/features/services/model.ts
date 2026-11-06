import { api, type Node } from "../../lib/api.ts"

export type Module = "snell" | "sb" | "fw" | "pm"
export type ApplyIntent = "create" | "update"
export type Resource = { name: string; port: number; host?: string; public_port?: number; mode?: string; psk?: string; protocol?: string; sni?: string; target_host?: string; target_port?: number }
export type IpLimit = { max_ips: number; cooldown_min: number; whitelist: string[] }
export type PmPolicy = Resource & { quota_gb: number; quota_mode: "in_out" | "out_only"; reset_day: number; expiry_date: string; egress_mbps: number; ip_limit?: IpLimit }
export type PmUsage = { rx: number; tx: number; sampled_at: number; period_start: string; alert: string; ip_count?: number; ip_level?: number; ip_list?: string[] }
/** The PM form: the IP limit flattened, 0 meaning off, and the whitelist as typed. */
export type PmDraft = Omit<PmPolicy, "ip_limit"> & { max_ips: number; cooldown_min: number; whitelist: string }
export type CoreAction = "core_upgrade" | "core_rollback" | "core_uninstall"
export type Core = { version: string | null; sha256: string | null; previous_version: string | null; candidates: string[] }
export type Snapshot = { core?: Core; usage?: Record<string, PmUsage>; runtime?: Record<string, string>; installed: boolean; blocked: boolean; revision: string; resources: Resource[] }
export type PortRow = { port: number; quota_gb: number; quota_mode: string; reset_day: number; comment: string; expiry_date: string; burst_enabled: boolean; state: string; rx?: number; tx?: number; last_sample?: number }
export type RootSnapshot = Snapshot & { modules?: Record<string, Snapshot>; port_monitor?: { status: string; rows: PortRow[] } }
export type Lifecycle = { id: string; scope: Module | "all"; status: string; underlying_status?: string; stage: string; message: string; attempt: number }
export const lifecycleActive = (job?: Lifecycle) => !!job && !["succeeded", "cancelled", "failed", "revoked"].includes(job.status)
export const lifecycleLabels: Record<string, string> = { running: "正在卸载", paused: "卸载已暂停", interrupted: "节点离线，结果未确认", cleaning: "正在清理管理组件", finalizing: "正在确认清理结果", succeeded: "卸载已确认完成", revoked: "恢复备份后原卸载授权已失效" }
export type Status = { lifecycle?: Lifecycle; compatible?: boolean; abandonable?: boolean; component_current?: boolean; bound: boolean; online?: boolean; last_seen?: number; snapshot?: RootSnapshot; operations?: { id: string; status: string; message: string; created: number; module?: string; action?: string }[] }
export const names: Record<Module, string> = { snell: "Snell", sb: "SB", fw: "FW 转发", pm: "PM 端口统计" }
export const runtimeLabels: Record<string, string> = { active: "运行中", inactive: "已停止", failed: "服务失败", activating: "启动中", deactivating: "停止中", unknown: "运行状态未知" }
export const actionLabels: Record<string, string> = { install: "安装", apply: "保存应用", delete: "删除", reset: "重置用量", check: "目标 TCP 检查", core_upgrade: "升级核心", core_rollback: "回退核心", core_uninstall: "卸载模块" }
export const labels: Record<string, string> = { checked: "目标检查完成", queued: "等待发送", delivered: "执行中／待核实", succeeded: "已应用", rolled_back: "操作未完成", rejected: "请求被拒绝", cancelled: "未发送，已取消", expired: "过期未执行" }
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

export type NodeStatus = { status?: Status; error?: string; loading: boolean; updatedAt?: number }
export const modules = Object.keys(names) as Module[]
export function pmDraft({ ip_limit, ...rule }: PmPolicy): PmDraft {
  return { ...rule, max_ips: ip_limit?.max_ips ?? 0, cooldown_min: ip_limit?.cooldown_min ?? 30, whitelist: ip_limit?.whitelist.join("\n") ?? "" }
}
/** An IP limit of 0 is left out, so the rule keeps the shape older components accept. */
export function pmPayload({ max_ips, cooldown_min, whitelist, ...rule }: PmDraft): PmPolicy {
  return max_ips ? { ...rule, ip_limit: { max_ips, cooldown_min, whitelist: whitelist.split(/[\s,，]+/).filter(Boolean) } } : rule
}
export function moduleView(status: Status | undefined, module: Module) {
  return module === "snell" ? status?.snapshot : status?.snapshot?.modules?.[module]
}

export const pmStatus: Record<string, string> = { not_installed: "未检测到原 PM 配置", unavailable: "原 PM 配置不可读取或版本不兼容", busy: "原 PM 正在更新，等待下次读取", ready: "已读取原 PM 配置" }
export const pmAlerts: Record<string, string> = { normal: "正常", quota_warning: "用量达到配额的 80%", quota_reached: "已达到配额", expired: "已到期，端口流量被阻断", legacy_conflict: "原 PM 已管理此端口，受管规则已暂停", legacy_pending: "发现原 PM 管理此端口，等待暂停受管规则", maintenance_pending: "日期策略等待定时任务应用" }
export const gib = (bytes: number) => (bytes / 1073741824).toFixed(3)

