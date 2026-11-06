import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Plus } from "lucide-react"
import { InlinePanel, useServiceConfirmation } from "./ServiceInteractions"
import { type ApplyIntent, type PmDraft, type PmPolicy, type RootSnapshot, type Snapshot, pmStatus, pmAlerts, gib, pmDraft, pmPayload } from "./model"

export function PmPanel({ node, snapshot, view, writable, busy, feedback, refreshing, onDirtyChange, run, submit, opening }: {
  node: string
  /** Opened from an instance row: edit its rule, or add one for its port, with a kept draft if any. */
  opening?: { port: number; draft?: PmDraft; seq: number }
  snapshot?: RootSnapshot
  view?: Snapshot
  writable: boolean
  busy: boolean
  feedback: ReactNode
  refreshing: boolean
  onDirtyChange: (dirty: boolean) => void
  run: (action: () => Promise<void>) => Promise<void>
  submit: (action: string, payload: object, revision?: string, intent?: ApplyIntent) => Promise<void>
}) {
  const { confirm, discard, confirmation } = useServiceConfirmation()
  const legacy = snapshot?.port_monitor
  const rules = view?.resources as PmPolicy[] | undefined
  const candidates = [snapshot, snapshot?.modules?.sb, snapshot?.modules?.fw].flatMap(s =>
    s?.resources.filter(r => r.port >= 1024 && s.runtime?.[String(r.port)] === "active"
      && !rules?.some(rule => rule.port === r.port) && !legacy?.rows.some(row => row.port === r.port)) ?? [])
  const legacyReadable = legacy?.status === "ready" || legacy?.status === "not_installed"
  function draftFor(item?: PmPolicy, port = candidates[0]?.port, draft?: PmDraft) {
    if (item) return pmDraft(item)
    if (!candidates.some(candidate => candidate.port === port)) return null
    return draft ?? pmDraft({ port, name: candidates.find(c => c.port === port)?.name || "端口监控", quota_gb: 0, quota_mode: "in_out", reset_day: 0, expiry_date: "", egress_mbps: 0 })
  }
  // Opened from an instance row, the panel starts with that port's form.
  const [start] = useState(() => {
    if (!opening) return { form: null, editing: false, notice: "" }
    const rule = rules?.find(item => item.port === opening.port)
    const form = view?.installed ? draftFor(rule, opening.port, opening.draft) : null
    return { form, editing: !!rule, notice: form ? "" : `端口 ${opening.port} 暂不能添加 PM 规则：实例需在运行中，且未由原 PM 管理。` }
  })
  const [form, setForm] = useState<PmDraft | null>(start.form)
  const [revision, setRevision] = useState(start.form ? view?.revision ?? "" : "")
  const [editing, setEditing] = useState(start.editing)
  const [initialForm, setInitialForm] = useState(start.form ? JSON.stringify(start.form) : "")
  const [draftNotice, setDraftNotice] = useState(start.notice)
  const formTrigger = useRef<HTMLElement | null>(null)
  const dirty = form !== null && JSON.stringify(form) !== initialForm
  const draftStale = form !== null && revision !== view?.revision
  useLayoutEffect(() => { onDirtyChange(dirty) }, [dirty, onDirtyChange])
  useEffect(() => () => onDirtyChange(false), [onDirtyChange])

  function loadDraft(item?: PmPolicy) {
    const next = draftFor(item)
    if (!next) return
    setEditing(!!item)
    setRevision(view?.revision ?? "")
    setInitialForm(JSON.stringify(next)); setForm(next); setDraftNotice("")
  }

  async function edit(item?: PmPolicy, trigger?: HTMLElement) {
    if (busy) return
    if (form && editing === !!item && (!item || form.port === item.port)) { await closeForm(); return }
    if (await discard(dirty)) { formTrigger.current = trigger ?? null; loadDraft(item) }
  }
  function restoreFocus() { if (formTrigger.current?.isConnected) formTrigger.current.focus({ preventScroll: true }) }
  async function closeForm() { if (!busy && await discard(dirty)) { setForm(null); requestAnimationFrame(restoreFocus) } }
  async function reloadDraft() {
    if (!view || !form || !await confirm("重新载入最新配置将替换当前草稿，是否继续？")) return
    const latest = editing ? rules?.find(item => item.port === form.port) : undefined
    if (editing && !latest) { setForm(null); setDraftNotice("原 PM 规则已被删除，草稿已关闭。") }
    else if (!editing && !candidates.length) { setForm(null); setDraftNotice("暂无可用业务端口，草稿已关闭。") }
    else loadDraft(latest)
  }

  return <div className="space-y-4">
    {feedback}
    {!form && confirmation}
    {draftNotice && <p role="status" className="text-sm">{draftNotice}</p>}
    <div className="space-y-3">
      <p className="text-xs text-muted-foreground">统计 IPv4 TCP/UDP 流量，默认仅计量；配额、到期和出站超速策略以丢包执行，IPv6 不受影响。</p>
      {view?.blocked && <p className="text-sm text-destructive">PM 状态需要在 VPS 核实。规则表确认已缺失时，可由 root 执行 python3 /opt/node-manager/pm.py repair --confirm-checkpoint-loss，从最近的持久检查点重建。</p>}
      {!view ? <p>此管理组件尚未上报受管 PM。绑定并升级组件后可配置。</p> :
        !view.installed ? <p className="text-sm text-muted-foreground">请先启用 PM。</p> : <>
          <div className="flex flex-wrap items-center justify-between gap-3"><span className="text-xs text-muted-foreground">端口规则 · {rules?.length ?? 0}</span><Button className="min-h-11" disabled={!writable || !legacyReadable || !candidates.length} aria-expanded={!!form && !editing} onClick={event => { void edit(undefined, event.currentTarget) }}><Plus aria-hidden="true" />{form && !editing ? "收起新增" : "新增 PM 规则"}</Button></div>
          {!legacyReadable && <p className="text-sm text-muted-foreground">等待原 PM 配置可读取后再添加规则。</p>}
          {legacyReadable && !candidates.length && <p className="text-sm text-muted-foreground">暂无可用业务端口。请先启动监听 1024–65535 的 Snell、SB 或 FW 实例。</p>}
          {rules?.map(rule => {
            const usage = view.usage?.[String(rule.port)]
            return <div key={rule.port} className="flex flex-wrap items-center justify-between gap-3 rounded border p-3 text-sm">
              <div className="space-y-1">
                <strong>{rule.name} · {rule.port}</strong>
                <p>{rule.quota_gb ? `配额 ${rule.quota_gb} GiB · ${rule.quota_mode === "out_only" ? "仅出站" : "双向"}` : "只计量，无配额"} · {rule.reset_day ? `每月 ${rule.reset_day} 日重置` : "无自动重置"}</p>
                <p>到期日期 {rule.expiry_date || "未设置"} · 出站超速丢包 {rule.egress_mbps ? `${rule.egress_mbps} Mbps` : "关闭"} · IP 告警 {rule.ip_limit ? `超过 ${rule.ip_limit.max_ips} 个` : "关闭"}</p>
                {usage ? <>
                  <p>入站 {gib(usage.rx)} GiB · 出站 {gib(usage.tx)} GiB · <strong>{pmAlerts[usage.alert]}</strong></p>
                  {rule.ip_limit && usage.ip_count != null && <p className={usage.ip_level ? "text-destructive" : undefined}>在线 IP {usage.ip_count} / {rule.ip_limit.max_ips}{usage.ip_list?.length ? ` · ${usage.ip_list.join("、")}` : ""}</p>}
                  <p className="text-xs text-muted-foreground">采样 {new Date(usage.sampled_at * 1000).toLocaleString()}{usage.period_start && ` · 账期起始 ${usage.period_start} UTC`}</p>
                </> : <p className="text-muted-foreground">用量未知，请核实 VPS 上的 PM 服务。</p>}
              </div>
              <div className="flex flex-wrap gap-2">
                <Button variant="outline" disabled={!writable || !legacyReadable} aria-expanded={editing && form?.port === rule.port} onClick={event => { void edit(rule, event.currentTarget) }}>{editing && form?.port === rule.port ? "收起编辑" : "编辑"}</Button>
                <Button variant="outline" disabled={!writable} onClick={() => run(async () => {
                  if (await confirm(`重置 ${node} 上端口 ${rule.port} 的入站和出站用量？重置后按当前规则重新累计。`, { destructive: true })) await submit("reset", { port: rule.port })
                })}>重置用量</Button>
                <Button variant="outline" disabled={!writable} onClick={() => run(async () => {
                  if (await confirm(`删除 ${node} 上端口 ${rule.port} 的 PM 规则及计数？`, { destructive: true })) await submit("delete", { port: rule.port })
                })}>删除</Button>
              </div>
            </div>
          })}
        </>}
      {form && <InlinePanel title={`${editing ? "编辑" : "新增"} PM 规则 · ${node}`} testId="pm-policy-editor" busy={busy} onClose={() => { void closeForm() }} footer={<><Button variant="outline" disabled={busy} onClick={() => { void closeForm() }}>取消</Button><Button type="submit" form="pm-policy-form" disabled={!writable || draftStale || !legacyReadable || (!editing && !candidates.some(candidate => candidate.port === form.port))}>保存并应用</Button></>}>
          <form id="pm-policy-form" className="space-y-4" onSubmit={e => {
        e.preventDefault()
        void run(async () => {
          if (draftStale) throw new Error("配置已更新，请先重新载入最新配置。")
          const payload = pmPayload(form)
          if (!await confirm(`在 ${node} 应用端口 ${form.port} 的 PM 规则？开启配额、到期或超速策略可能丢弃端口流量。`)) return
          await submit("apply", payload, revision, editing ? "update" : "create")
          setForm(null)
        })
      }}>
        {draftStale && <div role="alert" className="space-y-2 text-sm"><p>服务器配置已更新，当前草稿已保留。请先重新载入最新配置，再修改并保存。</p><Button type="button" variant="outline" disabled={busy || refreshing || !view || !writable} onClick={() => { void reloadDraft() }}>重新载入最新配置</Button></div>}
        <fieldset disabled={busy} className="grid gap-3 sm:grid-cols-2">
          <label>业务端口<select aria-label="业务端口" className="block w-full rounded border bg-background p-2" disabled={editing} value={form.port} onChange={e => setForm({ ...form, port: Number(e.target.value) })}>
            {editing ? <option value={form.port}>{form.port}</option> : candidates.map(candidate => <option key={candidate.port} value={candidate.port}>{candidate.port} · {candidate.name}</option>)}
          </select></label>
          <label>名称／备注<Input required maxLength={48} value={form.name} onChange={e => setForm({ ...form, name: e.target.value })} /></label>
        </fieldset>
        <PmFields form={form} setForm={setForm} busy={busy} />
        <p className="text-xs text-muted-foreground">重置按 UTC，每月不足指定日时取月末。到期日 UTC 结束后开始阻断。定时任务约每分钟保存用量，异常重启可能丢失最近一次保存后的用量。</p>
      </form>
          {confirmation}
      </InlinePanel>}
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

/** The PM policy fields, shared by the PM form and the new-instance form. */
export function PmFields({ form, setForm, busy }: { form: PmDraft; setForm: (form: PmDraft) => void; busy: boolean }) {
  return <>
    <fieldset disabled={busy} className="grid gap-3 sm:grid-cols-2">
      <label>配额（GiB，0 关闭）<Input type="number" required min={0} max={8589934591} value={form.quota_gb} onChange={e => setForm({ ...form, quota_gb: Number(e.target.value) })} /></label>
      <label>计费方向<select aria-label="计费方向" className="block w-full rounded border bg-background p-2" value={form.quota_mode} onChange={e => setForm({ ...form, quota_mode: e.target.value as PmPolicy["quota_mode"] })}><option value="in_out">双向，达到配额后丢弃双向流量</option><option value="out_only">仅出站，达到配额后丢弃出站流量</option></select></label>
      <label>每月重置日（0 关闭）<Input type="number" required min={0} max={31} value={form.reset_day} onChange={e => setForm({ ...form, reset_day: Number(e.target.value) })} /></label>
      <label>到期日期（可选）<Input type="date" value={form.expiry_date} onChange={e => setForm({ ...form, expiry_date: e.target.value })} /></label>
      <label>出站超速丢包（Mbps，0 关闭）<Input type="number" required min={0} max={10000} value={form.egress_mbps} onChange={e => setForm({ ...form, egress_mbps: Number(e.target.value) })} /></label>
    </fieldset>
    <details className="text-sm" open={form.max_ips > 0}>
      <summary className="cursor-pointer text-muted-foreground">IP 告警（可选）</summary>
      <fieldset disabled={busy} className="mt-3 grid gap-3 sm:grid-cols-2">
        <label>同时在线 IP 上限（0 关闭）<Input type="number" required min={0} max={65535} value={form.max_ips} onChange={e => setForm({ ...form, max_ips: Number(e.target.value) })} /></label>
        <label>重复提醒间隔（分钟）<Input type="number" required min={1} max={525600} disabled={!form.max_ips} value={form.cooldown_min} onChange={e => setForm({ ...form, cooldown_min: Number(e.target.value) })} /></label>
        <label className="sm:col-span-2">白名单 IP（可选，每行或逗号分隔，最多 32 个）<textarea aria-label="白名单 IP（可选，每行或逗号分隔，最多 32 个）" className="block min-h-20 w-full rounded border bg-background p-2" disabled={!form.max_ips} value={form.whitelist} onChange={e => setForm({ ...form, whitelist: e.target.value })} /></label>
      </fieldset>
      <p className="mt-2 text-xs text-muted-foreground">每分钟统计已建立 TCP 连接的不同来源 IP，连续两分钟出现才计入，避免扫描误报。超过上限时通过服务管理提醒推送，超过上限 2 个以上为高风险，同一等级按间隔重复提醒；只提醒，不断开连接。</p>
    </details>
  </>
}
