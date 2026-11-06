import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react"
import { api, ApiError, type Node } from "@/lib/api"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Dialog, DialogContent, DialogDescription, DialogDrawerContent, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Plus, RefreshCw, X } from "lucide-react"
import { PmPanel } from "./PortPolicies"
import { type Module, type ApplyIntent, type Resource, type CoreAction, type Snapshot, type Status, type NodeStatus, names, modules, moduleView, lifecycleActive, lifecycleLabels, runtimeLabels, actionLabels, labels, sniChoices, quote, newResource, editable, confirmDiscard } from "./model"

export function ManagementDetail({ node, module, selectedName, record, defaultAddress, notice, onSubmitted, onDirtyChange, onNodeManagement, enabled, settingsError, reload }: {
  node: number; module: Module; selectedName: string; record?: NodeStatus; defaultAddress: string; notice?: string; onSubmitted: (operation: string) => void; onDirtyChange: (dirty: boolean) => void; onNodeManagement: (trigger: HTMLElement) => void; enabled: boolean | null; settingsError: string; reload: (manual?: boolean) => Promise<void>
}) {
  const status = record?.status
  const fetchError = record?.error ?? ""
  const [maintenanceOpen, setMaintenanceOpen] = useState(false)
  const maintenanceTrigger = useRef<HTMLButtonElement>(null)
  const formTrigger = useRef<HTMLElement | null>(null)
  const [error, setError] = useState("")
  const [message, setMessage] = useState("")
  const [submittedOperation, setSubmittedOperation] = useState("")
  const [busy, setBusy] = useState(false)
  const configReading = useRef(false)
  const [configLoading, setConfigLoading] = useState(false)
  const [configError, setConfigError] = useState("")
  const [clientConfig, setClientConfig] = useState<{ text: string; revision?: string; port: number } | null>(null)
  const [form, setForm] = useState<Resource | null>(null)
  const [editing, setEditing] = useState(false)
  const [baseRevision, setBaseRevision] = useState("")
  const [initialForm, setInitialForm] = useState("")
  const [advanced, setAdvanced] = useState(false)
  const dirty = form !== null && JSON.stringify(form) !== initialForm
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
    try { await action(); await reload() } catch (e) { if (mounted.current) setError(String(e)) } finally { running.current = false; if (mounted.current) setBusy(false) }
  }
  const view = moduleView(status, module)
  const submittedResult = status?.operations?.find(operation => operation.id === submittedOperation && !["queued", "delivered"].includes(operation.status))
  const draftStale = form !== null && baseRevision !== view?.revision
  const active = lifecycleActive(status?.lifecycle) || status?.operations?.some(o => ["queued", "delivered"].includes(o.status))
  const writable = enabled && !settingsError && !fetchError && status?.bound && status.online && status.compatible && view && !status.snapshot?.blocked && !view.blocked && !active && !busy
  async function submit(action: string, payload: object, revision = view?.revision, intent?: ApplyIntent) {
    if (!writable || !revision) throw new Error("当前配置不可写，请刷新并核实节点状态")
    const id = crypto.randomUUID().replaceAll("-", "")
    await api(`/management/${node}/operations`, { method: "POST", body: JSON.stringify({ id, action, payload, revision, intent, ...(module !== "snell" ? { module } : {}) }) })
    onSubmitted(id)
    if (mounted.current) { setSubmittedOperation(id); setMessage("操作已提交，正在确认结果。") }
  }
  async function uninstall() {
    if (!confirmDiscard(dirty || pmDirty.current)) return
    const operation = await requestUninstall(node, selectedName, status, module, false, onSubmitted, reload)
    if (operation && mounted.current) {
      setForm(null); onDirtyChange(false)
      setSubmittedOperation(operation)
      setMessage("卸载请求已提交；以节点实际确认结果为准。")
    }
  }
  async function install() {
    const prompt = module === "pm" ? `在 ${selectedName} 启用 PM？目标 VPS 需预装 nftables，启用会创建独立的规则表和定时任务。` : `在 ${selectedName} 安装独立的 ${names[module]} 核心？不接管旧服务、不修改防火墙。`
    if (confirm(prompt)) await submit("install", {})
  }
  const configTrigger = useRef<HTMLElement | null>(null)
  async function showConfiguration(port: number) {
    if (configReading.current) return
    configReading.current = true
    setConfigLoading(true); setConfigError("")
    const revision = view?.revision
    try {
      const response = await fetch(`/api/management/${node}/export?format=${module === "sb" ? "sing-box" : "surge"}&port=${port}`, { cache: "no-store", signal: AbortSignal.timeout(15_000) })
      if (!response.ok) throw new Error(await response.text())
      const text = await response.text()
      if (mounted.current) setClientConfig({ text, revision, port })
    } catch (e) {
      if (mounted.current) setConfigError(`配置读取失败：${String(e)}`)
    } finally {
      configReading.current = false
      if (mounted.current) setConfigLoading(false)
    }
  }
  function loadDraft(item?: Resource) {
    const next = item ? editable(module, item) : newResource(module, defaultAddress)
    setEditing(!!item); setBaseRevision(view?.revision ?? ""); setInitialForm(JSON.stringify(next)); setForm(next); setAdvanced(module !== "fw" && !next.host)
  }
  function edit(item?: Resource, trigger?: HTMLElement) {
    if (confirmDiscard(dirty)) { formTrigger.current = trigger ?? null; loadDraft(item) }
  }
  function closeForm() { if (!busy && confirmDiscard(dirty)) setForm(null) }
  function showNodeManagement(trigger: HTMLElement) { setMaintenanceOpen(false); onNodeManagement(trigger) }

  function reloadDraft() {
    if (!view || !form || !confirm("重新载入最新配置将替换当前草稿，是否继续？")) return
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
    {error && <p role="alert" className="text-sm text-destructive">操作失败：{error}</p>}
    {fetchError && <p role="alert" className="text-sm text-destructive">配置读取失败：{fetchError}{status ? " · 显示上次读取的配置，已暂停写入和配置查看。" : ""}</p>}
    {settingsError && <p role="alert" className="text-sm text-destructive">管理设置读取失败：{settingsError} · 已暂停写入和配置查看。</p>}
    {notice && <p role="status" className="text-sm text-muted-foreground">{notice}</p>}
    {message && !(submittedOperation && status?.lifecycle?.id === submittedOperation && status.lifecycle.status === "succeeded") && <p role="status" className="text-sm">{submittedResult ? `${labels[submittedResult.status] ?? submittedResult.status} · ${submittedResult.message || "请展开操作记录查看详情。"}` : submittedOperation && notice ? "操作已提交，结果尚未确认，请手动刷新。" : message}</p>}
  </>
  const availability = <>
    {enabled === false && <p className="text-sm text-muted-foreground">尚未启用管理写入。后台需要配置 HTTPS 公网地址；现有监控不受此开关控制。</p>}
    {status?.bound && status.online && !status.compatible && <p role="alert" className="text-sm text-destructive">管理组件与后台版本不一致，已暂停写入。节点服务不受影响，请升级组件后继续。</p>}
    {status?.bound && !status.online && <p className="text-sm text-muted-foreground">管理组件离线，配置只读。请检查 VPS 上的管理组件。</p>}
    {status?.snapshot?.blocked && <p role="alert" className="text-sm text-destructive">配置漂移或有未完成恢复，已暂停写入及配置查看。请在 VPS 执行 python3 /opt/node-manager/agent.py status；有恢复记录时执行 recover，外部修改需先人工核实。</p>}
    {view?.blocked && !status?.snapshot?.blocked && <p role="alert" className="text-sm text-destructive">此模块配置需要核实，已暂停写入和配置查看。</p>}
    {status?.lifecycle && status.lifecycle.status !== "succeeded" ? <div role="status" className="flex flex-wrap items-center justify-between gap-2 rounded-lg bg-muted/50 px-3 py-2 text-sm" data-testid="lifecycle-summary"><span>{lifecycleLabels[status.lifecycle.status] ?? "卸载结果待确认"} · {status.lifecycle.scope === "all" ? "此节点全部受管服务" : names[status.lifecycle.scope]}</span><Button variant="ghost" onClick={event => showNodeManagement(event.currentTarget)}>查看进度</Button></div> : active && <p role="status" className="text-sm text-muted-foreground">节点有待完成操作，核实结果后可继续修改。</p>}
  </>
  const installButton = <Button className="min-h-11" disabled={!writable} onClick={() => { void run(install) }}>{module === "pm" ? "启用 PM" : `安装 ${names[module]}`}</Button>
  return <div className="space-y-4" data-testid={`management-selection-${node}-${module}`}>
    {configError && <p role="alert" className="text-sm text-destructive">{configError}</p>}
    <div className="overflow-hidden rounded-xl border" data-testid="service-card">
      <div className="flex flex-wrap items-center justify-between gap-3 border-b p-4"><h2 className="min-w-0 break-words font-medium">{selectedName ? `${selectedName} · ${names[module]}` : "管理设置"}</h2><div className="flex flex-wrap gap-2">
        <Button variant="ghost" className="min-h-11" aria-label="刷新此节点" disabled={busy || record?.loading} onClick={() => { void reload(true) }}><RefreshCw aria-hidden="true" />{record?.loading ? "正在刷新…" : "刷新"}</Button>
        <Button ref={maintenanceTrigger} variant="outline" className="min-h-11" disabled={!node} onClick={() => setMaintenanceOpen(true)}>模块维护</Button>
      </div></div>
      <div className="space-y-3 p-4" data-testid="instance-config">
        {/* An open drawer shows these itself; one copy keeps screen readers from announcing them twice. */}
        {!maintenanceOpen && !form && <>{feedback}{availability}</>}
        {!status && !fetchError && <p role="status" className="text-sm text-muted-foreground">正在读取配置…</p>}
        {status && !status.bound && <div className="flex flex-wrap items-center justify-between gap-3 rounded-lg bg-muted/40 p-4"><p className="text-sm text-muted-foreground">节点尚未接入管理。</p><Button onClick={event => showNodeManagement(event.currentTarget)}>接入管理</Button></div>}
        {status?.bound && !status.snapshot && <p className="text-sm text-muted-foreground">等待管理组件首次上报配置，请稍后手动刷新配置。</p>}
        {status?.bound && status.snapshot && !view && <p className="text-sm text-muted-foreground">此管理组件尚未支持 {names[module]}，请升级组件。不会把未支持状态当作未安装。</p>}
        {status?.bound && view && !view.installed && <div className="flex flex-wrap items-center justify-between gap-4 rounded-lg bg-muted/40 p-4"><div><h3 className="text-sm font-medium">{module === "pm" ? "尚未启用" : "尚未安装"}</h3><p className="mt-1 text-xs text-muted-foreground">{module === "pm" ? "启用后可配置端口计量、配额和到期策略。" : `安装后即可新增${module === "fw" ? "转发规则" : "实例"}。`}</p></div>{installButton}</div>}
        {status?.bound && view?.installed && (module === "pm" ?
          <PmPanel key={`${node}/${status.lifecycle?.id ?? ""}`} node={selectedName} snapshot={status.snapshot} view={view} writable={Boolean(writable)} busy={busy} feedback={feedback} refreshing={Boolean(record?.loading)} reload={() => reload(true)} onDirtyChange={reportPmDirty} run={run} submit={submit} />
          : <div className="space-y-3"><div className="flex flex-wrap items-center justify-between gap-3"><span className="text-xs text-muted-foreground">{module === "fw" ? "转发规则" : "实例"} · {view.resources.length}</span><Button className="min-h-11" disabled={!writable} onClick={event => edit(undefined, event.currentTarget)}><Plus aria-hidden="true" />新增{module === "fw" ? "转发规则" : "实例"}</Button></div>
        {view.resources.map(item => <div key={item.port} data-testid={`instance-${item.port}`} className="flex flex-wrap items-center justify-between gap-3 rounded border p-3"><div><strong className="font-medium">{item.name}</strong><span className="ml-2 text-xs text-muted-foreground">{status?.online ? runtimeLabels[view.runtime?.[String(item.port)] ?? "unknown"] : "运行状态待确认"}</span>{module !== "fw" && <p className="mt-1 text-xs text-muted-foreground">{module === "sb" ? item.protocol === "reality" ? "VLESS Reality · " : "Shadowsocks 2022 · " : ""}端口 {item.port}</p>}{module === "fw" && <p className="break-all text-sm text-muted-foreground">{item.port} → {item.target_host}:{item.target_port} · TCP/UDP</p>}</div><div className="flex flex-wrap gap-2">{(module === "snell" || module === "sb") && <Button variant="outline" disabled={busy || !!active || !!fetchError || !!settingsError || status?.snapshot?.blocked || view.blocked} aria-busy={configLoading} onClick={e => { if (configReading.current) return; configTrigger.current = e.currentTarget; void showConfiguration(item.port) }}>显示配置</Button>}{module === "fw" && <Button variant="outline" disabled={!writable} onClick={() => run(async () => { await submit("check", { port: item.port }) })}>测试目标 TCP</Button>}<Button variant="outline" disabled={!writable} onClick={event => edit(item, event.currentTarget)}>编辑</Button><Button variant="outline" disabled={!writable} onClick={() => run(async () => { if (confirm(`删除 ${selectedName} 上的 ${item.name}（${item.port}）？将停止对应服务。`)) await submit("delete", { port: item.port }) })}>删除</Button></div>
          {clientConfig?.port === item.port && clientConfig.revision === view.revision && !active && !fetchError && !settingsError && !status?.snapshot?.blocked && !view.blocked && <ConfigurationDialog name={item.name} text={clientConfig.text} onClose={() => setClientConfig(null)} restoreFocus={() => { if (configTrigger.current?.isConnected) configTrigger.current.focus() }} />}
        </div>)}

          {!view.resources.length && <p className="text-sm text-muted-foreground">暂无配置，可从右上方新增。</p>}
        </div>)}
      </div>
      <OperationHistory status={status} />
    </div>
    {maintenanceOpen && <ManagementDrawer title={`${selectedName} · ${names[module]} 维护`} description="当前模块的核心版本、升级、回退与卸载" onClose={() => setMaintenanceOpen(false)} restoreFocus={() => maintenanceTrigger.current?.focus()} footer={<><Button variant="outline" disabled={busy || record?.loading} onClick={() => { void reload(true) }}>刷新此节点</Button><Button onClick={() => setMaintenanceOpen(false)}>关闭</Button></>}>
      <div className="space-y-4" data-testid="module-maintenance">{feedback}{availability}
        {!status?.bound ? <Button onClick={event => showNodeManagement(event.currentTarget)}>接入管理</Button> : !view ? <p className="text-sm text-muted-foreground">等待此模块配置上报。</p> : view.installed ? <CorePanel node={selectedName} module={module} view={view} writable={Boolean(writable)} run={run} submit={submit} uninstall={() => { void run(uninstall) }} /> : installButton}
      </div>
    </ManagementDrawer>}
    {form && <ManagementDrawer title={`${editing ? "编辑" : "新增"}${names[module]} · ${selectedName}`} description="配置仅应用到当前节点" onClose={closeForm} restoreFocus={() => { if (formTrigger.current?.isConnected) formTrigger.current.focus(); else maintenanceTrigger.current?.focus() }} footer={<><Button variant="ghost" disabled={busy || record?.loading} onClick={() => { void reload(true) }}>刷新此节点</Button><Button variant="outline" disabled={busy} onClick={closeForm}>取消</Button><Button type="submit" form="service-instance-form" disabled={!writable || draftStale}>保存并应用</Button></>}>
      <div className="space-y-4">{feedback}
        <form id="service-instance-form" className="space-y-4" onInvalidCapture={e => { const details = (e.target as HTMLElement).closest("details"); if (details) { details.open = true; setAdvanced(true) } }} onSubmit={e => { e.preventDefault(); run(async () => { if (draftStale) throw new Error("配置已更新，请先重新载入最新配置。"); if (editing && !confirm(`保存并应用到 ${selectedName} 的 ${form.port}？该实例将重启。`)) return; await submit("apply", form, baseRevision, editing ? "update" : "create"); setForm(null) }) }}>
          {draftStale && <div role="alert" className="space-y-2 text-sm"><p>服务器配置已更新，当前草稿已保留。请先重新载入最新配置，再修改并保存。</p><Button type="button" variant="outline" disabled={busy || !!fetchError || !view || record?.loading} onClick={reloadDraft}>重新载入最新配置</Button></div>}
          <div className="grid gap-3 sm:grid-cols-2">
            <label>名称／备注<Input required maxLength={48} value={form.name} onChange={e => setForm({ ...form, name: e.target.value })} /><span className="text-xs text-muted-foreground">文字、数字、空格、横线和下划线</span></label>
            {module === "sb" && <label>协议<select aria-label="协议" className="block rounded border bg-background p-2" disabled={editing} value={form.protocol} onChange={e => { const protocol = e.target.value; const port = protocol === "reality" ? 443 : 10000 + crypto.getRandomValues(new Uint32Array(1))[0] % 50000; setForm({ ...form, protocol, port, public_port: form.public_port === form.port ? port : form.public_port, name: protocol === "reality" ? "Reality" : "SS2022", sni: protocol === "reality" ? sniChoices[0] : "", psk: "" }) }}><option value="reality">VLESS Vision Reality</option><option value="ss2022">Shadowsocks 2022</option></select></label>}
            <label>监听端口<Input type="number" required min={1} max={65535} disabled={editing} value={form.port} onChange={e => changePort(Number(e.target.value))} /></label>
            {module === "fw" && <><label>目标 IPv4 地址<Input required value={form.target_host ?? ""} onChange={e => setForm({ ...form, target_host: e.target.value })} /></label><label>目标端口<Input type="number" required min={1} max={65535} value={form.target_port} onChange={e => setForm({ ...form, target_port: Number(e.target.value) })} /></label></>}
          </div>
          {module !== "fw" && <details data-testid="advanced-config" open={advanced} onToggle={e => setAdvanced(e.currentTarget.open)}><summary className="cursor-pointer text-sm">高级配置</summary><div className="mt-3 grid gap-3 sm:grid-cols-2">
            {!form.host && <p className="text-sm text-destructive sm:col-span-2">此节点没有可用的公网 IPv4 地址，请填写客户端访问域名或 IP 后保存。</p>}
            <label>客户端访问域名或 IP<Input required value={form.host ?? ""} onChange={e => setForm({ ...form, host: e.target.value })} /></label>
            <label>客户端访问端口<Input type="number" required min={1} max={65535} value={form.public_port} onChange={e => setForm({ ...form, public_port: Number(e.target.value) })} /></label>
            {module === "sb" && form.protocol === "reality" && <label>Reality 目标域名<Input list="reality-targets" required value={form.sni ?? ""} onChange={e => setForm({ ...form, sni: e.target.value })} /><datalist id="reality-targets">{sniChoices.map(s => <option key={s} value={s} />)}</datalist><span className="text-xs text-muted-foreground">保存前验证公网 TLS 1.3 / HTTP/2。凭据自动生成，编辑保留。</span></label>}
            {(module === "snell" || form.protocol === "ss2022") && <label>密钥（留空{editing ? "保留" : "自动生成"}）<Input type="password" autoComplete="new-password" value={form.psk ?? ""} onChange={e => setForm({ ...form, psk: e.target.value })} /></label>}
            {module === "snell" && <label>模式<select className="ml-2 rounded border bg-background p-2" value={form.mode} onChange={e => setForm({ ...form, mode: e.target.value })}><option value="default">default</option><option value="unshaped">unshaped</option></select></label>}
            {module === "sb" && form.protocol === "ss2022" && <p className="text-xs sm:col-span-2">固定算法 2022-blake3-aes-128-gcm，自填密钥需为 16 字节 Base64。</p>}
          </div></details>}
          {module === "fw" && <p className="text-xs text-muted-foreground">FW 不提供访问认证，转发本机或内网服务前请确认目标认证或入口限制。</p>}
          <p className="text-xs text-muted-foreground">保存会重启此实例；监听 IPv4。域名需自行解析，业务端口需自行放行。后台 HTTPS 反代不转发业务端口；本机应用成功不代表公网已连通。</p>
        </form>
      </div>
    </ManagementDrawer>}
  </div>
}

function ManagementDrawer({ title, description, children, footer, onClose, restoreFocus }: { title: string; description: string; children: ReactNode; footer?: ReactNode; onClose: () => void; restoreFocus: () => void }) {
  return <Dialog open onOpenChange={open => { if (!open) onClose() }}>
    <DialogDrawerContent showCloseButton={false} onCloseAutoFocus={event => { event.preventDefault(); restoreFocus() }}>
      <DialogHeader className="shrink-0 border-b p-4 text-left"><div className="flex items-center justify-between gap-3"><DialogTitle className="min-w-0 break-words text-base leading-normal font-medium">{title}</DialogTitle><Button variant="ghost" size="icon" className="size-11" aria-label="关闭面板" onClick={onClose}><X aria-hidden="true" /></Button></div><DialogDescription className="sr-only">{description}</DialogDescription></DialogHeader>
      <div className="min-h-0 flex-1 overflow-y-auto p-4">{children}</div>
      {footer && <div className="flex shrink-0 flex-wrap justify-end gap-2 border-t p-4 [&_button]:min-h-11">{footer}</div>}
    </DialogDrawerContent>
  </Dialog>
}

async function requestUninstall(node: number, name: string, status: Status | undefined, scope: Module | "all", resume: boolean, onSubmitted: (operation: string) => void, reload: () => Promise<void>) {
  if (!status?.bound || !status.online || !status.compatible) throw new Error("节点离线或组件版本不一致，请上线并升级组件后重试。")
  if (resume && (!status.lifecycle || status.lifecycle.scope !== scope)) throw new Error("卸载进度已更新，请刷新后重试。")
  const target = scope === "all" ? "全部受管业务、配置和管理组件" : `${names[scope]} 全部实例、关联受管 PM 规则及核心`
  if (!confirm(`${resume ? "继续" : "确认"}卸载「${name}」的${target}？已删除内容不会自动恢复。原 monitor 和其他节点不受影响。`)) return
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
  return <div className="space-y-2 rounded-lg bg-muted/40 p-3" role="status" data-testid="lifecycle-progress"><h3 className="text-sm font-medium">{lifecycleLabels[lifecycle.status] ?? "卸载结果待确认"} · {lifecycle.scope === "all" ? "此节点全部受管服务" : names[lifecycle.scope]}</h3><p className="text-sm">{lifecycle.message}</p>{["paused", "interrupted"].includes(lifecycle.status) && <><p className="text-xs text-muted-foreground">已完成步骤保留，不会自动恢复已删除业务。请先排除错误；失联不代表卸载成功。</p>{lifecycle.status === "paused" || lifecycle.underlying_status === "paused" ? <Button variant="outline" disabled={busy || fetchError || !status?.online || !status.bound || !status.compatible} onClick={resume}>继续卸载</Button> : <p className="text-xs text-muted-foreground">等待节点恢复连接后核实进度；请勿重复发起卸载。</p>}</>}</div>
}

function OperationHistory({ status }: { status?: Status }) {
  const count = (status?.operations?.length ?? 0) + (status?.lifecycle ? 1 : 0)
  if (!count) return null
  return <details className="mx-4 space-y-2 border-t py-3" data-testid="operation-history"><summary className="cursor-pointer text-xs text-muted-foreground">节点操作记录 · {count} 条</summary>
    {status?.lifecycle && <div className="space-y-1 border-b py-2 text-sm" data-testid="lifecycle-history"><p>{lifecycleLabels[status.lifecycle.status] ?? "卸载结果待确认"} · {status.lifecycle.scope === "all" ? "此节点全部受管服务" : names[status.lifecycle.scope]}</p><p className="text-xs text-muted-foreground">{status.lifecycle.message}</p></div>}
    {status?.operations?.map(operation => <OperationRow key={operation.id} operation={operation} />)}
  </details>
}

export function NodeManagement({ node: target, record, notice, enabled, settingsError, onSubmitted, onClose, restoreFocus, reload }: { node: Node; record?: NodeStatus; notice?: string; enabled: boolean | null; settingsError: string; onSubmitted: (operation: string) => void; onClose: () => void; restoreFocus: () => void; reload: (manual?: boolean) => Promise<void> }) {
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
    try { await action(); await reload() } catch (failure) { if (mounted.current) setError(String(failure)) } finally { running.current = false; if (mounted.current) setBusy(false) }
  }
  async function uninstall(resume = false) {
    const operation = await requestUninstall(node, selectedName, status, resume ? status!.lifecycle!.scope : "all", resume, onSubmitted, reload)
    if (operation && mounted.current) { setSubmittedLifecycle(operation); setMessage("卸载请求已提交；以节点实际确认结果为准。") }
  }
  async function enroll() {
    const data = await api<{ site: string; node: number; enrollment: string }>(`/management/${node}/enrollment`, { method: "POST" })
    const response = await fetch("/management-files/manifest.json", { cache: "no-store" })
    if (!response.ok) throw new Error("无法读取安装校验信息")
    const manifest = await response.json()
    const server = data.site.replace(/\/$/, "")
    if (!mounted.current) return
    setCommand(`(set -eu\numask 077\nd=$(mktemp -d)\ntrap 'rm -rf "$d"' EXIT\ncurl --proto '=https' --tlsv1.2 -fsS ${quote(server + "/management-files/install.py")} -o "$d/install.py"\nprintf '%s  %s\\n' ${quote(manifest["install.py"])} "$d/install.py" | sha256sum -c -\npython3 "$d/install.py" --server ${quote(server)} --node ${data.node} <<'NODE_MANAGER_ENROLLMENT'\n${JSON.stringify({ enrollment: data.enrollment })}\nNODE_MANAGER_ENROLLMENT\n)`)
  }
  return <ManagementDrawer title={`${selectedName} · 节点管理`} description="此节点的管理绑定、卸载进度和授权管理" onClose={onClose} restoreFocus={restoreFocus} footer={<><Button variant="outline" disabled={busy || record?.loading} onClick={() => { void reload(true) }}>{record?.loading ? "正在刷新…" : "刷新此节点"}</Button><Button onClick={onClose}>关闭</Button></>}>
    <div className="space-y-5" data-testid="node-maintenance" data-node-id={node}>
      {error && <p role="alert" className="text-sm text-destructive">操作失败：{error}</p>}
      {fetchError && <p role="alert" className="text-sm text-destructive">配置读取失败：{fetchError} · 刷新成功后才能继续操作。</p>}
      {settingsError && <p role="alert" className="text-sm text-destructive">管理设置读取失败：{settingsError}</p>}
      {notice && <p role="status" className="text-sm text-muted-foreground">{notice}</p>}
      {message && !(submittedLifecycle && status?.lifecycle?.id === submittedLifecycle && status.lifecycle.status === "succeeded") && <p role="status" className="text-sm">{message}</p>}
      <dl className="space-y-3 text-sm"><div className="flex justify-between gap-3"><dt className="text-muted-foreground">节点地址</dt><dd className="min-w-0 break-all">{target.ipv4 || target.ip || target.hostname || "暂无地址"}</dd></div><div className="flex justify-between gap-3"><dt className="text-muted-foreground">管理绑定</dt><dd>{!status ? "正在读取…" : status.bound ? "已绑定" : "未绑定"}</dd></div><div className="flex justify-between gap-3"><dt className="text-muted-foreground">状态</dt><dd>{fetchError ? "读取失败" : !status ? "正在读取…" : !status.bound ? "未接入" : status.online ? "上次读取在线" : "上次读取离线"}</dd></div>{status?.last_seen ? <div className="flex flex-wrap justify-between gap-3 text-xs"><dt className="text-muted-foreground">组件最后通信</dt><dd className="tabular-nums">{new Date(status.last_seen * 1000).toLocaleString()}</dd></div> : null}</dl>
      <LifecycleProgress status={status} busy={busy} fetchError={!!fetchError || !!settingsError} resume={() => { void run(() => uninstall(true)) }} />
      {status && !status.bound && <div className="space-y-3"><p className="text-xs text-muted-foreground">若此前撤销过授权，重新接入前请先在 VPS 执行 python3 /opt/node-manager/agent.py unlink。</p><Button disabled={!enabled || !!settingsError || !!fetchError || busy || !!active} onClick={() => { void run(enroll) }}>安装并接入此节点</Button></div>}
      {command && !status?.bound && !active && <div className="space-y-3"><p className="text-sm">在目标 VPS 以 root 执行，需要 Python 3.10+、curl、sha256sum 和 systemd。命令十分钟内有效，包含一次性凭据，请勿分享。安装与授权一次完成。执行后点击“刷新此节点”确认接入和首次上报。</p><pre className="max-h-72 overflow-auto rounded bg-muted p-3 text-xs">{command}</pre><Button variant="outline" onClick={() => { void run(async () => { await navigator.clipboard.writeText(command); if (mounted.current) setMessage("安装命令已复制") }) }}>复制命令</Button></div>}
      {status?.bound && <>
        {status.component_current === false && <p className="text-xs text-muted-foreground" data-testid="component-outdated">此节点的管理组件与后台版本不一致或未上报版本。请先升级后台，再在 VPS 执行 sudo python3 /opt/node-manager/install.py upgrade。</p>}
        {!status.compatible && <p role="alert" className="text-sm text-destructive">管理组件与后台版本不一致，请先升级组件。</p>}
        <div className="space-y-3 border-t pt-4"><h3 className="text-sm font-medium">全部卸载</h3><p className="text-xs text-muted-foreground">清理此节点全部受管服务、配置及管理组件。</p><Button variant="outline" className="text-destructive" disabled={!writable} onClick={() => { void run(() => uninstall()) }}>全部卸载此节点</Button>{!status.online && <p className="text-sm text-muted-foreground">节点离线，无法执行卸载；已有请求保持未确认，恢复在线后继续。</p>}</div>
        <details className="space-y-3 border-t pt-3"><summary className="cursor-pointer text-sm text-muted-foreground">紧急撤销授权</summary><p className="text-xs text-muted-foreground">仅应急撤销凭据，不卸载服务；重新接入前须在 VPS 执行 unlink。</p><Button variant="outline" className="text-destructive" disabled={busy || !!active || !!fetchError || !!settingsError} onClick={() => { void run(async () => { if (!confirm(`撤销 ${selectedName} 的管理绑定？业务服务保留，重新绑定前须在 VPS 执行 unlink。`)) return; await api(`/management/${node}`, { method: "DELETE" }) }) }}>撤销管理绑定</Button></details>
      </>}
      {status?.abandonable && <details className="space-y-3 border-t pt-3" data-testid="abandon-node"><summary className="cursor-pointer text-sm text-muted-foreground">节点已不存在？</summary><p className="text-xs text-muted-foreground">此节点已离线超过 10 分钟。仅在 VPS 已销毁、已重装或确定不再恢复时使用：后台放弃待确认的任务和卸载进度并撤销授权。此操作不会清理 VPS 上的任何内容；若 VPS 之后恢复，需在本机执行 python3 /opt/node-manager/agent.py unlink 才能重新接入。</p><Button variant="outline" className="text-destructive" disabled={busy || !!fetchError || !!settingsError} onClick={() => { void run(async () => { const typed = prompt(`放弃对该节点的管理？请输入节点名称确认：${selectedName}`); if (typed === null) return; await api(`/management/${node}/abandon`, { method: "POST", body: JSON.stringify({ name: typed }) }) }) }}>确认节点已失效</Button></details>}
      <OperationHistory status={status} />
    </div>
  </ManagementDrawer>
}

function ConfigurationDialog({ name, text, onClose, restoreFocus }: { name: string; text: string; onClose: () => void; restoreFocus: () => void }) {
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
  return <Dialog open onOpenChange={open => { if (!open) onClose() }}>
    <DialogContent showCloseButton={false} className="sm:max-w-2xl motion-reduce:animate-none" data-testid="client-configuration" onCloseAutoFocus={e => { e.preventDefault(); restoreFocus() }}>
      <DialogHeader><DialogTitle className="break-words leading-normal">{name}</DialogTitle><DialogDescription>实例配置 · 包含连接密钥</DialogDescription></DialogHeader>
      <pre tabIndex={0} aria-label="客户端配置内容" className="max-h-[55dvh] min-w-0 overflow-auto rounded bg-muted p-3 text-xs whitespace-pre-wrap break-all">{text}</pre>
      {copyStatus && <p role="status" className="text-sm">{copyStatus}</p>}
      <DialogFooter><Button variant="outline" onClick={onClose}>关闭</Button><Button disabled={copying} onClick={() => { void copy() }}>{copying ? "正在复制…" : "复制配置"}</Button></DialogFooter>
    </DialogContent>
  </Dialog>
}

function OperationRow({ operation: o }: { operation: NonNullable<Status["operations"]>[number] }) {
  return <div className="rounded border p-3 text-sm"><span>{o.module ? names[o.module as Module] ?? o.module : "历史任务"}{o.action ? ` · ${actionLabels[o.action] ?? o.action}` : ""} · {labels[o.status] ?? o.status}</span><p>{o.message || "请等待管理组件核实，勿重复执行。"}</p><p className="break-all text-xs text-muted-foreground">{new Date(o.created * 1000).toLocaleString()} · {o.id}</p></div>
}

function CorePanel({ node, module, view, writable, run, submit, uninstall }: {
  node: string
  module: Module
  view: Snapshot
  writable: boolean
  run: (action: () => Promise<void>) => Promise<void>
  submit: (action: CoreAction, payload: object) => Promise<void>
  uninstall: () => void
}) {
  const [selectedVersion, setSelectedVersion] = useState("")
  const core = view.core
  if (!core) return <p className="text-sm text-muted-foreground">升级管理组件后可查看和维护此模块的核心。</p>

  const version = core.candidates.includes(selectedVersion) ? selectedVersion : core.candidates[0]
  const previous = core.previous_version
  const title = names[module]
  function maintain(action: CoreAction, target?: string) {
    void run(async () => {
      const detail = target
        ? `${core?.version} → ${target}。本模块运行中的全部实例将重启，连接会短暂中断。`
        : "将删除本后台安装的核心或规则运行环境。请先清理所有实例、规则及相关 PM 策略。"
      if (!confirm(`在 ${node} 对 ${title} 执行「${actionLabels[action]}」？${detail}`)) return
      await submit(action, target ? { version: target } : {})
    })
  }

  return <div className="space-y-3 rounded border p-3" data-testid="core-maintenance">
    <h3 className="text-sm font-medium">{module === "pm" ? "PM 模块维护" : `${title} 核心维护`}</h3>
    {module !== "pm" && <>
      <p className="text-sm">当前版本 {core.version ?? "未知"} · 上一版 {previous ?? "暂无"}</p>
      {version ? <div className="flex flex-wrap items-center gap-2">
        <label>可信升级版本 <select
          aria-label="可信升级版本"
          className="rounded border bg-background p-2"
          value={version}
          disabled={!writable}
          onChange={e => setSelectedVersion(e.target.value)}
        >{core.candidates.map(v => <option key={v} value={v}>{v}</option>)}</select></label>
        <Button disabled={!writable} onClick={() => maintain("core_upgrade", version)}>升级核心</Button>
      </div> : <p className="text-sm text-muted-foreground">{core.version ? "暂无更高的可信版本。" : "请在 VPS 核实核心文件及版本。"}</p>}
    </>}
    <div className="flex flex-wrap gap-2">
      {previous && <Button variant="outline" disabled={!writable} onClick={() => maintain("core_rollback", previous)}>回退至 {previous}</Button>}
      <Button variant="outline" disabled={!writable} onClick={uninstall}>卸载 {title}</Button>
    </div>
    {view.resources.length > 0 && <p className="text-sm text-muted-foreground">卸载会先清理关联受管 PM 规则，再删除此模块的实例和核心；其他模块保留。</p>}
  </div>
}

