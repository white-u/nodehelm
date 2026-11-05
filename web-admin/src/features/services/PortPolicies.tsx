import { useLayoutEffect, useState } from "react"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { type ApplyIntent, type PmPolicy, type RootSnapshot, type Snapshot, confirmDiscard, pmStatus, pmAlerts, gib } from "./model"

export function PmPanel({ node, snapshot, view, writable, busy, onDirtyChange, run, submit }: {
  node: string
  snapshot?: RootSnapshot
  view?: Snapshot
  writable: boolean
  busy: boolean
  onDirtyChange: (dirty: boolean) => void
  run: (action: () => Promise<void>) => Promise<void>
  submit: (action: string, payload: object, revision?: string, intent?: ApplyIntent) => Promise<void>
}) {
  const [form, setForm] = useState<PmPolicy | null>(null)
  const [revision, setRevision] = useState("")
  const [editing, setEditing] = useState(false)
  const [initialForm, setInitialForm] = useState("")
  const [draftNotice, setDraftNotice] = useState("")
  const dirty = form !== null && JSON.stringify(form) !== initialForm
  const draftStale = form !== null && revision !== view?.revision
  useLayoutEffect(() => { onDirtyChange(dirty) }, [dirty, onDirtyChange])
  const legacy = snapshot?.port_monitor
  const rules = view?.resources as PmPolicy[] | undefined
  const candidates = [snapshot, snapshot?.modules?.sb, snapshot?.modules?.fw].flatMap(s =>
    s?.resources.filter(r => r.port >= 1024 && s.runtime?.[String(r.port)] === "active"
      && !rules?.some(rule => rule.port === r.port) && !legacy?.rows.some(row => row.port === r.port)) ?? [])
  const legacyReadable = legacy?.status === "ready" || legacy?.status === "not_installed"

  function loadDraft(item?: PmPolicy) {
    if (!item && !candidates.length) return
    setEditing(!!item)
    setRevision(view?.revision ?? "")
    const next = item ?? { port: candidates[0].port, name: "端口监控", quota_gb: 0, quota_mode: "in_out" as const, reset_day: 0, expiry_date: "", egress_mbps: 0 }
    setInitialForm(JSON.stringify(next)); setForm(next); setDraftNotice("")
  }

  function edit(item?: PmPolicy) {
    if (confirmDiscard(dirty)) loadDraft(item)
  }
  function reloadDraft() {
    if (!view || !form || !confirm("重新载入最新配置将替换当前草稿，是否继续？")) return
    const latest = editing ? rules?.find(item => item.port === form.port) : undefined
    if (editing && !latest) { setForm(null); setDraftNotice("原 PM 规则已被删除，草稿已关闭。") }
    else if (!editing && !candidates.length) { setForm(null); setDraftNotice("暂无可用业务端口，草稿已关闭。") }
    else loadDraft(latest)
  }

  return <div className="space-y-4">
    {draftNotice && <p role="status" className="text-sm">{draftNotice}</p>}
    <div className="space-y-3">
      <h2 className="font-medium">受管 PM 规则</h2>
      <p className="text-sm text-muted-foreground">统计本后台已启动的业务端口（1024–65535）的 IPv4 TCP/UDP 流量。IPv6 流量不受这些规则影响。默认只计量。开启配额或到期阻断会丢弃匹配流量，出站超速策略也以丢包执行。告警显示在本页面。</p>
      {view?.blocked && <p className="text-sm text-destructive">PM 状态需要在 VPS 核实。规则表确认已缺失时，可由 root 执行 python3 /opt/node-manager/pm.py repair --confirm-checkpoint-loss，从最近的持久检查点重建。</p>}
      {!view ? <p>此管理组件尚未上报受管 PM。绑定并升级组件后可配置。</p> :
        !view.installed ? <p className="text-sm text-muted-foreground">请先在“管理”页启用 PM。</p> : <>
          <Button disabled={!writable || !legacyReadable || !candidates.length} onClick={() => edit()}>新增 PM 规则</Button>
          {!legacyReadable && <p className="text-sm text-muted-foreground">等待原 PM 配置可读取后再添加规则。</p>}
          {legacyReadable && !candidates.length && <p className="text-sm text-muted-foreground">暂无可用业务端口。请先启动监听 1024–65535 的 Snell、SB 或 FW 实例。</p>}
          {rules?.map(rule => {
            const usage = view.usage?.[String(rule.port)]
            return <div key={rule.port} className="flex flex-wrap items-center justify-between gap-3 rounded border p-3 text-sm">
              <div className="space-y-1">
                <strong>{rule.name} · {rule.port}</strong>
                <p>{rule.quota_gb ? `配额 ${rule.quota_gb} GiB · ${rule.quota_mode === "out_only" ? "仅出站" : "双向"}` : "只计量，无配额"} · {rule.reset_day ? `每月 ${rule.reset_day} 日重置` : "无自动重置"}</p>
                <p>到期日期 {rule.expiry_date || "未设置"} · 出站超速丢包 {rule.egress_mbps ? `${rule.egress_mbps} Mbps` : "关闭"}</p>
                {usage ? <>
                  <p>入站 {gib(usage.rx)} GiB · 出站 {gib(usage.tx)} GiB · <strong>{pmAlerts[usage.alert]}</strong></p>
                  <p className="text-xs text-muted-foreground">采样 {new Date(usage.sampled_at * 1000).toLocaleString()}{usage.period_start && ` · 账期起始 ${usage.period_start} UTC`}</p>
                </> : <p className="text-muted-foreground">用量未知，请核实 VPS 上的 PM 服务。</p>}
              </div>
              <div className="flex flex-wrap gap-2">
                <Button variant="outline" disabled={!writable || !legacyReadable} onClick={() => edit(rule)}>编辑</Button>
                <Button variant="outline" disabled={!writable} onClick={() => run(async () => {
                  if (confirm(`重置 ${node} 上端口 ${rule.port} 的入站和出站用量？重置后按当前规则重新累计。`)) await submit("reset", { port: rule.port })
                })}>重置用量</Button>
                <Button variant="outline" disabled={!writable} onClick={() => run(async () => {
                  if (confirm(`删除 ${node} 上端口 ${rule.port} 的 PM 规则及计数？`)) await submit("delete", { port: rule.port })
                })}>删除</Button>
              </div>
            </div>
          })}
        </>}
      {form && <form className="space-y-3 rounded border p-4" onSubmit={e => {
        e.preventDefault()
        void run(async () => {
          if (draftStale) throw new Error("配置已更新，请先重新载入最新配置。")
          if (!confirm(`在 ${node} 应用端口 ${form.port} 的 PM 规则？开启配额、到期或超速策略可能丢弃端口流量。`)) return
          await submit("apply", form, revision, editing ? "update" : "create")
          setForm(null)
        })
      }}>
        <h3 className="font-medium">{editing ? "编辑" : "新增"} PM 规则 · {node}</h3>
        {draftStale && <div role="alert" className="space-y-2 text-sm"><p>服务器配置已更新，当前草稿已保留。请先重新载入最新配置，再修改并保存。</p><Button type="button" variant="outline" disabled={busy || !view || !writable} onClick={reloadDraft}>重新载入最新配置</Button></div>}
        <div className="grid gap-3 sm:grid-cols-2">
          <label>业务端口<select aria-label="业务端口" className="block w-full rounded border bg-background p-2" disabled={editing} value={form.port} onChange={e => setForm({ ...form, port: Number(e.target.value) })}>
            {editing ? <option value={form.port}>{form.port}</option> : candidates.map(candidate => <option key={candidate.port} value={candidate.port}>{candidate.port} · {candidate.name}</option>)}
          </select></label>
          <label>名称／备注<Input required maxLength={48} value={form.name} onChange={e => setForm({ ...form, name: e.target.value })} /></label>
          <label>配额（GiB，0 关闭）<Input type="number" required min={0} max={8589934591} value={form.quota_gb} onChange={e => setForm({ ...form, quota_gb: Number(e.target.value) })} /></label>
          <label>计费方向<select aria-label="计费方向" className="block w-full rounded border bg-background p-2" value={form.quota_mode} onChange={e => setForm({ ...form, quota_mode: e.target.value as PmPolicy["quota_mode"] })}><option value="in_out">双向，达到配额后丢弃双向流量</option><option value="out_only">仅出站，达到配额后丢弃出站流量</option></select></label>
          <label>每月重置日（0 关闭）<Input type="number" required min={0} max={31} value={form.reset_day} onChange={e => setForm({ ...form, reset_day: Number(e.target.value) })} /></label>
          <label>到期日期（可选）<Input type="date" value={form.expiry_date} onChange={e => setForm({ ...form, expiry_date: e.target.value })} /></label>
          <label>出站超速丢包（Mbps，0 关闭）<Input type="number" required min={0} max={10000} value={form.egress_mbps} onChange={e => setForm({ ...form, egress_mbps: Number(e.target.value) })} /></label>
        </div>
        <p className="text-xs text-muted-foreground">重置按 UTC，每月不足指定日时取月末。到期日 UTC 结束后开始阻断。定时任务约每分钟保存用量，异常重启可能丢失最近一次保存后的用量。</p>
        <Button type="submit" disabled={!writable || draftStale || !legacyReadable || (!editing && !candidates.some(candidate => candidate.port === form.port))}>保存并应用</Button>{" "}
        <Button variant="outline" type="button" disabled={busy} onClick={() => { if (confirmDiscard(dirty)) setForm(null) }}>取消</Button>
      </form>}
    </div>
    <details className="space-y-2 rounded border p-3">
      <summary className="cursor-pointer">原 PM · {pmStatus[legacy?.status ?? ""] ?? "等待组件上报"}</summary>
      <p className="text-sm text-muted-foreground">原 PM 配置和用量以只读方式显示。</p>
      {legacy?.rows.map(row => <div key={row.port} className="rounded border p-3 text-sm">
        <strong>{row.comment || "端口"} · {row.port}</strong>
        <p>配额 {row.quota_gb} GiB · {row.quota_mode === "out_only" ? "仅出站" : "双向"} · 重置日 {row.reset_day || "关闭"}</p>
        <p>{row.state === "ready" && row.rx != null && row.tx != null ? `入站 ${gib(row.rx)} GiB · 出站 ${gib(row.tx)} GiB` : "用量未知，请检查原 PM"}</p>
        <p>突发限速 {row.burst_enabled ? "开启" : "关闭"} · 到期日期 {row.expiry_date || "未设置"}</p>
        <p className="text-xs text-muted-foreground">采样 {row.last_sample ? new Date(row.last_sample * 1000).toLocaleString() : "未知"}</p>
      </div>)}
    </details>
  </div>
}
