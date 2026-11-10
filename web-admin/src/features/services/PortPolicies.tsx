import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react"
import { bytes } from "@/lib/format"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Plus } from "lucide-react"
import { InlinePanel, useServiceConfirmation } from "./ServiceInteractions"
import { type ApplyIntent, type PmDraft, type PmPolicy, type PmUsage, type RootSnapshot, type Snapshot, pmAlerts, pmTrafficBars, type PmTrafficHour, type PmTrafficBar, gib, pmDraft, newPmDraft, pmPayload, ipThresholds, ipBlock, readManagement } from "./model"

export function PmPanel({ node, nodeId, snapshot, view, writable, busy, feedback, refreshing, onDirtyChange, run, submit, opening, alerts = true }: {
  node: string
  nodeId: number
  alerts?: boolean
  /** Opened from an instance row: show its rule, or add one for its port, with a kept draft if any. */
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
  const rules = view?.resources as PmPolicy[] | undefined
  const candidates = [snapshot, snapshot?.modules?.sb, snapshot?.modules?.fw].flatMap(s =>
    s?.resources.filter(r => r.port >= 1024 && s.runtime?.[String(r.port)] === "active"
      && !rules?.some(rule => rule.port === r.port)) ?? [])
  function draftFor(item?: PmPolicy, port = candidates[0]?.port, draft?: PmDraft) {
    if (item) return pmDraft(item)
    if (!candidates.some(candidate => candidate.port === port)) return null
    return draft ?? newPmDraft(port, candidates.find(c => c.port === port)?.name || "端口监控")
  }
  // Opened from an instance row: an existing rule is only scrolled to, a missing one starts its form.
  const [start] = useState(() => {
    if (!opening || rules?.some(item => item.port === opening.port)) return { form: null, editing: false, notice: "" }
    const form = view?.installed ? draftFor(undefined, opening.port, opening.draft) : null
    return { form, editing: false, notice: form ? "" : `端口 ${opening.port} 暂不能添加流量统计规则：实例需在运行中。` }
  })
  const openedRule = useRef<HTMLDivElement | null>(null)
  // After the page's own scroll to the module area, which is queued first.
  useEffect(() => {
    const frame = requestAnimationFrame(() => openedRule.current?.scrollIntoView({ block: "nearest", behavior: window.matchMedia("(prefers-reduced-motion: reduce)").matches ? "instant" : "smooth" }))
    return () => cancelAnimationFrame(frame)
  }, [])
  const [form, setForm] = useState<PmDraft | null>(start.form)
  const [revision, setRevision] = useState(start.form ? view?.revision ?? "" : "")
  const [editing, setEditing] = useState(start.editing)
  const [ruleDetails, setRuleDetails] = useState<Record<number, boolean>>({})
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
    if (editing && !latest) { setForm(null); setDraftNotice("该流量统计规则已被删除，草稿已关闭。") }
    else if (!editing && !candidates.length) { setForm(null); setDraftNotice("暂无可用业务端口，草稿已关闭。") }
    else loadDraft(latest)
  }

  // Placed as in the other modules: a new rule under its button, an edited one in its own card.
  const editor = form && <InlinePanel title={editing ? `编辑 · ${rules?.find(rule => rule.port === form.port)?.name ?? form.port}` : "新增流量统计规则"} testId="pm-policy-editor" busy={busy} closable={false} embedded={editing} onClose={() => { void closeForm() }} footer={<><Button variant="outline" disabled={busy} onClick={() => { void closeForm() }}>取消</Button><Button type="submit" form="pm-policy-form" disabled={!writable || draftStale || (!editing && !candidates.some(candidate => candidate.port === form.port))}>保存并应用</Button></>}>
          <form id="pm-policy-form" className="space-y-4" onSubmit={e => {
        e.preventDefault()
        void run(async () => {
          if (draftStale) throw new Error("配置已更新，请先重新载入最新配置。")
          const payload = pmPayload(form)
          if (!await confirm(`在 ${node} 应用端口 ${form.port} 的流量统计规则？开启配额、到期或超速策略可能丢弃端口流量。`)) return
          await submit("apply", payload, revision, editing ? "update" : "create")
          setForm(null)
        })
      }}>
        {draftStale && <div role="alert" className="space-y-2 text-sm"><p>服务器配置已更新，当前草稿已保留。请先重新载入最新配置，再修改并保存。</p><Button type="button" variant="outline" disabled={busy || refreshing || !view || !writable} onClick={() => { void reloadDraft() }}>重新载入最新配置</Button></div>}
        <fieldset disabled={busy} className="grid gap-3 sm:grid-cols-2">
          <legend className="mb-2 text-xs font-medium text-muted-foreground">基础</legend>
          <label>业务端口<select aria-label="业务端口" className="block w-full rounded border bg-background p-2" disabled={editing} value={form.port} onChange={e => setForm({ ...form, port: Number(e.target.value) })}>
            {editing ? <option value={form.port}>{form.port}</option> : candidates.map(candidate => <option key={candidate.port} value={candidate.port}>{candidate.port} · {candidate.name}</option>)}
          </select></label>
          <label>名称／备注<Input required maxLength={48} value={form.name} onChange={e => setForm({ ...form, name: e.target.value })} /></label>
        </fieldset>
        <PmFields form={form} setForm={setForm} busy={busy} alerts={alerts} />
      </form>
          {confirmation}
      </InlinePanel>

  return <div className="space-y-4">
    {feedback}
    {!form && confirmation}
    {draftNotice && <p role="status" className="text-sm">{draftNotice}</p>}
    <div className="space-y-3">
      {view?.blocked && <p className="text-sm text-destructive">流量统计状态需要在 VPS 核实。规则表确认已缺失时，可由 root 执行 python3 /opt/node-manager/pm.py repair --confirm-checkpoint-loss，从最近的持久检查点重建。</p>}
      {!view ? <p>此管理组件尚未上报受管流量统计。绑定并升级组件后可配置。</p> :
        !view.installed ? <p className="text-sm text-muted-foreground">请先启用流量统计。</p> : <>
          <div className="flex flex-wrap items-center justify-between gap-3"><span className="text-xs text-muted-foreground">端口规则 · {rules?.length ?? 0}</span><Button className="min-h-11" disabled={!writable || !candidates.length} aria-expanded={!!form && !editing} onClick={event => { void edit(undefined, event.currentTarget) }}><Plus aria-hidden="true" />新增流量统计规则</Button></div>
          {form && (!editing || !rules?.some(rule => rule.port === form.port)) && editor}
          {!candidates.length && <p className="text-sm text-muted-foreground">暂无可用业务端口。请先启动监听 1024–65535 的 Snell、singbox 或 realm 转发实例。</p>}
          {rules?.map(rule => {
            const usage = view.usage?.[String(rule.port)]
            const used = usage ? (rule.quota_mode === "out_only" ? usage.tx : usage.rx + usage.tx) : 0
            const percent = rule.quota_gb ? used / (rule.quota_gb * 1073741824) * 100 : 0
            const alert = usage?.alert ?? "normal"
            const tone = alert === "quota_warning" ? "bg-amber-500/10 text-amber-700 dark:text-amber-400" : alert === "quota_reached" || alert === "expired" ? "bg-destructive/10 text-destructive" : "bg-muted text-muted-foreground"
            const label = pmAlerts[alert] ?? alert
            return <div key={rule.port} ref={rule.port === opening?.port ? openedRule : undefined} data-testid={`pm-rule-${rule.port}`} className="min-w-0 space-y-4 rounded-lg border bg-background p-4 text-sm">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1"><strong className="break-all font-medium">{rule.name}</strong>{rule.name !== String(rule.port) && <span className="text-xs text-muted-foreground">端口 {rule.port}</span>}{usage && <span className={`rounded px-2 py-1 text-xs ${tone}`}>{label}</span>}</div>
                <div className="flex flex-wrap gap-2">
                  <Button variant="outline" disabled={!writable} aria-expanded={editing && form?.port === rule.port} onClick={event => { void edit(rule, event.currentTarget) }}>{editing && form?.port === rule.port ? "收起编辑" : "编辑"}</Button>
                  <Button variant="outline" disabled={!writable} onClick={() => run(async () => {
                    if (await confirm(`重置 ${node} 上端口 ${rule.port} 的入站和出站用量？重置后按当前规则重新累计。`, { destructive: true })) await submit("reset", { port: rule.port })
                  })}>重置用量</Button>
                  <Button variant="ghost" className="text-muted-foreground hover:text-destructive" disabled={!writable} onClick={() => run(async () => {
                    if (await confirm(`删除 ${node} 上端口 ${rule.port} 的流量统计规则及计数？`, { destructive: true })) await submit("delete", { port: rule.port })
                  })}>删除</Button>
                </div>
              </div>
              {usage ? <>
                <div className="space-y-2">
                  <div className="flex flex-wrap items-baseline justify-between gap-2">
                    <p className="tabular-nums"><span className="text-muted-foreground">已用 </span><strong className="font-medium">{gib(used)}</strong><span className="text-muted-foreground">{rule.quota_gb ? ` / ${rule.quota_gb} GiB` : " GiB"}</span></p>
                    <span className="text-xs text-muted-foreground">{rule.quota_gb ? `${rule.quota_mode === "out_only" ? "仅出站" : "双向"} · ${percent.toFixed(1)}%` : "仅计量"}</span>
                  </div>
                  {rule.quota_gb > 0 && <div role="progressbar" aria-label={`${rule.name}配额用量`} aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.min(100, percent)} aria-valuetext={`已用 ${gib(used)} / ${rule.quota_gb} GiB`} className="h-1.5 overflow-hidden rounded-full bg-muted"><div className={`h-full ${alert === "quota_reached" || alert === "expired" ? "bg-destructive" : alert === "quota_warning" ? "bg-amber-500" : "bg-primary"}`} style={{ width: `${Math.min(100, percent)}%` }} /></div>}
                </div>
                <div className="flex flex-wrap items-start gap-x-6 gap-y-2 text-xs">
                  <p className="tabular-nums"><span className="text-muted-foreground">入站 </span>{gib(usage.rx)} GiB</p>
                  <p className="tabular-nums"><span className="text-muted-foreground">出站 </span>{gib(usage.tx)} GiB</p>
                  <OnlineIps usage={usage} />
                </div>
              </> : <p className="text-muted-foreground">用量未知，请核实 VPS 上的流量统计服务。</p>}
              <div className="flex flex-wrap items-start gap-x-6 gap-y-2 text-xs">
                <button type="button" aria-expanded={!!ruleDetails[rule.port]} aria-controls={`pm-rule-details-${rule.port}`} className="cursor-pointer text-muted-foreground hover:text-foreground focus-visible:outline-2 focus-visible:outline-ring" onClick={() => setRuleDetails(open => ({ ...open, [rule.port]: !open[rule.port] }))}><span aria-hidden="true">{ruleDetails[rule.port] ? "▾" : "▸"}</span> 规则详情</button>
                  <dl id={`pm-rule-details-${rule.port}`} hidden={!ruleDetails[rule.port]} className="order-1 mt-1 grid basis-full grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-2">
                    <dt className="text-muted-foreground">配额</dt><dd>{rule.quota_gb ? `${rule.quota_gb} GiB · ${rule.quota_mode === "out_only" ? "单向-仅出站" : "双向"}` : "仅计量"}</dd>
                    <dt className="text-muted-foreground">自动重置</dt><dd>{rule.reset_day ? `每月 ${rule.reset_day} 日` : "未设置"}</dd>
                    {rule.expiry_date && <><dt className="text-muted-foreground">到期日期</dt><dd>{rule.expiry_date}</dd></>}
                    {rule.egress_mbps > 0 && <><dt className="text-muted-foreground">出站限速</dt><dd>{rule.egress_mbps} Mbps</dd></>}
                    {rule.ip_limit && <><dt className="text-muted-foreground">IP 告警</dt><dd>{ipThresholds(rule.ip_limit.max_ips, rule.ip_limit.block)}</dd></>}
                    {usage && <><dt className="text-muted-foreground">更新时间</dt><dd>{new Date(usage.sampled_at * 1000).toLocaleString()}</dd>{usage.period_start && <><dt className="text-muted-foreground">账期起始</dt><dd>{usage.period_start} UTC</dd></>}</>}
                  </dl>
                <PortTraffic key={`${nodeId}:${rule.port}`} node={nodeId} port={rule.port} />
              </div>
              {editing && form?.port === rule.port && editor}
            </div>
          })}
        </>}
    </div>
  </div>
}

/** The PM policy fields, shared by the PM form and the new-instance form. */
export function PmFields({ form, setForm, busy, alerts = true }: { form: PmDraft; setForm: (form: PmDraft) => void; busy: boolean; alerts?: boolean }) {
  return <>
    <fieldset disabled={busy} className="grid gap-3 sm:grid-cols-2">
      <legend className="mb-2 text-xs font-medium text-muted-foreground">配额与周期</legend>
      <label>配额（GiB，0 关闭）<Input className="block sm:max-w-48" type="number" required min={0} max={8589934591} value={form.quota_gb} onChange={e => setForm({ ...form, quota_gb: Number(e.target.value) })} /></label>
      <label>计费方向<select aria-label="计费方向" className="block h-9 w-full min-w-0 rounded-md border border-input bg-transparent px-3 py-1 text-base shadow-xs outline-none transition-[color,box-shadow] focus-visible:border-ring focus-visible:ring-[3px] focus-visible:ring-ring/50 disabled:pointer-events-none disabled:cursor-not-allowed disabled:opacity-50 md:text-sm dark:bg-input/30" value={form.quota_mode} onChange={e => setForm({ ...form, quota_mode: e.target.value as PmPolicy["quota_mode"] })}><option value="in_out">双向</option><option value="out_only">单向-仅出站</option></select></label>
      <label>每月重置日（0 关闭）<Input className="block sm:max-w-48" type="number" required min={0} max={31} value={form.reset_day} onChange={e => setForm({ ...form, reset_day: Number(e.target.value) })} /></label>
      <label>到期日期（可选）<Input type="date" value={form.expiry_date} onChange={e => setForm({ ...form, expiry_date: e.target.value })} /></label>
      <label>出站超速丢包（Mbps，0 关闭）<Input className="block sm:max-w-48" type="number" required min={0} max={10000} value={form.egress_mbps} onChange={e => setForm({ ...form, egress_mbps: Number(e.target.value) })} /></label>
      <p className="text-xs text-muted-foreground sm:col-span-2">重置按 UTC，每月不足指定日时取月末。到期日 UTC 结束后开始阻断。定时任务约每分钟保存用量，异常重启可能丢失最近一次保存后的用量。</p>
    </fieldset>
    <details className="text-sm" open={form.max_ips > 0}>
      <summary className="cursor-pointer text-xs font-medium text-muted-foreground">IP 告警与阻断</summary>
      <fieldset disabled={busy} className="mt-3 grid gap-3 sm:grid-cols-2">
        <label>告警阈值（IP 数，0 关闭）<Input className="block sm:max-w-48" aria-label="告警阈值（IP 数，0 关闭）" type="number" required min={0} max={65535} value={form.max_ips} onChange={e => setForm({ ...form, max_ips: Number(e.target.value) })} />{form.max_ips > 0 && <span className="text-xs text-muted-foreground">{ipThresholds(form.max_ips, form.block)}</span>}</label>
        <label>重复提醒间隔（分钟）<Input className="block sm:max-w-48" type="number" required min={1} max={525600} disabled={!form.max_ips} value={form.cooldown_min} onChange={e => setForm({ ...form, cooldown_min: Number(e.target.value) })} /></label>
        <label className="flex items-center gap-2 sm:col-span-2"><input type="checkbox" className="size-4" aria-label={form.max_ips > 0 ? ipBlock(form.max_ips) : "阻断后续连接"} disabled={!form.max_ips} checked={form.block} onChange={e => setForm({ ...form, block: e.target.checked })} />{form.max_ips > 0 ? ipBlock(form.max_ips) : "阻断后续连接"}</label>
        <label className="sm:col-span-2">白名单 IP / 网段（可选，最多 32 项）<textarea aria-label="白名单 IP / 网段（可选，最多 32 项）" className="block min-h-20 w-full rounded border bg-background p-2" disabled={!form.max_ips} placeholder="例如：203.0.113.7、203.0.113.0/24、2001:db8::/64；每行或逗号分隔" value={form.whitelist} onChange={e => setForm({ ...form, whitelist: e.target.value })} /></label>
      </fieldset>
      <p className="mt-2 text-xs text-muted-foreground">白名单内的 IP 和网段不计数、不阻断。</p>
      <details className="mt-2 text-xs text-muted-foreground"><summary className="cursor-pointer">统计与阻断说明</summary><p className="mt-2">每分钟检查，连续两次在线才计数；扫描和 Reality 验证失败不计入。超过阈值后按提醒间隔推送。阻断保留最早在线的 IP，新增 IP 拒绝 30 分钟；白名单或数量变更后下一分钟重新判断。</p></details>
    </details>
    {!alerts && (form.quota_gb > 0 || form.max_ips > 0) && <p role="status" className="text-xs text-amber-700 dark:text-amber-400">提醒推送未启用，可在“通知”页配置。</p>}
  </>
}

/** Online TCP addresses, counted for every rule; coloured by level when the rule has a limit. */
function OnlineIps({ usage }: { usage: PmUsage }) {
  if (usage.ip_count == null) return null
  const list = usage.ip_list ?? []
  const tone = usage.ip_level === 2 ? "text-destructive" : usage.ip_level === 1 ? "text-amber-600 dark:text-amber-400" : undefined
  const label = `TCP 在线 IP ${usage.ip_count} 个${usage.ip_level === 2 ? " · 报警" : usage.ip_level === 1 ? " · 告警" : ""}`
  if (!list.length) return <p className={tone}>{label}</p>
  return <details className={tone}>
    <summary className="cursor-pointer">{label}</summary>
    <p className="break-all">{list.join("、")}{usage.ip_count > list.length ? `（显示前 ${list.length} 个）` : ""}</p>
    <p className="text-xs text-muted-foreground">连续两分钟以上的 TCP 来源地址，不含 UDP 与白名单。</p>
  </details>
}

/** Lazy, bounded history: seven UTC days and a complete 24-hour drill-down. */
function PortTraffic({ node, port }: { node: number; port: number }) {
  const [hours, setHours] = useState<PmTrafficHour[] | null>(null)
  const [error, setError] = useState("")
  const [loading, setLoading] = useState(false)
  const [day, setDay] = useState("")
  const [now, setNow] = useState(() => Date.now() / 1000)
  const [open, setOpen] = useState(false)
  const request = useRef(false)
  async function load() {
    if (request.current) return
    request.current = true; setLoading(true); setError("")
    try {
      const data = await readManagement<{ hours: PmTrafficHour[] }>(`/management/${node}/traffic?port=${port}`)
      setHours(data.hours); setNow(Date.now() / 1000)
    } catch (e) { setError((e as Error).message) }
    finally { request.current = false; setLoading(false) }
  }
  const days = pmTrafficBars(hours ?? [], now)
  const chosen = days.find(item => item.key === day)
  const items = chosen ? pmTrafficBars(hours ?? [], now, chosen.key) : days
  // A toggle in the card's detail row, and its panel below the row (order-1) at full width.
  return <>
    <button type="button" aria-expanded={open} aria-controls={`pm-traffic-${port}`} className="cursor-pointer text-muted-foreground hover:text-foreground focus-visible:outline-2 focus-visible:outline-ring" onClick={() => { setOpen(!open); if (!open && hours === null) void load() }}><span aria-hidden="true">{open ? "▾" : "▸"}</span> 用量详情（近 7 天）</button>
    <div id={`pm-traffic-${port}`} hidden={!open} className="order-1 mt-1 min-w-0 basis-full space-y-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap items-center gap-3">
          <span>{chosen ? `${chosen.key} · 小时用量` : "近 7 天用量"} <span className="text-muted-foreground">UTC</span></span>
          {chosen && <Button size="sm" variant="ghost" onClick={() => setDay("")}>返回近 7 天</Button>}
        </div>
        <Button size="sm" variant="ghost" disabled={loading} onClick={() => void load()}>{loading ? "读取中…" : "刷新"}</Button>
      </div>
      {error && <p role="alert" className="text-destructive">{error}</p>}
      {hours !== null && <>
        <TrafficBars key={chosen?.key ?? "days"} items={items} hourly={!!chosen} onSelect={chosen ? undefined : setDay} />
        <p className="tabular-nums text-muted-foreground">{chosen ? "当日已记录" : "近 7 天已记录"} 入站 {bytes(items.reduce((sum, d) => sum + d.rx, 0))} · 出站 {bytes(items.reduce((sum, d) => sum + d.tx, 0))}</p>
        <details className="text-muted-foreground"><summary className="cursor-pointer">统计口径</summary><p className="mt-2">统计 IPv4 TCP/UDP 流量，默认仅计量；配额、到期和出站超速策略以丢包执行，IPv6 不受影响。</p><p className="mt-2">按 UTC 和后台收到上报的时间归集；离线期间的增量计入恢复上报的小时。仅保留滚动 7 天，不补齐首次记录前的数据；账期切换、计数重置或缺失读数可能丢失部分增量。无记录不代表零用量，图表合计不等于当前账期用量。</p></details>
      </>}
    </div>
  </>
}

function TrafficBars({ items, hourly, onSelect }: { items: PmTrafficBar[]; hourly: boolean; onSelect?: (key: string) => void }) {
  const [selected, setSelected] = useState(() => Math.max(0, items.findLastIndex(item => item.state === "recorded")))
  const top = Math.max(1, ...items.flatMap(item => [item.rx, item.tx]))
  const current = items[selected] ?? items[0]
  const stateLabel = (item: PmTrafficBar) => item.state === "future" ? "未到时间" : item.state === "missing" ? "无记录" : `入站 ${bytes(item.rx)} · 出站 ${bytes(item.tx)}`
  const caption = (item: PmTrafficBar) => `${item.label}${hourly ? `–${String(Number(item.key) + 1).padStart(2, "0")}:00` : ""}`
  return <div className="space-y-3">
    <div className="flex flex-wrap items-center justify-between gap-2 text-muted-foreground">
      <span>流量{hourly ? " · 每组 1 小时" : " · 点击日期查看小时"}</span>
      <div className="flex gap-4"><span className="flex items-center gap-1"><i className="size-2 rounded-sm bg-blue-300 dark:bg-blue-400/60" />入站</span><span className="flex items-center gap-1"><i className="size-2 rounded-sm bg-blue-600 dark:bg-blue-400" />出站</span></div>
    </div>
    <div className="grid grid-cols-[3.5rem_minmax(0,1fr)] gap-2">
      <div aria-hidden="true" className="flex h-40 flex-col justify-between text-right text-[11px] tabular-nums text-muted-foreground"><span>{bytes(top)}</span><span>{bytes(top / 2)}</span><span>0</span></div>
      <div className="min-w-0">
        <div className="relative h-40 border-b" role="group" aria-label={hourly ? "24 小时入站与出站用量" : "近 7 天入站与出站用量"}>
          <div aria-hidden="true" className="pointer-events-none absolute inset-x-0 top-0 border-t" /><div aria-hidden="true" className="pointer-events-none absolute inset-x-0 top-1/2 border-t" />
          <div className="relative grid h-full gap-px" style={{ gridTemplateColumns: `repeat(${items.length}, minmax(0, 1fr))` }}>
            {items.map((item, index) => <button key={item.key} type="button" aria-label={`${caption(item)} ${stateLabel(item)}${item.partial ? "（时段未完整覆盖）" : ""}`} aria-pressed={index === selected} className={`relative flex h-full min-w-0 items-end justify-center gap-px px-px outline-offset-2 hover:bg-muted focus-visible:bg-muted ${index === selected ? "bg-muted/60" : ""}`} onMouseEnter={() => setSelected(index)} onFocus={() => setSelected(index)} onClick={() => { setSelected(index); onSelect?.(item.key) }}>
              {item.state === "recorded" ? <><span aria-hidden="true" className="w-2/5 rounded-t-sm bg-blue-300 dark:bg-blue-400/60" style={{ height: `${item.rx / top * 100}%`, minHeight: item.rx ? 2 : 0 }} /><span aria-hidden="true" className="w-2/5 rounded-t-sm bg-blue-600 dark:bg-blue-400" style={{ height: `${item.tx / top * 100}%`, minHeight: item.tx ? 2 : 0 }} /></> : <span aria-hidden="true" className={`mb-1 size-1 rounded-full ${item.state === "missing" ? "bg-muted-foreground/60" : "border border-muted-foreground/50"}`} />}
            </button>)}
          </div>
        </div>
        <div aria-hidden="true" className="mt-2 grid text-center text-[11px] text-muted-foreground" style={{ gridTemplateColumns: `repeat(${items.length}, minmax(0, 1fr))` }}>{items.map((item, index) => <span key={item.key} className="relative">{!hourly || [0, 6, 12, 18, 23].includes(index) ? <span className="absolute left-1/2 -translate-x-1/2 whitespace-nowrap">{hourly ? item.key.padStart(2, "0") : item.label}</span> : null}</span>)}</div>
      </div>
    </div>
    <div className="pt-3">
      {hourly && <label className="mb-2 flex items-center gap-2 text-muted-foreground">小时<select className="h-9 rounded-md border bg-background px-2 text-sm text-foreground" aria-label="查看小时用量" value={selected} onChange={event => setSelected(Number(event.target.value))}>{items.map((item, index) => <option key={item.key} value={index}>{caption(item)}</option>)}</select></label>}
      <p aria-live="polite" className="rounded-md bg-muted/50 p-3 tabular-nums">{caption(current)} · {stateLabel(current)}{current.partial && current.state !== "future" ? " · 时段未完整覆盖" : ""}</p>
    </div>
  </div>
}
