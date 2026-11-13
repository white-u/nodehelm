import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react"
import { api, type Node } from "@/lib/api"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { RefreshCw, Search } from "lucide-react"
import { ManagementDetail, NodeManagement } from "./ServiceDetail"
import { useServiceConfirmation } from "./ServiceInteractions"
import { type FollowUp, type Module, type PmDraft, type Status, type NodeStatus, names, modules, moduleView, lifecycleActive, lifecycleLabels, defaultHost, readManagement, feedbackSeconds } from "./model"

export function Management({ nodes }: { nodes: Node[] }) {
  const { discard, confirmation } = useServiceConfirmation()
  const [selectedNode, setNode] = useState(0)
  const node = nodes.some(n => n.id === selectedNode) ? selectedNode : nodes[0]?.id ?? 0
  const [module, setModule] = useState<Module>("snell")
  const [search, setSearch] = useState("")
  // Whether the selected module's configuration is expanded under its node's row.
  const [open, setOpen] = useState(false)
  // A node row to bring into view once the expanded configuration has rendered.
  const reveal = useRef(0)
  const [chosenFilter, setFilter] = useState<"bound" | "unbound" | "all">()
  const [managedNode, setManagedNode] = useState(0)
  const nodeManagementTrigger = useRef<HTMLElement | null>(null)
  const dirty = useRef(false)
  const onDirtyChange = useCallback((value: boolean) => { dirty.current = value }, [])
  const [records, setRecords] = useState<Record<number, NodeStatus>>({})
  const [enabled, setEnabled] = useState<boolean | null>(null)
  const [settingsError, setSettingsError] = useState("")
  // Whether PM alerts are pushed anywhere; unknown counts as yes, so no false warning.
  const [alerts, setAlerts] = useState(true)
  const alive = useRef(false)
  const sequences = useRef(new Map<number, number>())
  const pending = useRef(new Map<number, Promise<void>>())
  const settingsSequence = useRef(0)
  const watches = useRef(new Map<number, { deadline: number; operation?: string; timer?: ReturnType<typeof setTimeout> }>())
  const stopped = useRef(new Set<number>())
  const refreshRef = useRef<(id: number) => Promise<void>>(async () => {})
  const [notices, setNotices] = useState<Record<number, string>>({})
  // Two-step changes by node: the step the hub queues after the watched operation
  // (`next`), or that step itself once queued (`final`). The hub runs it either way.
  const chains = useRef(new Map<number, { next?: FollowUp; final?: FollowUp }>())
  // What the page shows of a chain; "1/2" or "2/2" while it runs, which keeps writes locked.
  const [progress, setProgress] = useState<Record<number, { text: string; status: string; action?: string }>>({})
  // PM drafts whose step did not go through, by "node:port", for "添加流量统计".
  const keptDrafts = useRef(new Map<string, PmDraft>())
  const [pmOpening, setPmOpening] = useState<{ node: number; port: number; draft?: PmDraft; seq: number } | null>(null)
  const endChain = useCallback((id: number, text: string, failed = true, outcome = failed ? "failed" : "succeeded") => {
    const chain = chains.current.get(id)
    chains.current.delete(id)
    const step = chain?.next ?? chain?.final
    if (failed && step?.draft) keptDrafts.current.set(`${id}:${step.draft.port}`, step.draft)
    const resume = failed && step?.draft ? "可在实例行点击“添加流量统计”继续，已保留填写的参数。" : ""
    const entry = { text: text + resume, status: outcome, action: step?.action }
    setProgress(previous => ({ ...previous, [id]: entry }))
    // A finished chain's success note goes after a few seconds; failures stay until the next action.
    if (outcome === "succeeded") setTimeout(() => setProgress(previous => {
      if (previous[id] !== entry) return previous
      const { [id]: _, ...rest } = previous
      return rest
    }), feedbackSeconds * 1000)
  }, [])
  const stopWatching = useCallback((id: number, notice = "", suspend = false) => {
    const watch = watches.current.get(id)
    if (watch?.timer) clearTimeout(watch.timer)
    watches.current.delete(id)
    if (suspend) stopped.current.add(id)
    if (suspend && chains.current.has(id)) {
      const { next, final } = chains.current.get(id)!
      endChain(id, next ? `已停止自动确认；${next.steps[0]}成功后后台会继续${next.steps[1]}，请稍后手动刷新查看。` : `${final?.steps[1]}结果尚未确认，请手动刷新查看。`, false, "pending")
    }
    setNotices(previous => ({ ...previous, [id]: notice }))
  }, [endChain])
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
  const submitted = useCallback((id: number, operation: string, next?: FollowUp) => {
    stopWatching(id)
    stopped.current.delete(id)
    chains.current.delete(id)
    if (next) chains.current.set(id, { next })
    setProgress(previous => ({ ...previous, [id]: { text: next ? `1/2 ${next.steps[0]}…` : "", status: "pending" } }))
    watches.current.set(id, { deadline: Date.now() + 120_000, operation })
    schedule(id)
  }, [schedule, stopWatching])
  /** The watched operation has a result. True while the page keeps watching
   *  the second step, which the hub queued when the first succeeded. */
  const advance = useCallback((id: number, status: Status, result: { id: string; status: string; message: string }) => {
    const finish = (step: FollowUp, outcome: typeof result) => {
      if (outcome.status === "succeeded") endChain(id, step.done, false)
      else endChain(id, `${step.steps[1]}未成功：${(outcome.message || "请展开操作记录查看详情").replace(/。$/, "")}。`)
    }
    const chain = chains.current.get(id)
    if (chain?.final) { finish(chain.final, result); return false }
    const next = chain?.next
    if (!next) return false
    if (result.status !== "succeeded") { endChain(id, `${next.steps[0]}未成功，未${next.steps[1]}。`, false, "failed"); return false }
    const follow = status.operations?.find(operation => operation.after === result.id)
    if (!follow) { endChain(id, `${next.steps[0]}已完成，${next.steps[1]}未执行。`); return false }
    chains.current.set(id, { final: next })
    if (!["queued", "delivered"].includes(follow.status)) { finish(next, follow); return false }
    setProgress(previous => ({ ...previous, [id]: { text: `2/2 ${next.steps[1]}…`, status: "pending" } }))
    watches.current.set(id, { deadline: Date.now() + 120_000, operation: follow.id })
    return true
  }, [endChain])
  const detail = useRef<HTMLDivElement>(null)
  const ids = nodes.map(n => n.id).join(",")

  const refreshSettings = useCallback(async () => {
    const sequence = ++settingsSequence.current
    try {
      const settings = await readManagement<{ enabled: boolean; alerts?: boolean }>("/management")
      if (alive.current && sequence === settingsSequence.current) { setEnabled(settings.enabled); setAlerts(settings.alerts !== false); setSettingsError("") }
    } catch (e) {
      if (alive.current && sequence === settingsSequence.current) setSettingsError(String(e))
    }
  }, [])
  /** Starts a read of the node's status; the returned sequence identifies it in `settle`. */
  const begin = useCallback((id: number, force: boolean, manual: boolean) => {
    if (manual) {
      // A refresh restarts the watch but keeps a running chain's operation.
      const operation = chains.current.has(id) ? watches.current.get(id)?.operation : undefined
      stopWatching(id); stopped.current.delete(id)
      if (operation) watches.current.set(id, { deadline: Date.now() + 120_000, operation })
    }
    const sequence = (sequences.current.get(id) ?? 0) + 1
    sequences.current.set(id, sequence)
    // Background reads retain the current presentation. Only first load or an
    // explicit refresh should show a loading state and disable refresh controls.
    setRecords(previous => ({ ...previous, [id]: { ...previous[id], loading: force || !previous[id]?.status } }))
    return sequence
  }, [stopWatching])
  /** Applies a read's outcome unless a later read of the node has started. */
  const settle = useCallback((id: number, sequence: number, manual: boolean, outcome: Status | Error) => {
    if (!alive.current || sequences.current.get(id) !== sequence) return
    if (outcome instanceof Error) {
      setRecords(previous => ({ ...previous, [id]: { ...previous[id], loading: false, error: String(outcome) } }))
      if (watches.current.has(id)) stopWatching(id, "读取失败，已停止自动确认。请手动刷新配置查看结果。", true)
      return
    }
    const status = outcome
    setRecords(previous => ({ ...previous, [id]: { status, loading: false, updatedAt: Date.now(), refreshed: manual } }))
    const active = status.operations?.some(operation => ["queued", "delivered"].includes(operation.status))
    const blocked = status.snapshot?.blocked || Object.values(status.snapshot?.modules ?? {}).some(view => view.blocked)
    const watch = watches.current.get(id)
    const result = status.operations?.find(operation => operation.id === watch?.operation && !["queued", "delivered"].includes(operation.status))
    if (status.lifecycle && ["paused", "interrupted"].includes(status.lifecycle.status)) stopWatching(id, "卸载尚未完成，请查看管理页中的进度和恢复提示。", true)
    else if (blocked) stopWatching(id, "配置待核实，已停止自动确认。核实后请手动刷新。", true)
    else if (!status.bound && !lifecycleActive(status.lifecycle)) stopWatching(id, watch && status.lifecycle?.status !== "succeeded" ? "管理绑定尚未确认，请安装完成后手动刷新配置。" : "")
    else if (watch?.operation && status.lifecycle?.id === watch.operation && status.lifecycle.status === "succeeded") stopWatching(id)
    else if (result) {
      if (advance(id, status, result)) schedule(id)
      else stopWatching(id)
    }
    else if ((active || lifecycleActive(status.lifecycle) || watch?.operation) && !stopped.current.has(id)) {
      if (!watch) watches.current.set(id, { deadline: Date.now() + 120_000 })
      schedule(id)
    } else if (watch) stopWatching(id)
    if (!watches.current.has(id) && chains.current.has(id)) endChain(id, "已停止自动确认，请手动刷新查看后续步骤结果。", false, "pending")
  }, [advance, endChain, schedule, stopWatching])
  const refreshNode = useCallback((id: number, force = false, manual = false): Promise<void> => {
    const running = pending.current.get(id)
    if (running && !force) return running
    const sequence = begin(id, force, manual)
    const request = (async () => {
      try {
        settle(id, sequence, manual, await readManagement<Status>(`/management/${id}`))
      } catch (e) {
        settle(id, sequence, manual, e instanceof Error ? e : new Error(String(e)))
      } finally {
        if (sequences.current.get(id) === sequence) pending.current.delete(id)
      }
    })()
    pending.current.set(id, request)
    return request
  }, [begin, settle])
  useLayoutEffect(() => { refreshRef.current = refreshNode }, [refreshNode])
  // Every node in one request; a node the hub could not read carries its own error.
  const refreshAll = useCallback(async (manual = false) => {
    const queue = ids ? ids.split(",").map(Number) : []
    if (!queue.length) return
    const started = queue.map(id => [id, begin(id, manual, manual)] as const)
    const request = (async () => {
      try {
        const all = await readManagement<Record<string, Status & { error?: string }>>("/management/nodes")
        for (const [id, sequence] of started) {
          const entry = all[id]
          settle(id, sequence, manual, !entry ? new Error("节点不存在，可能已被删除") : entry.error ? new Error(entry.error) : entry)
        }
      } catch (e) {
        for (const [id, sequence] of started) settle(id, sequence, manual, e instanceof Error ? e : new Error(String(e)))
      } finally {
        for (const [id, sequence] of started) if (sequences.current.get(id) === sequence) pending.current.delete(id)
      }
    })()
    for (const [id] of started) pending.current.set(id, request)
    await request
  }, [ids, begin, settle])
  const invalidate = useCallback(() => {
    alive.current = false
    settingsSequence.current++
    sequences.current.forEach((sequence, id) => sequences.current.set(id, sequence + 1))
    pending.current.clear()
    watches.current.forEach(watch => { if (watch.timer) clearTimeout(watch.timer) })
    watches.current.clear()
    chains.current.clear()
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
    if (changing) { dirty.current = false; setPmOpening(null) }
    setNode(id); setModule(nextModule); setOpen(true)
    setManagedNode(0)
    if (scroll) reveal.current = id
    return true
  }
  async function collapse() {
    if (!await discard(dirty.current)) return
    dirty.current = false; setPmOpening(null)
    setOpen(false)
  }
  // After the commit, so the row is placed after any row above it has collapsed.
  useEffect(() => {
    const id = reveal.current
    if (!id) return
    reveal.current = 0
    detail.current?.focus({ preventScroll: true })
    document.getElementById(`node-row-${id}`)?.scrollIntoView({ behavior: window.matchMedia("(prefers-reduced-motion: reduce)").matches ? "instant" : "smooth", block: "start" })
  })
  const query = search.trim().toLocaleLowerCase()
  const bound = nodes.filter(n => records[n.id]?.status?.bound).length
  const loaded = nodes.every(n => records[n.id]?.status || records[n.id]?.error)
  // With nothing bound yet, default to 全部 so 接入管理 is reachable.
  const filter = chosenFilter ?? (nodes.length && loaded && !bound ? "all" : "bound")
  const unbound = nodes.filter(n => records[n.id]?.status && !records[n.id]?.status?.bound).length
  // Nodes not read yet, or unreadable, are listed under 全部 only.
  const matches = (n: Node) => filter === "all" || (filter === "bound") === !!records[n.id]?.status?.bound && !!records[n.id]?.status
  const shown = nodes.filter(n => (open && n.id === node) || n.id === managedNode || (matches(n) && [n.name, n.group, ...(n.addresses ?? []).map(item => item.address), n.hostname].some(value => value?.toLocaleLowerCase().includes(query))))
  const online = nodes.filter(n => records[n.id]?.status?.online && !records[n.id]?.error).length
  const record = records[node]
  const managementTarget = nodes.find(n => n.id === managedNode)
  const expanded = managedNode || (open ? node : 0)
  async function openPm(port: number) {
    if (!await select(node, "pm")) return
    const key = `${node}:${port}`
    setPmOpening({ node, port, draft: keptDrafts.current.get(key), seq: Date.now() })
    keptDrafts.current.delete(key)
  }
  function openNodeManagement(id: number, trigger: HTMLElement) {
    // The entry toggles, as the module entries do.
    if (managedNode === id) { setManagedNode(0); return }
    nodeManagementTrigger.current = trigger
    setManagedNode(id)
    void refreshNode(id)
  }

  const nodeManagement = managementTarget && <NodeManagement key={managementTarget.id} node={managementTarget} record={records[managementTarget.id]} notice={notices[managementTarget.id]} enabled={enabled} settingsError={settingsError} onSubmitted={operation => submitted(managementTarget.id, operation)} onClose={() => setManagedNode(0)} restoreFocus={() => {
      if (nodeManagementTrigger.current?.isConnected) nodeManagementTrigger.current.focus()
      else {
        const trigger = document.getElementById(`node-manage-${managementTarget.id}`)
        if (trigger) trigger.focus()
        else detail.current?.focus({ preventScroll: true })
      }
    }} reload={async (manual = false) => { await Promise.all([refreshSettings(), refreshNode(managementTarget.id, true, manual)]) }} read={refreshNode} />
  const moduleDetail = <div ref={detail} hidden={!!managementTarget || !record?.status?.bound} id="service-detail" tabIndex={-1} aria-label={`${nodes.find(n => n.id === node)?.name ?? "节点"} · ${names[module]}配置`} className="border-t bg-muted/30 p-3 focus:outline-none" data-testid="node-detail" data-node-id={node}>
      <ManagementDetail key={`${node}/${module}`} node={node} module={module} selectedName={nodes.find(n => n.id === node)?.name ?? ""} record={record} defaultAddress={defaultHost(nodes.find(n => n.id === node))} notice={notices[node]} progress={progress[node]?.text} progressStatus={progress[node]?.status} progressAction={progress[node]?.action} alerts={alerts} onSubmitted={(operation, next) => submitted(node, operation, next)} onOpenPm={port => { void openPm(port) }} pmOpening={pmOpening?.node === node && module === "pm" ? pmOpening : undefined} onDirtyChange={onDirtyChange} onNodeManagement={trigger => openNodeManagement(node, trigger)} enabled={enabled} settingsError={settingsError} reload={async (manual = false) => { await Promise.all([refreshSettings(), node ? refreshNode(node, true, manual) : Promise.resolve()]) }} />
    </div>

  return <section className="space-y-5">
    <div className="flex flex-wrap items-start justify-between gap-4">
      <div><h1 className="text-xl font-semibold tracking-tight">节点服务管理</h1><p className="mt-2 text-sm text-muted-foreground">选择节点中的模块，在下方查看和管理配置。</p></div>
      <Button className="min-h-11" variant="outline" onClick={() => { void refreshSettings(); void refreshAll(true) }} disabled={nodes.length > 0 && nodes.every(n => records[n.id]?.loading)}><RefreshCw aria-hidden="true" />刷新全部</Button>
    </div>
    {enabled === false && <div className="flex flex-wrap items-center gap-3 rounded border p-3"><p className="text-sm">服务管理尚未启用，现有监控不受影响。</p><Button variant="outline" onClick={async () => { try { await api("/management", { method: "POST", body: JSON.stringify({ enabled: true }) }); await refreshSettings() } catch (e) { setSettingsError(String(e)) } }}>启用服务管理</Button></div>}
    <div className="space-y-4" data-testid="management-overview">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div><h2 className="text-sm font-medium">我的节点 <span className="ml-1 text-muted-foreground">{nodes.length}</span></h2><p className="mt-1 text-xs text-muted-foreground">{online} 台上次读取在线</p></div>
        <div className="flex w-full flex-wrap items-center gap-2 sm:w-auto">
          <div role="group" aria-label="按接入状态筛选" className="flex h-11 w-full rounded-md bg-muted p-1 text-sm sm:w-auto">
            {([["bound", "已接入", bound], ["unbound", "未接入", unbound], ["all", "全部", nodes.length]] as const).map(([value, label, count]) => <button key={value} type="button" aria-pressed={filter === value} onClick={() => setFilter(value)} className={`flex-1 whitespace-nowrap rounded px-3 transition-colors motion-reduce:transition-none focus-visible:outline-2 focus-visible:outline-ring ${filter === value ? "bg-background font-medium shadow-sm" : "text-muted-foreground hover:text-foreground"}`}>{label} <span className="text-muted-foreground">{count}</span></button>)}
          </div>
          <div className="relative w-full sm:w-64"><Search aria-hidden="true" className="pointer-events-none absolute left-3 top-3.5 size-4 text-muted-foreground" /><Input aria-label="搜索服务器" placeholder="搜索节点、分组或 IP" className="h-11 pl-9" value={search} onChange={e => setSearch(e.target.value)} /></div>
        </div>
      </div>
      {!nodes.length ? <p className="text-sm text-muted-foreground">请先在节点管理中添加监控节点。</p> : !shown.length ? <p className="text-sm text-muted-foreground">{query ? "没有匹配的服务器。" : !loaded ? "正在读取节点状态…" : filter === "bound" ? "没有已接入的节点。" : "没有未接入的节点。"}</p> : <div className="grid gap-3">
        {shown.map(n => {
          const row = records[n.id]
          const status = row?.status
          // Only a whole-node uninstall is named here; a module's own work shows on its entry.
          const working = status?.lifecycle?.scope === "all" && lifecycleActive(status.lifecycle) ? lifecycleLabels[status.lifecycle.status] ?? "卸载待确认" : ""
          const [dot, tag, tagClass] = row?.error ? ["bg-destructive", "读取失败", "text-destructive"] : !status ? ["bg-muted-foreground/30", "读取中", "text-muted-foreground"]
            : !status.bound ? ["bg-amber-500", "未绑定", "text-amber-700 dark:text-amber-400"] : status.online ? ["bg-emerald-500", "", ""] : ["bg-muted-foreground/40", "离线", "text-muted-foreground"]
          const heading = <div className="min-w-0 lg:w-52 lg:shrink-0">
            <div className="flex items-center gap-2"><span aria-hidden="true" className={`size-2 shrink-0 rounded-full ${dot}`} /><h3 className="min-w-0 break-words font-medium">{n.name}</h3>{(tag || working) && <span className={`shrink-0 text-xs ${tag ? tagClass : "text-muted-foreground"}`}>{[tag, working].filter(Boolean).join(" · ")}</span>}</div>
            <p className="break-all pl-4 text-xs text-muted-foreground">{[n.group, n.addresses?.[0]?.address || n.hostname].filter(Boolean).join(" · ") || `节点 #${n.id}`}</p>
          </div>
          const entries = status && !status.bound ? <Button variant="outline" aria-expanded={managedNode === n.id} onClick={event => openNodeManagement(n.id, event.currentTarget)}>接入管理</Button> : <div className="grid gap-2 sm:grid-cols-2 xl:grid-cols-4" aria-label={`${n.name}的模块`}>{modules.map(id => {
              const view = moduleView(status, id)
              const active = status?.operations?.some(o => o.module === id && ["queued", "delivered"].includes(o.status))
              const removing = lifecycleActive(status?.lifecycle) && (status?.lifecycle?.scope === id || status?.lifecycle?.scope === "all")
              const state = !status ? "读取中" : !status.bound ? "待绑定" : !view ? "未上报" : view.blocked ? "待核实" : removing ? "卸载待确认" : active ? "执行中" : !view.installed ? "未安装" : `${view.resources.length} 项`
              const selected = open && node === n.id && module === id
              return <button key={id} type="button" data-testid={`node-module-${n.id}-${id}`} aria-label={`${n.name} · ${names[id]} · ${state}`} aria-pressed={selected} aria-controls="service-detail" onClick={() => { void (selected ? collapse() : select(n.id, id)) }} className={`flex min-h-11 min-w-0 items-center gap-2 whitespace-nowrap rounded-md px-3 py-2.5 text-left text-sm transition-colors motion-reduce:transition-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring ${selected ? "bg-primary text-primary-foreground" : "bg-muted/60 hover:bg-muted"}`}>
                <span className={`min-w-0 flex-1 truncate ${state === "未安装" && !selected ? "text-muted-foreground" : "font-medium"}`}>{names[id]}</span>
                <span className={`shrink-0 text-xs ${selected ? "text-primary-foreground" : "text-muted-foreground"}`}>{state}</span>
              </button>
            })}</div>
          // The expanded configuration explains these in full, so the card leaves them out then.
          const warnings = open && node === n.id && !managedNode && status?.bound ? null : <>{row?.error && <p className="text-xs text-destructive">{status ? "显示上次配置；刷新成功后可操作。" : row.error}</p>}{status?.snapshot?.blocked && <p className="text-xs text-destructive">配置待核实，已暂停写入</p>}</>
          const manage = <Button id={`node-manage-${n.id}`} variant="ghost" className="ml-auto min-h-11 shrink-0 text-xs text-muted-foreground" data-testid={`node-manage-${n.id}`} aria-expanded={managedNode === n.id} onClick={event => openNodeManagement(n.id, event.currentTarget)}>节点管理</Button>
          return <div key={n.id} className={`overflow-hidden rounded-xl border bg-background shadow-xs transition-colors motion-reduce:transition-none ${expanded === n.id ? "border-primary/40 ring-1 ring-primary/10" : "hover:border-foreground/20"}`}>
            <article id={`node-row-${n.id}`} data-testid={`node-card-${n.id}`} className="min-w-0 scroll-mt-24 space-y-2 p-3">
              <div className="flex flex-col gap-3 lg:flex-row lg:items-center">
                {heading}
                <div className="min-w-0 flex-1">{entries}</div>
                {manage}
              </div>
              {warnings}
            </article>
            {managementTarget?.id === n.id && <div className="border-t bg-muted/30 p-3">{nodeManagement}</div>}
            {open && node === n.id && <>{moduleDetail}{confirmation}</>}
          </div>
        })}
      </div>}
    </div>
    <p className="text-xs text-muted-foreground">显示上次读取的状态；按需刷新，仅操作待确认时自动查询。仅管理本后台创建的服务。</p>
    {!open && confirmation}
  </section>
}

