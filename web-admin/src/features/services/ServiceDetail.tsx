import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react"
import { api, ApiError, type Node } from "@/lib/api"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Plus, RefreshCw } from "lucide-react"
import { PmFields, PmPanel } from "./PortPolicies"
import { InlinePanel, useServiceConfirmation } from "./ServiceInteractions"
import { type Module, type ApplyIntent, type Resource, type CoreAction, type Snapshot, type Status, type NodeStatus, type FollowUp, type PmDraft, type PmPolicy, names, modules, moduleView, lifecycleActive, lifecycleLabels, runtimeLabels, runtimeDots, actionLabels, labels, sniChoices, quote, newResource, editable, instancePayload, pmBlocked, pmDraft, newPmDraft, pmPayload, pmSummary, serviceFeedbackClass } from "./model"

export function ManagementDetail({ node, module, selectedName, record, defaultAddress, notice, progress = "", progressStatus = "pending", progressAction, alerts = true, onSubmitted, onOpenPm, pmOpening, onDirtyChange, onNodeManagement, enabled, settingsError, reload }: {
  node: number; module: Module; selectedName: string; record?: NodeStatus; defaultAddress: string; notice?: string
  /** A two-step change's progress; "1/2" or "2/2" while it runs. */
  progress?: string
  progressStatus?: string
  progressAction?: string
  /** False when PM alerts reach no channel. */
  alerts?: boolean
  onSubmitted: (operation: string, next?: FollowUp) => void; onOpenPm: (port: number) => void; pmOpening?: { port: number; draft?: PmDraft; seq: number }; onDirtyChange: (dirty: boolean) => void; onNodeManagement: (trigger: HTMLElement) => void; enabled: boolean | null; settingsError: string; reload: (manual?: boolean) => Promise<void>
}) {
  const status = record?.status
  const fetchError = record?.error ?? ""
  const { confirm, discard, confirmation } = useServiceConfirmation()
  const [maintenanceOpen, setMaintenanceOpen] = useState(false)
  const formTrigger = useRef<HTMLElement | null>(null)
  const [error, setError] = useState("")
  const [message, setMessage] = useState("")
  const [submittedOperation, setSubmittedOperation] = useState("")
  const [busy, setBusy] = useState(false)
  const configSequence = useRef(0)
  const [configLoading, setConfigLoading] = useState<number | null>(null)
  const [configError, setConfigError] = useState("")
  const [clientConfig, setClientConfig] = useState<{ text: string; revision?: string; port: number } | null>(null)
  const [form, setForm] = useState<Resource | null>(null)
  const [editing, setEditing] = useState(false)
  const [baseRevision, setBaseRevision] = useState("")
  const [initialForm, setInitialForm] = useState("")
  // PM fields of a new instance, when "同时启用流量统计" is ticked.
  const [withPm, setWithPm] = useState<PmDraft | null>(null)
  const dirty = form !== null && (JSON.stringify(form) !== initialForm || withPm !== null)
  useLayoutEffect(() => { if (module !== "pm" || !status?.bound) onDirtyChange(module !== "pm" && dirty) }, [dirty, module, onDirtyChange, status?.bound])
  const pmDirty = useRef(false)
  const reportPmDirty = useCallback((value: boolean) => { pmDirty.current = value; onDirtyChange(value) }, [onDirtyChange])
  const running = useRef(false)
  const mounted = useRef(true)
  useEffect(() => { mounted.current = true; return () => { mounted.current = false } }, [])
  async function run(action: () => Promise<void>) {
    if (running.current) return
    running.current = true
    setBusy(true); setError(""); setMessage(""); setSubmittedOperation("")
    try { await action(); if (mounted.current) await reload() } catch (e) { if (mounted.current) setError(String(e)) } finally { running.current = false; if (mounted.current) setBusy(false) }
  }
  const view = moduleView(status, module)
  const submittedResult = status?.operations?.find(operation => operation.id === submittedOperation && !["queued", "delivered"].includes(operation.status))
  const draftStale = form !== null && baseRevision !== view?.revision
  const chaining = /^[12]\/2/.test(progress)
  const active = chaining || lifecycleActive(status?.lifecycle) || status?.operations?.some(o => ["queued", "delivered"].includes(o.status))
  const writable = enabled && !settingsError && !fetchError && status?.bound && status.online && status.compatible && view && !status.snapshot?.blocked && !view.blocked && !active && !busy
  const configurationAvailable = clientConfig?.revision === view?.revision && !active && !fetchError && !settingsError && !status?.snapshot?.blocked && !view?.blocked
  async function submit(action: string, payload: object, revision = view?.revision, intent?: ApplyIntent, target: Module = module, next?: FollowUp) {
    if (!writable || !revision) throw new Error("当前配置不可写，请刷新并核实节点状态")
    const id = crypto.randomUUID().replaceAll("-", "")
    await api(`/management/${node}/operations`, { method: "POST", body: JSON.stringify({ id, action, payload, revision, intent, ...(target !== "snell" ? { module: target } : {}) }) })
    onSubmitted(id, next)
    if (mounted.current) { setSubmittedOperation(id); setMessage("操作已提交，正在确认结果。") }
  }
  async function uninstall() {
    if (!await discard(dirty || pmDirty.current)) return
    const operation = await requestUninstall(node, selectedName, status, module, false, onSubmitted, reload, confirm)
    if (operation && mounted.current) {
      setForm(null); onDirtyChange(false)
      setSubmittedOperation(operation)
      setMessage("卸载请求已提交；以节点实际确认结果为准。")
    }
  }
  async function install() {
    const question = module === "pm" ? `在 ${selectedName} 启用流量统计？目标 VPS 需预装 nftables，启用会创建独立的规则表和定时任务。` : `在 ${selectedName} 安装独立的 ${names[module]} 核心？不接管旧服务、不修改防火墙。`
    if (await confirm(question)) await submit("install", {})
  }
  async function showConfiguration(port: number) {
    if (configLoading !== null) return
    if (!await discard(dirty) || !mounted.current) return
    setForm(null)
    if (clientConfig?.port === port && clientConfig.revision === view?.revision) { setClientConfig(null); return }
    const sequence = ++configSequence.current
    setClientConfig(null); setConfigLoading(port); setConfigError("")
    const revision = view?.revision
    try {
      const response = await fetch(`/api/management/${node}/export?format=${module === "sb" ? "share" : "surge"}&port=${port}`, { cache: "no-store", signal: AbortSignal.timeout(15_000) })
      if (!response.ok) throw new Error(await response.text())
      const text = await response.text()
      if (mounted.current && sequence === configSequence.current) setClientConfig({ text, revision, port })
    } catch (e) {
      if (mounted.current && sequence === configSequence.current) setConfigError(`配置读取失败：${String(e)}`)
    } finally { if (mounted.current && sequence === configSequence.current) setConfigLoading(null) }
  }
  function loadDraft(item?: Resource) {
    const next = item ? editable(module, item) : newResource(module, defaultAddress)
    setEditing(!!item); setBaseRevision(view?.revision ?? ""); setInitialForm(JSON.stringify(next)); setForm(next); setWithPm(null)
    configSequence.current++; setConfigLoading(null); setClientConfig(null); setConfigError("")
  }
  function restoreFormFocus() { if (formTrigger.current?.isConnected) formTrigger.current.focus({ preventScroll: true }) }
  async function edit(item?: Resource, trigger?: HTMLElement) {
    if (busy) return
    if (form && editing === !!item && (!item || form.port === item.port)) { await closeForm(); return }
    if (await discard(dirty) && mounted.current) { formTrigger.current = trigger ?? null; loadDraft(item) }
  }
  async function closeForm() {
    if (!busy && await discard(dirty) && mounted.current) { setForm(null); requestAnimationFrame(restoreFormFocus) }
  }
  function showNodeManagement(trigger: HTMLElement) { onNodeManagement(trigger) }
  const pmView = moduleView(status, "pm")
  const pmRule = (port: number) => (pmView?.resources as PmPolicy[] | undefined)?.find(rule => rule.port === port)
  function pmLink(item: Resource, first = false) {
    const linked = !!pmRule(item.port)
    if (!linked && (pmBlocked(status?.snapshot, item.port) || view?.runtime?.[String(item.port)] !== "active")) return null
    const online = pmView?.usage?.[String(item.port)]?.ip_count
    return <>{first ? "" : " · "}<button type="button" className="text-primary underline-offset-2 hover:underline" onClick={() => onOpenPm(item.port)}>{linked ? "已统计流量" : "添加流量统计"}</button>{linked && online != null && ` · 在线 IP ${online}`}</>
  }
  async function remove(item: Resource) {
    const rule = pmRule(item.port)
    const question = rule ? `删除 ${selectedName} 上的 ${item.name}（${item.port}）？将先删除该端口的流量统计规则及计数，再删除实例并停止服务。` : `删除 ${selectedName} 上的 ${item.name}（${item.port}）？将停止对应服务。`
    if (!await confirm(question, { destructive: true })) return
    if (!rule) { await submit("delete", { port: item.port }); return }
    await submit("delete", { port: item.port }, pmView?.revision, undefined, "pm", { module, action: "delete", payload: { port: item.port }, steps: ["删除流量统计规则", "删除实例"], done: "流量统计规则和实例已删除。" })
  }
  async function reloadDraft() {
    if (!view || !form || !await confirm("重新载入最新配置将替换当前草稿，是否继续？") || !mounted.current) return
    const latest = editing ? view.resources.find(item => item.port === form.port) : undefined
    if (editing && !latest) { setForm(null); setMessage("原实例已被删除，草稿已关闭。") }
    else loadDraft(latest)
    setError("")
  }
  function changePort(port: number) {
    if (!form) return
    setForm({ ...form, port, ...(module === "fw" ? { target_port: form.target_port === form.port ? port : form.target_port } : { public_port: form.public_port === form.port ? port : form.public_port }) })
  }
  const feedback = <>
    {record?.refreshed && !record.loading && !fetchError && !settingsError && <p role="status" className={`text-sm ${serviceFeedbackClass("succeeded", "refresh")}`}>配置已刷新</p>}
    {error && <p role="alert" className="text-sm text-destructive">操作失败：{error}</p>}
    {fetchError && <p role="alert" className="text-sm text-destructive">配置读取失败：{fetchError}{status ? " · 显示上次读取的配置，已暂停写入和配置查看。" : ""}</p>}
    {settingsError && <p role="alert" className="text-sm text-destructive">管理设置读取失败：{settingsError} · 已暂停写入和配置查看。</p>}
    {notice && <p role="status" className={`text-sm ${serviceFeedbackClass("warning")}`}>{notice}</p>}
    {progress && <p role="status" className={`text-sm ${serviceFeedbackClass(progressStatus, progressAction)}`} data-testid="chain-progress">{progress}</p>}
    {message && !(submittedOperation && status?.lifecycle?.id === submittedOperation && status.lifecycle.status === "succeeded") && <p role={submittedResult && ["rejected", "rolled_back", "recovery_failed"].includes(submittedResult.status) ? "alert" : "status"} className={`text-sm ${serviceFeedbackClass(submittedResult?.status ?? "pending", submittedResult?.action)}`}>{submittedResult ? `${labels[submittedResult.status] ?? submittedResult.status} · ${submittedResult.message || "请展开操作记录查看详情。"}` : submittedOperation && notice ? "操作已提交，结果尚未确认，请手动刷新。" : message}</p>}
  </>
  const availability = <>
    {enabled === false && <p className="text-sm text-muted-foreground">尚未启用管理写入。后台需要配置 HTTPS 公网地址；现有监控不受此开关控制。</p>}
    {status?.bound && status.online && !status.compatible && <p role="alert" className="text-sm text-destructive">管理组件与后台版本不一致，已暂停写入。节点服务不受影响，请升级组件后继续。</p>}
    {status?.bound && !status.online && <p className="text-sm text-muted-foreground">管理组件离线，配置只读。请检查 VPS 上的管理组件。</p>}
    {status?.snapshot?.blocked && <p role="alert" className="text-sm text-destructive">配置漂移或有未完成恢复，已暂停写入及配置查看。请在 VPS 运行 sudo nodehelm；外部修改需先人工核实。</p>}
    {view?.blocked && !status?.snapshot?.blocked && <p role="alert" className="text-sm text-destructive">此模块配置需要核实，已暂停写入和配置查看。</p>}
    {status?.lifecycle && status.lifecycle.status !== "succeeded" ? <div role="status" className="flex flex-wrap items-center justify-between gap-2 rounded-lg bg-muted/50 px-3 py-2 text-sm" data-testid="lifecycle-summary"><span className={serviceFeedbackClass(status.lifecycle.status, "uninstall")}>{lifecycleLabels[status.lifecycle.status] ?? "卸载结果待确认"} · {status.lifecycle.scope === "all" ? "此节点全部受管服务" : names[status.lifecycle.scope]}</span><Button variant="ghost" onClick={event => showNodeManagement(event.currentTarget)}>查看进度</Button></div> : active && <p role="status" className="text-sm text-muted-foreground">节点有待完成操作，核实结果后可继续修改。</p>}
  </>
  const formPanel = form && <InlinePanel title={editing ? `编辑 · ${view?.resources.find(item => item.port === form.port)?.name ?? form.port}` : module === "fw" ? "新增转发规则" : "新增实例"} testId="service-instance-editor" busy={busy} closable={false} embedded={editing} onClose={() => { void closeForm() }}>
    {feedback}
    <form id="service-instance-form" className="space-y-4" onSubmit={event => {
      event.preventDefault()
      void run(async () => {
        if (draftStale) throw new Error("配置已更新，请先重新载入最新配置。")
        const payload = instancePayload(module, form)
        const pm = !editing && withPm && !pmBlocked(status?.snapshot, form.port) ? pmPayload({ ...withPm, port: payload.port, name: payload.name }) : null
        if (!await confirm(`应用到 ${selectedName} 的 ${form.port}？名称：${payload.name}。${editing ? "该实例将重启。" : "将创建并启动此实例。"}${pm ? `实例运行后应用流量统计规则（${pmSummary(pm)}）。` : ""}`)) return
        const next: FollowUp | undefined = pm ? { module: "pm", action: "apply", payload: pm, intent: "create", running: { module, port: payload.port }, steps: ["创建实例", "应用流量统计规则"], done: "实例和流量统计规则已完成。", draft: pmDraft(pm) } : undefined
        await submit("apply", payload, baseRevision, editing ? "update" : "create", module, next)
        if (mounted.current) { setForm(null); onDirtyChange(false); requestAnimationFrame(restoreFormFocus) }
      })
    }}>
      {draftStale && <div role="alert" className="space-y-2 text-sm"><p>服务器配置已更新，当前草稿已保留。请先重新载入最新配置，再修改并保存。</p><Button type="button" variant="outline" disabled={busy || !!fetchError || !view || record?.loading} onClick={() => { void reloadDraft() }}>重新载入最新配置</Button></div>}
      <fieldset disabled={busy} className="grid gap-3 sm:grid-cols-2">
        <label>{module === "fw" ? "名称／备注" : "名称（可选）"}<Input required={module === "fw"} maxLength={48} placeholder={module === "fw" ? "输入名称" : "留空使用监听端口"} value={form.name} onChange={event => setForm({ ...form, name: event.target.value })} /></label>
        <label>监听端口<Input type="number" required min={1} max={65535} readOnly={editing} className={`block sm:max-w-48 ${editing ? "border-transparent bg-transparent px-0 shadow-none focus-visible:ring-0" : ""}`} value={form.port} onChange={event => { if (!editing) changePort(Number(event.target.value)) }} /></label>
        {module === "sb" && <><label>协议<select aria-label="协议" className="block w-full rounded border bg-background p-2" disabled={editing} value={form.protocol} onChange={event => setForm({ ...form, protocol: event.target.value, sni: "", psk: "" })}><option value="reality">VLESS Reality</option><option value="ss2022">Shadowsocks 2022</option></select></label>{form.protocol === "reality" && <label>SNI 地址<Input list="reality-targets" required autoComplete="off" placeholder="例如：www.microsoft.com" value={form.sni ?? ""} onChange={event => setForm({ ...form, sni: event.target.value })} /><datalist id="reality-targets">{sniChoices.map(sni => <option key={sni} value={sni} />)}</datalist></label>}</>}
        {module === "fw" && <><label>目标 IPv4 地址<Input required value={form.target_host ?? ""} onChange={event => setForm({ ...form, target_host: event.target.value })} /></label><label>目标端口<Input className="block sm:max-w-48" type="number" required min={1} max={65535} value={form.target_port} onChange={event => setForm({ ...form, target_port: Number(event.target.value) })} /></label></>}
      </fieldset>
      {module !== "fw" && <details className="text-sm" open={form.public_port !== form.port}><summary className="cursor-pointer text-muted-foreground">客户端端口（NAT 端口映射时修改）</summary><div className="mt-3 space-y-1 sm:max-w-[calc(50%-0.375rem)]"><label className="block">客户端访问端口<Input className="block sm:max-w-48" type="number" required min={1} max={65535} disabled={busy} aria-describedby="client-port-hint" value={form.public_port} onChange={event => setForm({ ...form, public_port: Number(event.target.value) })} /></label><p id="client-port-hint" className="break-all text-xs text-muted-foreground">{form.host && form.public_port !== form.port ? `${form.host.includes(":") ? `[${form.host}]` : form.host}:${form.public_port || "?"} → 监听 ${form.port || "?"}` : "默认与监听端口相同；写入客户端配置和分享链接。"}</p></div></details>}
      {!editing && <PmOption port={form.port} blocked={pmBlocked(status?.snapshot, form.port)} draft={withPm} busy={busy} onChange={setWithPm} name={form.name} alerts={alerts} />}
      {module !== "fw" && !form.host && <p role="alert" className="text-sm text-destructive">此节点尚无可用的客户端访问地址，请先在节点信息中设置公网地址，再重新打开表单。</p>}
      <div className="flex flex-wrap items-center justify-between gap-3"><p className="text-xs text-muted-foreground">{module === "fw" ? "请确认目标认证与入口限制。" : "凭据自动生成，编辑时保留。"}</p><div className="flex flex-wrap gap-2"><Button type="button" variant="outline" disabled={busy} onClick={() => { void closeForm() }}>取消</Button><Button type="submit" disabled={!writable || draftStale || (module !== "fw" && !form.host)}>保存并应用</Button></div></div>
    </form>
    {confirmation}
  </InlinePanel>
  const panelTools = <>{record?.updatedAt && <span className={`text-xs tabular-nums ${record.refreshed ? serviceFeedbackClass("succeeded", "refresh") : "text-muted-foreground"}`}>{record.refreshed ? "已刷新于 " : "读取于 "}{new Date(record.updatedAt).toLocaleTimeString()}</span>}<Button variant="ghost" className="min-h-11" aria-label="刷新此节点" disabled={busy || record?.loading} onClick={() => { void reload(true) }}><RefreshCw aria-hidden="true" />{record?.loading ? "正在刷新…" : "刷新"}</Button></>
  return <div className="space-y-4" data-testid={`management-selection-${node}-${module}`}>
    {configError && <p role="alert" className="text-sm text-destructive">{configError}</p>}
    <div className="space-y-4" data-testid="instance-config">
      {status?.bound && view?.installed ? <div data-testid="module-maintenance"><CorePanel node={selectedName} module={module} view={view} writable={Boolean(writable)} expanded={maintenanceOpen} toggle={() => setMaintenanceOpen(value => !value)} run={run} submit={submit} uninstall={() => { void run(uninstall) }} tools={panelTools} /></div>
        : <div className="flex flex-wrap items-center justify-between gap-3"><h2 className="font-medium">{names[module]}</h2><div className="flex flex-wrap items-center gap-2">{panelTools}</div></div>}
      {!form && !(module === "pm" && status?.bound && view?.installed) && feedback}{availability}{!form && confirmation}
      {form && (!status?.bound || !view?.installed) && formPanel}
      {!status && !fetchError && <p role="status" className="text-sm text-muted-foreground">正在读取配置…</p>}
      {status && !status.bound && <div className="flex flex-wrap items-center justify-between gap-3 rounded-lg bg-muted/40 p-4"><p className="text-sm text-muted-foreground">节点尚未接入管理。</p><Button onClick={event => showNodeManagement(event.currentTarget)}>接入管理</Button></div>}
      {status?.bound && !status.snapshot && <p className="text-sm text-muted-foreground">等待管理组件首次上报配置，请稍后手动刷新配置。</p>}
      {status?.bound && status.snapshot && !view && <p className="text-sm text-muted-foreground">此管理组件尚未支持 {names[module]}，请升级组件。</p>}
      {status?.bound && view && !view.installed && <div className="flex flex-wrap items-center justify-between gap-4 rounded-lg bg-muted/40 p-4"><div><h3 className="text-sm font-medium">{module === "pm" ? "尚未启用" : "尚未安装"}</h3><p className="mt-1 text-xs text-muted-foreground">{module === "pm" ? "启用后可配置端口计量、配额和到期策略。" : `安装后即可新增${module === "fw" ? "转发规则" : "实例"}。`}</p></div><Button className="min-h-11" disabled={!writable} onClick={() => { void run(install) }}>{module === "pm" ? "启用流量统计" : `安装 ${names[module]}`}</Button></div>}
      {status?.bound && view?.installed && <>
        {module === "pm" ? <PmPanel key={`${node}/${status.lifecycle?.id ?? ""}/${pmOpening?.seq ?? ""}`} node={selectedName} nodeId={node} snapshot={status.snapshot} view={view} writable={Boolean(writable)} busy={busy} feedback={feedback} refreshing={Boolean(record?.loading)} onDirtyChange={reportPmDirty} run={run} submit={submit} opening={pmOpening} alerts={alerts} /> : <div className="space-y-3">
          <div className="flex flex-wrap items-center justify-between gap-3"><span className="text-xs text-muted-foreground">{module === "fw" ? "转发规则" : "实例"} · {view.resources.length}</span><Button className="min-h-11" disabled={!writable} aria-expanded={!!form && !editing} onClick={event => { void edit(undefined, event.currentTarget) }}><Plus aria-hidden="true" />{`新增${module === "fw" ? "转发规则" : "实例"}`}</Button></div>
          {form && (!editing || !view.resources.some(item => item.port === form.port)) && formPanel}
          {view.resources.map(item => <div key={item.port} data-testid={`instance-${item.port}`} className="space-y-3 rounded-lg border bg-background p-3">
            <div className="flex flex-wrap items-center justify-between gap-3"><div className="min-w-0">{(() => {
              const state = status.online ? view.runtime?.[String(item.port)] ?? "unknown" : ""
              const details = (module === "fw" ? [`${item.port} → ${item.target_host}:${item.target_port} · TCP/UDP`]
                : [item.name !== String(item.port) && `端口 ${item.port}`, item.public_port && item.public_port !== item.port && `客户端端口 ${item.public_port}`]).filter(Boolean).join(" · ")
              const link = pmLink(item, !details)
              return <><div className="flex items-center gap-2"><strong className="break-all font-medium">{item.name}</strong><span aria-hidden="true" className={`size-2 shrink-0 rounded-full ${runtimeDots[state] ?? "bg-muted-foreground/40"}`} /><span className="text-xs text-muted-foreground">{state ? runtimeLabels[state] ?? runtimeLabels.unknown : "运行状态待确认"}</span></div>{(details || link) && <p className="mt-1 break-all text-xs text-muted-foreground">{details}{link}</p>}</>
            })()}</div><div className="flex flex-wrap gap-2">
              {(module === "snell" || module === "sb") && <Button variant="outline" disabled={busy || configLoading !== null || !!active || !!fetchError || !!settingsError || status.snapshot?.blocked || view.blocked} aria-expanded={configurationAvailable && clientConfig?.port === item.port} aria-busy={configLoading === item.port} onClick={() => { void showConfiguration(item.port) }}>{configLoading === item.port ? "正在读取…" : configurationAvailable && clientConfig?.port === item.port ? "收起配置" : "显示配置"}</Button>}
              {module === "fw" && <Button variant="outline" disabled={!writable} onClick={() => { void run(async () => { await submit("check", { port: item.port }) }) }}>测试目标 TCP</Button>}
              <Button variant="outline" disabled={!writable} aria-expanded={editing && form?.port === item.port} onClick={event => { void edit(item, event.currentTarget) }}>{editing && form?.port === item.port ? "收起编辑" : "编辑"}</Button>
              <Button variant="ghost" className="text-muted-foreground hover:text-destructive" disabled={!writable} onClick={() => { void run(() => remove(item)) }}>删除</Button>
            </div></div>
            {editing && form?.port === item.port && formPanel}
            {clientConfig?.port === item.port && configurationAvailable && <ClientConfiguration text={clientConfig.text} />}
          </div>)}
          {!view.resources.length && <p className="text-sm text-muted-foreground">暂无配置，可从右上方新增。</p>}
        </div>}
      </>}
    </div>
    <OperationHistory status={status} />
  </div>
}

async function requestUninstall(node: number, name: string, status: Status | undefined, scope: Module | "all", resume: boolean, onSubmitted: (operation: string) => void, reload: () => Promise<void>, confirm: ReturnType<typeof useServiceConfirmation>["confirm"]) {
  if (!status?.bound || !status.online || !status.compatible) throw new Error("节点离线或组件版本不一致，请上线并升级组件后重试。")
  if (resume && (!status.lifecycle || status.lifecycle.scope !== scope)) throw new Error("卸载进度已更新，请刷新后重试。")
  const target = scope === "all" ? "全部受管业务、配置和管理组件" : `${names[scope]} 全部实例、关联受管流量统计规则及核心`
  if (!await confirm(`${resume ? "继续" : "确认"}卸载「${name}」的${target}？已删除内容不会自动恢复。原 monitor 和其他节点不受影响。`, { destructive: true })) return
  const id = resume ? status.lifecycle!.id : crypto.randomUUID().replaceAll("-", "")
  const revisions = Object.fromEntries(modules.map(module => [module, moduleView(status, module)?.revision ?? ""]))
  try {
    await api(`/management/${node}/lifecycle`, { method: "POST", signal: AbortSignal.timeout(15_000), body: JSON.stringify({ id, scope, revisions, resume, ...(resume ? { attempt: status.lifecycle!.attempt } : {}) }) })
  } catch (error) {
    if ((error instanceof ApiError && (error.status === 0 || error.status >= 500 || (error.status >= 200 && error.status < 300))) || (error as Error).name === "AbortError") {
      onSubmitted(id)
      await reload()
      throw new Error(`卸载请求结果未确认，已尝试读取节点进度；请核实结果，勿重复提交。${String(error)}`)
    }
    throw error
  }
  onSubmitted(id)
  return id
}

function LifecycleProgress({ status, busy, fetchError, resume }: { status?: Status; busy: boolean; fetchError: boolean; resume: () => void }) {
  const lifecycle = status?.lifecycle
  if (!lifecycle || lifecycle.status === "succeeded") return null
  return <div className="space-y-2 rounded-lg bg-muted/40 p-3" role="status" data-testid="lifecycle-progress"><h3 className={`text-sm font-medium ${serviceFeedbackClass(lifecycle.status, "uninstall")}`}>{lifecycleLabels[lifecycle.status] ?? "卸载结果待确认"} · {lifecycle.scope === "all" ? "此节点全部受管服务" : names[lifecycle.scope]}</h3><p className={`text-sm ${serviceFeedbackClass(lifecycle.status, "uninstall")}`}>{lifecycle.message}</p>{["paused", "interrupted"].includes(lifecycle.status) && <><p className="text-xs text-muted-foreground">已完成步骤保留，不会自动恢复已删除业务。请先排除错误；失联不代表卸载成功。</p>{lifecycle.status === "paused" || lifecycle.underlying_status === "paused" ? <Button variant="outline" disabled={busy || fetchError || !status?.online || !status.bound || !status.compatible} onClick={resume}>继续卸载</Button> : <p className="text-xs text-muted-foreground">等待节点恢复连接后核实进度；请勿重复发起卸载。</p>}</>}</div>
}

function OperationHistory({ status }: { status?: Status }) {
  const count = (status?.operations?.length ?? 0) + (status?.lifecycle ? 1 : 0)
  if (!count) return null
  return <details className="space-y-2 border-t pt-3" data-testid="operation-history"><summary className="cursor-pointer text-xs text-muted-foreground">节点操作记录 · {count} 条</summary>
    {status?.lifecycle && <div className="space-y-1 border-b py-2 text-sm" data-testid="lifecycle-history"><p className={serviceFeedbackClass(status.lifecycle.status, "uninstall")}>{lifecycleLabels[status.lifecycle.status] ?? "卸载结果待确认"} · {status.lifecycle.scope === "all" ? "此节点全部受管服务" : names[status.lifecycle.scope]}</p><p className="text-xs text-muted-foreground">{status.lifecycle.message}</p></div>}
    {status?.operations?.map(operation => <OperationRow key={operation.id} operation={operation} />)}
  </details>
}

export function NodeManagement({ node: target, record, notice, enabled, settingsError, onSubmitted, onClose, restoreFocus, reload }: { node: Node; record?: NodeStatus; notice?: string; enabled: boolean | null; settingsError: string; onSubmitted: (operation: string) => void; onClose: () => void; restoreFocus: () => void; reload: (manual?: boolean) => Promise<void> }) {
  const { confirm, confirmation } = useServiceConfirmation()
  const node = target.id
  const selectedName = target.name
  const status = record?.status
  const fetchError = record?.error ?? ""
  const [error, setError] = useState("")
  const [message, setMessage] = useState("")
  const [command, setCommand] = useState("")
  const [submittedLifecycle, setSubmittedLifecycle] = useState("")
  const [busy, setBusy] = useState(false)
  const running = useRef(false)
  const mounted = useRef(true)
  useEffect(() => { mounted.current = true; return () => { mounted.current = false } }, [])
  const active = lifecycleActive(status?.lifecycle) || status?.operations?.some(operation => ["queued", "delivered"].includes(operation.status))
  const blocked = status?.snapshot?.blocked || Object.values(status?.snapshot?.modules ?? {}).some(view => view.blocked)
  const writable = enabled && !settingsError && !fetchError && status?.bound && status.online && status.compatible && !active && !blocked && !busy
  async function run(action: () => Promise<void>) {
    if (running.current) return
    running.current = true; setBusy(true); setError(""); setMessage(""); setSubmittedLifecycle("")
    try { await action(); if (mounted.current) await reload() } catch (failure) { if (mounted.current) setError(String(failure)) } finally { running.current = false; if (mounted.current) setBusy(false) }
  }
  async function uninstall(resume = false) {
    const operation = await requestUninstall(node, selectedName, status, resume ? status!.lifecycle!.scope : "all", resume, onSubmitted, reload, confirm)
    if (operation && mounted.current) { setSubmittedLifecycle(operation); setMessage("卸载请求已提交；以节点实际确认结果为准。") }
  }
  async function enroll() {
    const data = await api<{ site: string; node: number; enrollment: string }>(`/management/${node}/enrollment`, { method: "POST" })
    const server = data.site.replace(/\/$/, "")
    if (!mounted.current) return
    // The hub's enroll.sh checks install.py against its digest; the code is an argument, never in the URL.
    setCommand(`curl -fsS ${quote(server + "/management-files/enroll.sh")} | sh -s -- ${data.enrollment}`)
  }
  return <InlinePanel title={`${selectedName} · 节点管理`} description="此节点的管理绑定、卸载进度和授权管理" testId="node-management-panel" busy={busy} onClose={() => { onClose(); requestAnimationFrame(restoreFocus) }} footer={<Button variant="outline" disabled={busy || record?.loading} onClick={() => { void reload(true) }}>{record?.loading ? "正在刷新…" : "刷新此节点"}</Button>}>
    <div className="space-y-5" data-testid="node-maintenance" data-node-id={node}>
      {record?.refreshed && !record.loading && !fetchError && !settingsError && <p role="status" className={`text-sm ${serviceFeedbackClass("succeeded", "refresh")}`}>配置已刷新</p>}
      {error && <p role="alert" className="text-sm text-destructive">操作失败：{error}</p>}
      {fetchError && <p role="alert" className="text-sm text-destructive">配置读取失败：{fetchError} · 刷新成功后才能继续操作。</p>}
      {settingsError && <p role="alert" className="text-sm text-destructive">管理设置读取失败：{settingsError}</p>}
      {notice && <p role="status" className={`text-sm ${serviceFeedbackClass("warning")}`}>{notice}</p>}
      {message && !(submittedLifecycle && status?.lifecycle?.id === submittedLifecycle && status.lifecycle.status === "succeeded") && <p role="status" className={`text-sm ${serviceFeedbackClass(message === "安装命令已复制" ? "succeeded" : "pending")}`}>{message}</p>}
      {confirmation}
      <dl className="space-y-3 text-sm"><div className="flex justify-between gap-3"><dt className="text-muted-foreground">节点地址</dt><dd className="min-w-0 break-all">{target.ipv4 || target.ip || target.hostname || "暂无地址"}</dd></div><div className="flex justify-between gap-3"><dt className="text-muted-foreground">管理绑定</dt><dd>{!status ? "正在读取…" : status.bound ? "已绑定" : "未绑定"}</dd></div><div className="flex justify-between gap-3"><dt className="text-muted-foreground">状态</dt><dd>{fetchError ? "读取失败" : !status ? "正在读取…" : !status.bound ? "未接入" : status.online ? "上次读取在线" : "上次读取离线"}</dd></div>{status?.last_seen ? <div className="flex flex-wrap justify-between gap-3 text-xs"><dt className="text-muted-foreground">组件最后通信</dt><dd className="tabular-nums">{new Date(status.last_seen * 1000).toLocaleString()}</dd></div> : null}</dl>
      <LifecycleProgress status={status} busy={busy} fetchError={!!fetchError || !!settingsError} resume={() => { void run(() => uninstall(true)) }} />
      {status && !status.bound && <div className="space-y-3"><p className="text-xs text-muted-foreground">已接入过的 VPS 在本机运行 sudo nodehelm rebind，输入后台地址和命令末尾的接入码。</p><Button disabled={!enabled || !!settingsError || !!fetchError || busy || !!active} onClick={() => { void run(enroll) }}>安装并接入此节点</Button></div>}
      {command && !status?.bound && !active && <div className="space-y-3"><p className="text-sm">在目标 VPS 以 root 执行，需要 Python 3.10+、curl、sha256sum 和 systemd。命令十分钟内有效，包含一次性凭据，请勿分享。安装与授权一次完成。执行后点击“刷新此节点”确认接入和首次上报。</p><pre className="max-h-72 overflow-auto rounded bg-muted p-3 text-xs">{command}</pre><Button variant="outline" onClick={() => { void run(async () => { await navigator.clipboard.writeText(command); if (mounted.current) setMessage("安装命令已复制") }) }}>复制命令</Button></div>}
      {status?.bound && <>
        {status.component_current === false && <p className="text-xs text-muted-foreground" data-testid="component-outdated">此节点的管理组件与后台版本不一致或未上报版本。请先升级后台，再在 VPS 执行 sudo python3 /opt/node-manager/install.py upgrade。</p>}
        {!status.compatible && <p role="alert" className="text-sm text-destructive">管理组件与后台版本不一致，请先升级组件。</p>}
        <div className="space-y-3 border-t pt-4"><h3 className="text-sm font-medium">全部卸载</h3><p className="text-xs text-muted-foreground">清理此节点全部受管服务、配置及管理组件。</p><Button variant="outline" className="text-destructive" disabled={!writable} onClick={() => { void run(() => uninstall()) }}>全部卸载此节点</Button>{!status.online && <p className="text-sm text-muted-foreground">节点离线，无法执行卸载；已有请求保持未确认，恢复在线后继续。</p>}</div>
        <details className="space-y-3 border-t pt-3"><summary className="cursor-pointer text-sm text-muted-foreground">紧急撤销授权</summary><p className="text-xs text-muted-foreground">仅应急撤销凭据，不卸载服务；重新接入时在 VPS 运行 sudo nodehelm rebind。</p><Button variant="outline" className="text-destructive" disabled={busy || !!active || !!fetchError || !!settingsError} onClick={() => { void run(async () => { if (!await confirm(`撤销 ${selectedName} 的管理绑定？业务服务保留，重新接入时在 VPS 运行 sudo nodehelm rebind。`, { destructive: true })) return; await api(`/management/${node}`, { method: "DELETE" }) }) }}>撤销管理绑定</Button></details>
      </>}
      {status?.abandonable && <details className="space-y-3 border-t pt-3" data-testid="abandon-node"><summary className="cursor-pointer text-sm text-muted-foreground">节点已不存在？</summary><p className="text-xs text-muted-foreground">此节点已离线超过 10 分钟。仅在 VPS 已销毁、已重装或确定不再恢复时使用：后台放弃待确认的任务和卸载进度并撤销授权。此操作不会清理 VPS 上的任何内容；若 VPS 之后恢复，在本机运行 sudo nodehelm rebind 重新接入。</p><Button variant="outline" className="text-destructive" disabled={busy || !!fetchError || !!settingsError} onClick={() => { void run(async () => { if (!await confirm(`放弃对 ${selectedName} 的管理？此操作不清理 VPS 上的内容。`, { destructive: true, expectedName: selectedName })) return; await api(`/management/${node}/abandon`, { method: "POST", body: JSON.stringify({ name: selectedName }) }) }) }}>确认节点已失效</Button></details>}
      <OperationHistory status={status} />
    </div>
  </InlinePanel>
}

function ClientConfiguration({ text }: { text: string }) {
  const [copyStatus, setCopyStatus] = useState("")
  const [copying, setCopying] = useState(false)
  const mounted = useRef(true)
  useEffect(() => { mounted.current = true; return () => { mounted.current = false } }, [])
  async function copy() {
    setCopying(true); setCopyStatus("")
    try {
      await navigator.clipboard.writeText(text)
      if (mounted.current) setCopyStatus("配置已复制")
    } catch {
      if (mounted.current) setCopyStatus("复制失败，请选中配置文本手动复制。")
    } finally { if (mounted.current) setCopying(false) }
  }
  return <div className="grid min-w-0 items-start gap-3 rounded bg-muted/50 p-3 sm:grid-cols-[minmax(0,1fr)_auto]" data-testid="client-configuration">
    <pre tabIndex={0} aria-label="客户端配置内容" className="min-w-0 whitespace-pre-wrap break-all text-xs leading-relaxed">{text}</pre>
    <Button variant="outline" className="min-h-11 justify-self-end" disabled={copying} onClick={() => { void copy() }}>{copying ? "正在复制…" : copyStatus === "配置已复制" ? "已复制" : "复制"}</Button>
    {copyStatus && copyStatus !== "配置已复制" && <p role="alert" className="text-sm text-destructive sm:col-span-2">{copyStatus}</p>}
  </div>
}

function OperationRow({ operation: o }: { operation: NonNullable<Status["operations"]>[number] }) {
  return <div className="rounded border p-3 text-sm"><span className={serviceFeedbackClass(o.status, o.action)}>{o.module ? names[o.module as Module] ?? o.module : "历史任务"}{o.action ? ` · ${actionLabels[o.action] ?? o.action}` : ""} · {labels[o.status] ?? o.status}</span><p className={serviceFeedbackClass(o.status, o.action)}>{o.message || "请等待管理组件核实，勿重复执行。"}</p><p className="break-all text-xs text-muted-foreground">{new Date(o.created * 1000).toLocaleString()} · {o.id}</p></div>
}

/** The panel's header row: module, core version and its maintenance, with the
 *  read time and refresh passed in as `tools`. */
function CorePanel({ node, module, view, writable, expanded, toggle, run, submit, uninstall, tools }: {
  node: string
  tools: ReactNode
  module: Module
  view: Snapshot
  writable: boolean
  expanded: boolean
  toggle: () => void
  run: (action: () => Promise<void>) => Promise<void>
  submit: (action: CoreAction, payload: object) => Promise<void>
  uninstall: () => void
}) {
  const { confirm, confirmation } = useServiceConfirmation()
  const [selectedVersion, setSelectedVersion] = useState("")
  const core = view.core

  const version = core?.candidates.includes(selectedVersion) ? selectedVersion : core?.candidates[0]
  const previous = core?.previous_version
  const title = names[module]
  function maintain(action: CoreAction, target?: string) {
    void run(async () => {
      const detail = target
        ? `${core?.version} → ${target}。本模块运行中的全部实例将重启，连接会短暂中断。`
        : "将删除本后台安装的核心或规则运行环境。请先清理所有实例、转发规则及相关流量统计规则。"
      if (!await confirm(`在 ${node} 对 ${title} 执行「${actionLabels[action]}」？${detail}`)) return
      await submit(action, target ? { version: target } : {})
    })
  }

  const summary = module === "pm" ? "" : !core ? "升级管理组件后可查看和维护此模块的核心。"
    : `核心 ${core.version ?? "未知"} · ${version ? `可升级到 ${version}` : core.version ? "已是最新" : "请在 VPS 核实核心文件及版本"}`
  return <div className="space-y-3" data-testid="core-maintenance">
    <div className="flex flex-wrap items-center justify-between gap-3">
      <div className="flex min-w-0 flex-wrap items-baseline gap-x-3 gap-y-1"><h2 className="font-medium">{title}</h2>{summary && <span className="text-xs text-muted-foreground">{summary}</span>}</div>
      <div className="flex flex-wrap items-center gap-2">
        {tools}
        {module !== "pm" && version && <>
          {core!.candidates.length > 1 && <select aria-label="可信升级版本" className="rounded border bg-background p-2 text-sm" value={version} disabled={!writable} onChange={e => setSelectedVersion(e.target.value)}>{core!.candidates.map(v => <option key={v} value={v}>{v}</option>)}</select>}
          <Button className="min-h-11" disabled={!writable} onClick={() => maintain("core_upgrade", version)}>升级核心</Button>
        </>}
        {core && <Button variant="ghost" className="min-h-11" aria-expanded={expanded} onClick={toggle}>{expanded ? "收起维护" : "模块维护"}</Button>}
      </div>
    </div>
    {confirmation}
    {expanded && <div className="space-y-3 rounded-lg border bg-background p-3"><div className="flex flex-wrap gap-2">
      {previous && <Button variant="outline" disabled={!writable} onClick={() => maintain("core_rollback", previous)}>回退至 {previous}</Button>}
      <Button variant="outline" disabled={!writable} onClick={uninstall}>卸载 {title}</Button>
    </div>
    {view.resources.length > 0 && <p className="text-sm text-muted-foreground">卸载会先清理关联受管流量统计规则，再删除此模块的实例和核心；其他模块保留。</p>}
    </div>}
  </div>
}

/** "同时启用流量统计" in the new-instance form. The rule takes the instance's
 *  port and name, and is applied once the instance runs. */
function PmOption({ port, name, blocked, draft, busy, alerts, onChange }: { port: number; name: string; blocked: string; draft: PmDraft | null; busy: boolean; alerts: boolean; onChange: (draft: PmDraft | null) => void }) {
  const on = !!draft && !blocked
  return <div className="space-y-3 text-sm">
    <label className="flex items-center gap-2"><input type="checkbox" className="size-4" disabled={busy || !!blocked} checked={on} onChange={event => onChange(event.target.checked ? newPmDraft(port, name) : null)} />同时启用流量统计{blocked && <span className="text-xs text-muted-foreground">（{blocked}）</span>}</label>
    {on && <><PmFields form={draft} setForm={onChange} busy={busy} alerts={alerts} /><p className="text-xs text-muted-foreground">实例运行后自动应用。</p></>}
  </div>
}
