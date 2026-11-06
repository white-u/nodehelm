import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react"
import { api, ApiError } from "@/lib/api"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { PmPanel } from "./PortPolicies"
import { type Module, type ApplyIntent, type Resource, type CoreAction, type Snapshot, type Status, type NodeStatus, names, modules, moduleView, lifecycleActive, lifecycleLabels, runtimeLabels, actionLabels, labels, sniChoices, quote, newResource, editable, confirmDiscard } from "./model"

export function ManagementDetail({ node, module, selectedName, record, defaultAddress, notice, onSubmitted, onDirtyChange, enabled, settingsError, reload }: {
  node: number; module: Module; selectedName: string; record?: NodeStatus; defaultAddress: string; notice?: string; onSubmitted: (operation: string) => void; onDirtyChange: (dirty: boolean) => void; enabled: boolean | null; settingsError: string; reload: (manual?: boolean) => Promise<void>
}) {
  const status = record?.status
  const fetchError = record?.error ?? ""
  const [detailTab, setDetailTab] = useState<"config" | "manage">("config")
  const [error, setError] = useState("")
  const [message, setMessage] = useState("")
  const [submittedOperation, setSubmittedOperation] = useState("")
  const [busy, setBusy] = useState(false)
  const [command, setCommand] = useState("")
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
  const writable = enabled && !settingsError && !fetchError && status?.online && status.compatible && view && !status.snapshot?.blocked && !view.blocked && !active && !busy
  async function submit(action: string, payload: object, revision = view?.revision, intent?: ApplyIntent) {
    if (!writable || !revision) throw new Error("当前配置不可写，请刷新并核实节点状态")
    const id = crypto.randomUUID().replaceAll("-", "")
    await api(`/management/${node}/operations`, { method: "POST", body: JSON.stringify({ id, action, payload, revision, intent, ...(module !== "snell" ? { module } : {}) }) })
    onSubmitted(id)
    if (mounted.current) { setSubmittedOperation(id); setMessage("操作已提交，正在确认结果。") }
  }
  async function uninstall(scope: Module | "all", resume = false) {
    if (!status?.online || !status.compatible) throw new Error("节点离线或组件版本不一致，请上线并升级组件后重试。")
    if (!confirmDiscard(dirty || pmDirty.current)) return
    const target = scope === "all" ? "全部受管业务、配置和管理组件" : `${names[scope]} 全部实例、关联受管 PM 规则及核心`
    if (!confirm(`${resume ? "继续" : "确认"}卸载「${selectedName}」的${target}？已删除内容不会自动恢复。原 monitor 和其他节点不受影响。`)) return
    const id = resume ? status.lifecycle!.id : crypto.randomUUID().replaceAll("-", "")
    const revisions = Object.fromEntries(modules.map(name => [name, moduleView(status, name)?.revision ?? ""]))
    try {
      await api(`/management/${node}/lifecycle`, { method: "POST", signal: AbortSignal.timeout(15_000), body: JSON.stringify({ id, scope, revisions, resume, ...(resume ? { attempt: status.lifecycle!.attempt } : {}) }) })
    } catch (e) {
      if ((e instanceof ApiError && (e.status === 0 || e.status >= 500 || (e.status >= 200 && e.status < 300))) || (e as Error).name === "AbortError") {
        onSubmitted(id)
        await reload()
        throw new Error(`卸载请求结果未确认，已尝试读取节点进度；请核实结果，勿重复提交。${String(e)}`)
      }
      throw e
    }
    setForm(null); onDirtyChange(false); onSubmitted(id)
    setMessage("卸载请求已提交；以节点实际确认结果为准。")
  }
  async function enroll() {
    const data = await api<{ site: string; node: number; enrollment: string }>(`/management/${node}/enrollment`, { method: "POST" })
    const res = await fetch("/management-files/manifest.json", { cache: "no-store" })
    if (!res.ok) throw new Error("无法读取安装校验信息")
    const manifest = await res.json()
    const server = data.site.replace(/\/$/, "")
    if (!mounted.current) return
    setCommand(`(set -eu\numask 077\nd=$(mktemp -d)\ntrap 'rm -rf "$d"' EXIT\ncurl --proto '=https' --tlsv1.2 -fsS ${quote(server + "/management-files/install.py")} -o "$d/install.py"\nprintf '%s  %s\\n' ${quote(manifest["install.py"])} "$d/install.py" | sha256sum -c -\npython3 "$d/install.py" --server ${quote(server)} --node ${data.node} <<'NODE_MANAGER_ENROLLMENT'\n${JSON.stringify({ enrollment: data.enrollment })}\nNODE_MANAGER_ENROLLMENT\n)`)
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
  function edit(item?: Resource) {
    if (confirmDiscard(dirty)) loadDraft(item)
  }
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
  return <div className="space-y-4" data-testid={`management-selection-${node}-${module}`}>
    {configError && <p role="alert" className="text-sm text-destructive">{configError}</p>}
    {error && <p role="alert" className="text-sm text-destructive">操作失败：{error}</p>}
    {fetchError && <p role="alert" className="text-sm text-destructive">配置读取失败：{fetchError}{status ? " · 显示上次读取的配置，已暂停写入和配置查看。" : ""}</p>}
    {settingsError && <p role="alert" className="text-sm text-destructive">管理设置读取失败：{settingsError} · 已暂停写入和配置查看。</p>}
    {notice && <p role="status" className="text-sm text-muted-foreground">{notice}</p>}
    {message && <p role="status" className="text-sm">{submittedResult ? `${labels[submittedResult.status] ?? submittedResult.status} · ${submittedResult.message || "请展开操作记录查看详情。"}` : submittedOperation && notice ? "操作已提交，结果尚未确认，请手动刷新。" : message}</p>}
    <div className="space-y-4 rounded-xl border p-4" data-testid="service-card">
    <div className="flex flex-wrap items-center justify-between gap-3"><h2 className="font-medium">{selectedName ? `${selectedName} · ${names[module]}` : "管理设置"}</h2><div className="flex flex-wrap gap-2">
      <Button variant="outline" className="min-w-28" disabled={busy || record?.loading} onClick={() => { void reload(true) }}>{record?.loading ? "正在刷新…" : "刷新此节点"}</Button>
    </div></div>
    {enabled === false && <p className="text-sm text-muted-foreground">尚未启用管理写入。后台需要配置 HTTPS 公网地址；现有监控不受此开关控制。</p>}
      {status?.bound && status.online && !status.compatible && <p role="alert" className="text-destructive">管理组件与后台版本不一致，已暂停写入。节点服务不受影响，请升级组件后继续。</p>}
      {status?.bound && !writable && <p className="text-sm text-muted-foreground">{busy ? "正在提交或刷新，请稍候。" : fetchError || settingsError ? "读取失败，刷新成功后才能修改配置。" : !enabled ? "启用管理写入后才能修改配置。" : !status.online ? "管理组件离线，配置只读。请检查 VPS 上的管理组件。" : active ? "节点有待完成操作，核实结果后可继续修改。" : status.snapshot?.blocked || view?.blocked ? "配置需要核实，已暂停写入。" : !status.snapshot ? "等待管理组件首次上报配置，请稍后手动刷新配置。" : !view ? "该模块尚未上报，升级管理组件后再试。" : ""}</p>}
      {status?.snapshot?.blocked && <p role="alert" className="text-destructive">配置漂移或有未完成恢复，已暂停写入及配置查看。请在 VPS 执行 python3 /opt/node-manager/agent.py status；有恢复记录时执行 recover，外部修改需先人工核实。</p>}
    {status?.lifecycle && status.lifecycle.status !== "succeeded" && <div className="space-y-2 rounded border p-3" role="status" data-testid="lifecycle-progress"><h3 className="text-sm font-medium">{lifecycleLabels[status.lifecycle.status] ?? "卸载结果待确认"} · {status.lifecycle.scope === "all" ? "此节点全部受管服务" : names[status.lifecycle.scope]}</h3><p className="text-sm">{status.lifecycle.message}</p>{["paused", "interrupted"].includes(status.lifecycle.status) && <><p className="text-xs text-muted-foreground">已完成步骤保留，不会自动恢复已删除业务。请先排除错误；失联不代表卸载成功。</p>{(status.lifecycle.status === "paused" || status.lifecycle.underlying_status === "paused") ? <Button variant="outline" disabled={busy || !!fetchError || !status.online || !status.bound} onClick={() => run(() => uninstall(status.lifecycle!.scope, true))}>继续卸载</Button> : <p className="text-xs text-muted-foreground">等待节点恢复连接后核实进度；请勿重复发起卸载。</p>}</>}</div>}
    <div role="tablist" aria-label="节点详情" className="flex gap-1 border-b" onKeyDown={e => {
      if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(e.key)) return
      e.preventDefault()
      const next = e.key === "Home" ? "config" : e.key === "End" ? "manage" : detailTab === "config" ? "manage" : "config"
      setDetailTab(next)
      document.getElementById(`detail-tab-${next}`)?.focus()
    }}>{(["config", "manage"] as const).map(tab => <button key={tab} type="button" role="tab" id={`detail-tab-${tab}`} aria-selected={detailTab === tab} aria-controls={`detail-panel-${tab}`} tabIndex={detailTab === tab ? 0 : -1} onClick={() => setDetailTab(tab)} className={`min-h-11 border-b-2 px-5 text-sm font-medium focus-visible:outline-2 focus-visible:outline-ring ${detailTab === tab ? "border-primary text-foreground" : "border-transparent text-muted-foreground hover:text-foreground"}`}>{tab === "config" ? "配置" : "管理"}</button>)}</div>
    <section hidden={detailTab !== "manage"} role="tabpanel" id="detail-panel-manage" aria-labelledby="detail-tab-manage" className="space-y-4" data-testid="node-maintenance">
      {status?.last_seen ? <p className="text-xs tabular-nums text-muted-foreground">组件最后通信：{new Date(status.last_seen * 1000).toLocaleString()}</p> : null}
      {status?.lifecycle?.status === "succeeded" && <details className="space-y-2 rounded border p-3" data-testid="lifecycle-history"><summary className="cursor-pointer text-sm text-muted-foreground">上次卸载已完成 · {status.lifecycle.scope === "all" ? "此节点全部受管服务" : names[status.lifecycle.scope]}</summary><p className="text-sm">{status.lifecycle.message}</p></details>}
    {!!node && <div className="space-y-4">
      {status && !status.bound ? <Button disabled={!enabled || !!settingsError || !!fetchError || busy || !!active} onClick={() => run(enroll)}>安装并接入此节点</Button> : status?.bound && <div className="rounded border p-3"><h3 className="text-sm font-medium">节点接入</h3><p className="my-2 text-xs text-muted-foreground">安装时自动完成授权。全部卸载只清理此节点由本项目管理的业务与组件。</p><Button variant="outline" disabled={!writable || !status.compatible} onClick={() => run(() => uninstall("all"))}>全部卸载此节点</Button>{!status.compatible && <p className="mt-2 text-xs text-muted-foreground">管理组件与后台版本不一致，请先升级组件。</p>}{status.component_current === false && <p className="mt-2 text-xs text-muted-foreground" data-testid="component-outdated">此节点的管理组件与后台版本不一致或未上报版本。请先升级后台，再在 VPS 执行 sudo python3 /opt/node-manager/install.py upgrade。</p>}{!status.online && <p className="mt-2 text-sm text-destructive">节点离线，无法执行卸载；已有请求保持未确认，恢复在线后继续。</p>}<details className="mt-3"><summary className="cursor-pointer text-xs text-muted-foreground">紧急撤销授权</summary><p className="my-2 text-xs text-muted-foreground">仅应急撤销凭据，不卸载服务；正常移除请用上方全部卸载。</p><Button variant="outline" disabled={busy || !!active || !!fetchError} onClick={() => run(async () => {
        if (!confirm(`撤销 ${selectedName} 的管理绑定？业务服务保留，重新绑定前须在 VPS 执行 unlink。`)) return
        await api(`/management/${node}`, { method: "DELETE" })
      })}>撤销管理绑定</Button></details></div>}
      {status?.abandonable && <div className="space-y-2 rounded border p-3" data-testid="abandon-node"><h3 className="text-sm font-medium">节点已不存在？</h3><p className="text-xs text-muted-foreground">此节点已离线超过 10 分钟。仅在 VPS 已销毁、已重装或确定不再恢复时使用：后台放弃待确认的任务和卸载进度并撤销授权，之后可重新接入或删除节点。此操作不会清理 VPS 上的任何内容；若 VPS 之后恢复，需在本机执行 python3 /opt/node-manager/agent.py unlink 才能重新接入。</p><Button variant="outline" disabled={busy || !!fetchError} onClick={() => run(async () => {
        const typed = prompt(`放弃对该节点的管理？请输入节点名称确认：${selectedName}`)
        if (typed === null) return
        await api(`/management/${node}/abandon`, { method: "POST", body: JSON.stringify({ name: typed }) })
      })}>确认节点已失效</Button></div>}
      {command && !status?.bound && !active && <div className="space-y-2"><p className="text-sm">在目标 VPS 以 root 执行，需要 Python 3.10+、curl、sha256sum 和 systemd。命令十分钟内有效，包含一次性凭据，请勿分享。安装与授权一次完成。仅生成命令不代表已安装；执行后点击「刷新此节点」确认接入和首次上报。</p><pre className="overflow-auto rounded bg-muted p-3 text-xs">{command}</pre><Button variant="outline" onClick={() => run(async () => { await navigator.clipboard.writeText(command); if (mounted.current) setMessage("安装命令已复制") })}>复制命令</Button></div>}
      {view?.installed && <CorePanel
        key={`${node}/${module}`}
        node={selectedName}
        module={module}
        view={view}
        writable={Boolean(writable)}
        run={run}
        submit={submit}
        uninstall={() => run(() => uninstall(module))}
      />}
      {status?.bound && view && !view.installed && <Button disabled={!writable} onClick={() => run(async () => {
        const prompt = module === "pm" ? `在 ${selectedName} 启用 PM？目标 VPS 需预装 nftables，启用会创建独立的规则表和定时任务。` : `在 ${selectedName} 安装独立的 ${names[module]} 核心？不接管旧服务、不修改防火墙。`
        if (confirm(prompt)) await submit("install", {})
      })}>{module === "pm" ? "启用 PM" : `安装 ${names[module]}`}</Button>}
    </div>}
    </section>
    {!!node && <section hidden={detailTab !== "config"} role="tabpanel" id="detail-panel-config" aria-labelledby="detail-tab-config" className="space-y-4" data-testid="instance-config">
      <div><h2 className="font-medium">{module === "pm" ? "端口规则" : module === "fw" ? "转发配置" : "实例配置"} · {names[module]}</h2><p className="mt-1 text-sm text-muted-foreground">{selectedName} · 配置仅应用到此节点</p></div>
      {status && !status.bound && <p className="text-sm text-muted-foreground">请先在“管理”页接入管理组件。</p>}
      {status?.bound && (module === "pm" ?
        <PmPanel key={`${node}/${status.lifecycle?.id ?? ""}`} node={selectedName} snapshot={status?.snapshot} view={view} writable={Boolean(writable)} busy={busy} onDirtyChange={reportPmDirty} run={run} submit={submit} />
        : status?.bound && <div className="space-y-3">
        {!status.snapshot ? <p>等待管理组件首次上报配置，请稍后手动刷新配置。</p> : !view ? <p>此管理组件尚未支持 {names[module]}，请升级组件。不会把未支持状态当作未安装。</p> : !view.installed ? <p className="text-sm text-muted-foreground">请先在“管理”页安装 {names[module]}。</p> : <Button disabled={!writable} onClick={() => edit()}>新增{module === "fw" ? "转发规则" : "实例"}</Button>}
        {view?.resources.map(item => <div key={item.port} data-testid={`instance-${item.port}`} className="flex flex-wrap items-center justify-between gap-3 rounded border p-3"><div><strong>{item.name}</strong><span className="ml-2 text-xs">{runtimeLabels[view.runtime?.[String(item.port)] ?? "unknown"]}</span>{module === "fw" && <p className="break-all text-sm text-muted-foreground">{item.port} → {item.target_host}:{item.target_port} · TCP/UDP</p>}</div><div className="flex flex-wrap gap-2">{(module === "snell" || module === "sb") && <Button variant="outline" disabled={busy || !!active || !!fetchError || !!settingsError || status.snapshot?.blocked || view.blocked} aria-busy={configLoading} onClick={e => { if (configReading.current) return; configTrigger.current = e.currentTarget; void showConfiguration(item.port) }}>显示配置</Button>}{module === "fw" && <Button variant="outline" disabled={!writable} onClick={() => run(async () => { await submit("check", { port: item.port }) })}>测试目标 TCP</Button>}<Button variant="outline" disabled={!writable} onClick={() => edit(item)}>编辑</Button><Button variant="outline" disabled={!writable} onClick={() => run(async () => { if (confirm(`删除 ${selectedName} 上的 ${item.name}（${item.port}）？将停止对应服务。`)) await submit("delete", { port: item.port }) })}>删除</Button></div>
          {clientConfig?.port === item.port && clientConfig.revision === view.revision && !active && !fetchError && !settingsError && !status.snapshot?.blocked && !view.blocked && <ConfigurationDialog name={item.name} text={clientConfig.text} onClose={() => setClientConfig(null)} restoreFocus={() => { if (configTrigger.current?.isConnected) configTrigger.current.focus() }} />}
        </div>)}
        {form && <form className="space-y-3 rounded border p-4" onInvalidCapture={e => { const details = (e.target as HTMLElement).closest("details"); if (details) { details.open = true; setAdvanced(true) } }} onSubmit={e => { e.preventDefault(); run(async () => { if (draftStale) throw new Error("配置已更新，请先重新载入最新配置。"); if (editing && !confirm(`保存并应用到 ${selectedName} 的 ${form.port}？该实例将重启。`)) return; await submit("apply", form, baseRevision, editing ? "update" : "create"); setForm(null) }) }}>
          <h2 className="font-medium">{editing ? "编辑" : "新增"}{names[module]} · {selectedName}</h2>
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
          <Button type="submit" disabled={!writable || draftStale}>保存并应用</Button>{" "}<Button variant="outline" type="button" disabled={busy} onClick={() => { if (confirmDiscard(dirty)) setForm(null) }}>取消</Button>
        </form>}
      </div>)}
    </section>}
    </div>
    {!!status?.operations?.length && <details className="space-y-2 rounded border p-3" data-testid="operation-history"><summary className="cursor-pointer text-sm">操作记录 · {status.operations.length} 条 · 最近{labels[status.operations[0].status] ?? status.operations[0].status}</summary>{status.operations.map(o => <OperationRow key={o.id} operation={o} />)}</details>}
  </div>
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
    <h3 className="text-sm font-medium">{module === "pm" ? "PM 模块维护" : `${title} 核心维护 · ${core.version ?? "版本未知"}`}</h3>
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

