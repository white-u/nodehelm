import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react"
import { api, type Node } from "@/lib/api"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Server, ArrowDownRight, RefreshCw, Search } from "lucide-react"
import { ManagementDetail, NodeManagement } from "./ServiceDetail"
import { useServiceConfirmation } from "./ServiceInteractions"
import { type Module, type Status, type NodeStatus, names, modules, moduleView, lifecycleActive, defaultHost, readManagement } from "./model"

export function Management({ nodes }: { nodes: Node[] }) {
  const { discard, confirmation } = useServiceConfirmation()
  const [selectedNode, setNode] = useState(0)
  const node = nodes.some(n => n.id === selectedNode) ? selectedNode : nodes[0]?.id ?? 0
  const [module, setModule] = useState<Module>("snell")
  const [search, setSearch] = useState("")
  const [managedNode, setManagedNode] = useState(0)
  const nodeManagementTrigger = useRef<HTMLElement | null>(null)
  const dirty = useRef(false)
  const onDirtyChange = useCallback((value: boolean) => { dirty.current = value }, [])
  const [records, setRecords] = useState<Record<number, NodeStatus>>({})
  const [enabled, setEnabled] = useState<boolean | null>(null)
  const [settingsError, setSettingsError] = useState("")
  const alive = useRef(false)
  const sequences = useRef(new Map<number, number>())
  const pending = useRef(new Map<number, Promise<void>>())
  const settingsSequence = useRef(0)
  const watches = useRef(new Map<number, { deadline: number; operation?: string; timer?: ReturnType<typeof setTimeout> }>())
  const stopped = useRef(new Set<number>())
  const refreshRef = useRef<(id: number) => Promise<void>>(async () => {})
  const [notices, setNotices] = useState<Record<number, string>>({})
  const stopWatching = useCallback((id: number, notice = "", suspend = false) => {
    const watch = watches.current.get(id)
    if (watch?.timer) clearTimeout(watch.timer)
    watches.current.delete(id)
    if (suspend) stopped.current.add(id)
    setNotices(previous => ({ ...previous, [id]: notice }))
  }, [])
  const schedule = useCallback((id: number) => {
    const watch = watches.current.get(id)
    if (!watch) return
    if (watch.timer) clearTimeout(watch.timer)
    if (Date.now() >= watch.deadline) {
      stopWatching(id, "自动确认已达两分钟，结果尚未确认。请手动刷新配置查看结果，勿重复提交。", true)
      return
    }
    watch.timer = setTimeout(() => { void refreshRef.current(id) }, Math.min(3000, watch.deadline - Date.now()))
  }, [stopWatching])
  const submitted = useCallback((id: number, operation: string) => {
    stopWatching(id)
    stopped.current.delete(id)
    watches.current.set(id, { deadline: Date.now() + 120_000, operation })
    schedule(id)
  }, [schedule, stopWatching])
  const detail = useRef<HTMLDivElement>(null)
  const ids = nodes.map(n => n.id).join(",")

  const refreshSettings = useCallback(async () => {
    const sequence = ++settingsSequence.current
    try {
      const settings = await readManagement<{ enabled: boolean }>("/management")
      if (alive.current && sequence === settingsSequence.current) { setEnabled(settings.enabled); setSettingsError("") }
    } catch (e) {
      if (alive.current && sequence === settingsSequence.current) setSettingsError(String(e))
    }
  }, [])
  const refreshNode = useCallback((id: number, force = false, manual = false): Promise<void> => {
    if (manual) { stopWatching(id); stopped.current.delete(id) }
    const running = pending.current.get(id)
    if (running && !force) return running
    const sequence = (sequences.current.get(id) ?? 0) + 1
    sequences.current.set(id, sequence)
    // Background reads retain the current presentation. Only first load or an
    // explicit refresh should show a loading state and disable refresh controls.
    setRecords(previous => ({ ...previous, [id]: { ...previous[id], loading: force || !previous[id]?.status } }))
    const request = (async () => {
      try {
        const status = await readManagement<Status>(`/management/${id}`)
        if (alive.current && sequences.current.get(id) === sequence) {
          setRecords(previous => ({ ...previous, [id]: { status, loading: false, updatedAt: Date.now() } }))
          const active = status.operations?.some(operation => ["queued", "delivered"].includes(operation.status))
          const blocked = status.snapshot?.blocked || Object.values(status.snapshot?.modules ?? {}).some(view => view.blocked)
          const watch = watches.current.get(id)
          if (status.lifecycle && ["paused", "interrupted"].includes(status.lifecycle.status)) stopWatching(id, "卸载尚未完成，请查看管理页中的进度和恢复提示。", true)
          else if (blocked) stopWatching(id, "配置待核实，已停止自动确认。核实后请手动刷新。", true)
          else if (!status.bound && !lifecycleActive(status.lifecycle)) stopWatching(id, watch && status.lifecycle?.status !== "succeeded" ? "管理绑定尚未确认，请安装完成后手动刷新配置。" : "")
          else if (watch?.operation && status.lifecycle?.id === watch.operation && status.lifecycle.status === "succeeded") stopWatching(id)
          else if (watch?.operation && status.operations?.some(operation => operation.id === watch.operation && !["queued", "delivered"].includes(operation.status))) stopWatching(id)
          else if ((active || lifecycleActive(status.lifecycle) || watch?.operation) && !stopped.current.has(id)) {
            if (!watch) watches.current.set(id, { deadline: Date.now() + 120_000 })
            schedule(id)
          } else if (watch) stopWatching(id)
        }
      } catch (e) {
        if (alive.current && sequences.current.get(id) === sequence) {
          setRecords(previous => ({ ...previous, [id]: { ...previous[id], loading: false, error: String(e) } }))
          if (watches.current.has(id)) stopWatching(id, "读取失败，已停止自动确认。请手动刷新配置查看结果。", true)
        }
      } finally {
        if (sequences.current.get(id) === sequence) pending.current.delete(id)
      }
    })()
    pending.current.set(id, request)
    return request
  }, [schedule, stopWatching])
  useLayoutEffect(() => { refreshRef.current = refreshNode }, [refreshNode])
  const refreshAll = useCallback(async (manual = false) => {
    const queue = ids ? ids.split(",").map(Number) : []
    // Existing per-node APIs, bounded so a large fleet cannot flood the hub.
    await Promise.all(Array.from({ length: Math.min(3, queue.length) }, async () => {
      while (alive.current && queue.length) await refreshNode(queue.shift()!, manual, manual)
    }))
  }, [ids, refreshNode])
  const invalidate = useCallback(() => {
    alive.current = false
    settingsSequence.current++
    sequences.current.forEach((sequence, id) => sequences.current.set(id, sequence + 1))
    pending.current.clear()
    watches.current.forEach(watch => { if (watch.timer) clearTimeout(watch.timer) })
    watches.current.clear()
  }, [])
  useEffect(() => { alive.current = true; return invalidate }, [invalidate])
  useEffect(() => {
    const first = setTimeout(() => { void refreshSettings(); void refreshAll() }, 0)
    return () => clearTimeout(first)
  }, [module, refreshAll, refreshSettings])
  useEffect(() => {
    const first = setTimeout(() => { if (node) void refreshNode(node) }, 0)
    return () => clearTimeout(first)
  }, [node, refreshNode])
  async function select(id: number, nextModule = module, scroll = true) {
    const changing = id !== node || nextModule !== module
    if (changing && !await discard(dirty.current)) return false
    if (changing) dirty.current = false
    setNode(id); setModule(nextModule)
    setManagedNode(0)
    if (scroll) requestAnimationFrame(() => {
      detail.current?.focus({ preventScroll: true })
      detail.current?.scrollIntoView({ behavior: window.matchMedia("(prefers-reduced-motion: reduce)").matches ? "instant" : "smooth", block: "start" })
    })
    return true
  }
  const query = search.trim().toLocaleLowerCase()
  const shown = nodes.filter(n => [n.name, n.group, n.ip, n.ipv4, n.ipv6, n.hostname].some(value => value?.toLocaleLowerCase().includes(query)))
  const bound = nodes.filter(n => records[n.id]?.status?.bound).length
  const online = nodes.filter(n => records[n.id]?.status?.online && !records[n.id]?.error).length
  const record = records[node]
  const managementTarget = nodes.find(n => n.id === managedNode)
  function openNodeManagement(id: number, trigger: HTMLElement) {
    nodeManagementTrigger.current = trigger
    setManagedNode(previous => previous === id ? 0 : id)
    void refreshNode(id)
  }

  return <section className="space-y-5">
    <div className="flex flex-wrap items-start justify-between gap-4">
      <div><h1 className="text-xl font-semibold tracking-tight">节点服务管理</h1><p className="mt-2 text-sm text-muted-foreground">选择节点中的模块，在下方查看和管理配置。</p></div>
      <Button className="min-h-11" variant="outline" onClick={() => { void refreshSettings(); void refreshAll(true) }} disabled={nodes.length > 0 && nodes.every(n => records[n.id]?.loading)}><RefreshCw aria-hidden="true" />刷新全部</Button>
    </div>
    {enabled === false && <div className="flex flex-wrap items-center gap-3 rounded border p-3"><p className="text-sm">服务管理尚未启用，现有监控不受影响。</p><Button variant="outline" onClick={async () => { try { await api("/management", { method: "POST", body: JSON.stringify({ enabled: true }) }); await refreshSettings() } catch (e) { setSettingsError(String(e)) } }}>启用服务管理</Button></div>}
    <div className="space-y-4" data-testid="management-overview">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div><h2 className="text-sm font-medium">我的节点 <span className="ml-1 text-muted-foreground">{nodes.length}</span></h2><p className="mt-1 text-xs text-muted-foreground">{bound} 台已绑定 · {online} 台上次读取在线</p></div>
        <div className="relative w-full sm:w-64"><Search aria-hidden="true" className="pointer-events-none absolute left-3 top-3.5 size-4 text-muted-foreground" /><Input aria-label="搜索服务器" placeholder="搜索节点、分组或 IP" className="h-11 pl-9" value={search} onChange={e => setSearch(e.target.value)} /></div>
      </div>
      {!nodes.length ? <p className="text-sm text-muted-foreground">请先在节点管理中添加监控节点。</p> : !shown.length ? <p className="text-sm text-muted-foreground">没有匹配的服务器。</p> : <div className="grid gap-3 lg:grid-cols-2 2xl:grid-cols-3">
        {shown.map(n => {
          const row = records[n.id]
          const status = row?.status
          const working = lifecycleActive(status?.lifecycle) || status?.operations?.some(operation => ["queued", "delivered"].includes(operation.status))
          return <article key={n.id} data-testid={`node-card-${n.id}`} className={`min-w-0 space-y-4 rounded-xl border bg-background p-4 transition-colors motion-reduce:transition-none ${node === n.id ? "border-primary/60 ring-1 ring-primary/10" : "border-border"}`}>
            <div className="flex items-start justify-between gap-2"><div className="flex min-w-0 items-start gap-3"><Server aria-hidden="true" className="mt-0.5 size-5 shrink-0 text-muted-foreground" /><div className="min-w-0"><h3 className="break-words font-medium">{n.name}</h3><p className="break-all text-xs text-muted-foreground">{[n.group, n.ipv4 || n.ip || n.hostname].filter(Boolean).join(" · ") || `节点 #${n.id}`}</p></div></div><span className={`shrink-0 rounded px-2 py-1 text-xs ${row?.error ? "bg-destructive/10 text-destructive" : status?.online ? "bg-primary/10 text-primary" : "bg-muted text-muted-foreground"}`}>{row?.error ? "读取失败" : !status ? "读取中" : !status.bound ? (working ? "操作待确认" : "未绑定") : `${status.online ? "上次读取在线" : "上次读取离线"}${working ? " · 操作待确认" : ""}`}</span></div>
            {row?.error && <p className="text-xs text-destructive">{status ? "显示上次配置；刷新成功后可操作。" : row.error}</p>}
            {status?.snapshot?.blocked && <p className="text-xs text-destructive">配置待核实，已暂停写入</p>}
            {status && !status.bound ? <Button variant="outline" onClick={event => openNodeManagement(n.id, event.currentTarget)}>接入管理</Button> : <div className="grid gap-2" aria-label={`${n.name}的模块`}>{modules.map(id => {
              const view = moduleView(status, id)
              const active = status?.operations?.some(o => o.module === id && ["queued", "delivered"].includes(o.status))
              const removing = lifecycleActive(status?.lifecycle) && (status?.lifecycle?.scope === id || status?.lifecycle?.scope === "all")
              const state = !status ? "读取中" : !status.bound ? "待绑定" : !view ? "未上报" : view.blocked ? "待核实" : removing ? "卸载待确认" : active ? "执行中" : !view.installed ? "未安装" : `${view.resources.length} 项配置`
              const selected = node === n.id && module === id
              return <button key={id} type="button" data-testid={`node-module-${n.id}-${id}`} aria-label={`${n.name} · ${names[id]} · ${state}`} aria-pressed={selected} aria-controls="service-detail" onClick={() => select(n.id, id)} className={`flex min-h-11 min-w-0 items-center gap-2 whitespace-nowrap rounded-lg border px-3 py-2.5 text-left text-sm transition-colors motion-reduce:transition-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring ${selected ? "border-primary bg-primary text-primary-foreground" : "border-border bg-background hover:bg-muted"}`}>
                <span className="flex-1 font-medium">{names[id]}</span>
                <span className={`shrink-0 text-xs ${selected ? "text-primary-foreground" : "text-muted-foreground"}`}>{state}</span>
                <ArrowDownRight aria-hidden="true" className={`size-4 shrink-0 ${selected ? "" : "invisible"}`} />
              </button>
            })}</div>}
            <div className="flex flex-wrap items-center justify-between gap-2">
              {row?.updatedAt && <p className="text-xs tabular-nums text-muted-foreground">读取于 {new Date(row.updatedAt).toLocaleTimeString()}</p>}
              <Button id={`node-manage-${n.id}`} variant="ghost" className="ml-auto min-h-11 text-xs" data-testid={`node-manage-${n.id}`} onClick={event => openNodeManagement(n.id, event.currentTarget)}>节点管理</Button>
            </div>
          </article>
        })}
      </div>}
    </div>
    <p className="text-xs text-muted-foreground">显示上次读取的状态；按需刷新，仅操作待确认时自动查询。仅管理本后台创建的服务。</p>
    {managementTarget && <NodeManagement key={managementTarget.id} node={managementTarget} record={records[managementTarget.id]} notice={notices[managementTarget.id]} enabled={enabled} settingsError={settingsError} onSubmitted={operation => submitted(managementTarget.id, operation)} onClose={() => setManagedNode(0)} restoreFocus={() => {
      if (nodeManagementTrigger.current?.isConnected) nodeManagementTrigger.current.focus()
      else {
        const trigger = document.getElementById(`node-manage-${managementTarget.id}`)
        if (trigger) trigger.focus()
        else detail.current?.focus({ preventScroll: true })
      }
    }} reload={async (manual = false) => { await Promise.all([refreshSettings(), refreshNode(managementTarget.id, true, manual)]) }} />}
    {confirmation}
    <div ref={detail} hidden={!!managementTarget} id="service-detail" tabIndex={-1} aria-label={`${nodes.find(n => n.id === node)?.name ?? "节点"} · ${names[module]}配置`} className="scroll-mt-24 focus:outline-none" data-testid="node-detail" data-node-id={node}>
      <ManagementDetail key={`${node}/${module}`} node={node} module={module} selectedName={nodes.find(n => n.id === node)?.name ?? ""} record={record} defaultAddress={defaultHost(nodes.find(n => n.id === node))} notice={notices[node]} onSubmitted={operation => submitted(node, operation)} onDirtyChange={onDirtyChange} onNodeManagement={trigger => openNodeManagement(node, trigger)} enabled={enabled} settingsError={settingsError} reload={async (manual = false) => { await Promise.all([refreshSettings(), node ? refreshNode(node, true, manual) : Promise.resolve()]) }} />
    </div>

  </section>
}

