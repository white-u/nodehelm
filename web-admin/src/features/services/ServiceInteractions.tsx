import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState, type ReactNode } from "react"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"

/** A page panel. Forms leave out the "收起" header button, as their own 取消
 *  closes them, and an `embedded` one sits inside its row without a frame. */
export function InlinePanel({ title, description, children, footer, onClose, busy = false, testId, closable = true, embedded = false }: {
  title: string; description?: string; children: ReactNode; footer?: ReactNode; busy?: boolean; testId?: string
  onClose: () => void; closable?: boolean; embedded?: boolean
}) {
  const heading = useRef<HTMLHeadingElement>(null)
  const id = useId()
  useLayoutEffect(() => {
    heading.current?.focus({ preventScroll: true })
    heading.current?.scrollIntoView({ block: "nearest", behavior: "instant" })
  }, [])
  function close() {
    if (busy) return
    onClose()
  }
  return <section aria-labelledby={id} data-testid={testId} className={`min-w-0 ${embedded ? "border-t pt-4" : "rounded-xl border bg-background"}`} onKeyDown={event => {
    if (event.key === "Escape" && !event.defaultPrevented) { event.preventDefault(); close() }
  }}>
    <div className={`flex flex-wrap items-start justify-between gap-3 ${closable ? "border-b p-4" : embedded ? "" : "px-4 pt-4"}`}>
      <div className="min-w-0"><h3 id={id} ref={heading} tabIndex={-1} className={`break-words font-medium outline-none ${closable ? "text-base" : "text-sm"}`}>{title}</h3>{description && <p className="mt-1 text-xs text-muted-foreground">{description}</p>}</div>
      {closable && <Button type="button" variant="ghost" className="min-h-11" disabled={busy} onClick={close}>收起</Button>}
    </div>
    <div className={`min-w-0 space-y-4 ${embedded ? "pt-4" : "p-4"}`}>{children}</div>
    {footer && <div className={`flex flex-wrap justify-end gap-2 [&_button]:min-h-11 ${embedded ? "pt-4" : "border-t p-4"}`}>{footer}</div>}
  </section>
}

type Confirmation = { message: string; destructive?: boolean; expectedName?: string; label?: string }

export function useServiceConfirmation() {
  const [pending, setPending] = useState<(Confirmation & { id: number }) | null>(null)
  const sequence = useRef(0)
  const resolver = useRef<((accepted: boolean) => void) | null>(null)
  const trigger = useRef<HTMLElement | null>(null)
  const confirm = useCallback((message: string, options: Omit<Confirmation, "message"> = {}) => {
    resolver.current?.(false)
    trigger.current = document.activeElement instanceof HTMLElement ? document.activeElement : null
    setPending({ message, ...options, id: ++sequence.current })
    return new Promise<boolean>(resolve => { resolver.current = resolve })
  }, [])
  const settle = useCallback((accepted: boolean) => {
    const resolve = resolver.current
    resolver.current = null
    setPending(null)
    resolve?.(accepted)
    const source = trigger.current
    requestAnimationFrame(() => { if (source?.isConnected && !source.matches(":disabled")) source.focus({ preventScroll: true }) })
  }, [])
  // Leaving a node/module cancels the local decision, never its submitted task.
  useEffect(() => () => { resolver.current?.(false); resolver.current = null }, [])
  const discard = useCallback((dirty: boolean) => !dirty ? Promise.resolve(true) : confirm("有尚未保存的修改，放弃草稿并继续？", { label: "放弃修改", destructive: true }), [confirm])
  return { confirm, discard, confirmation: pending ? <ServiceConfirmation key={pending.id} {...pending} settle={settle} /> : null }
}

function ServiceConfirmation({ message, destructive, expectedName, label = "确认操作", settle }: Confirmation & { settle: (accepted: boolean) => void }) {
  const [typed, setTyped] = useState("")
  const cancel = useRef<HTMLButtonElement>(null)
  const id = useId()
  useLayoutEffect(() => {
    cancel.current?.focus({ preventScroll: true })
    cancel.current?.closest("section")?.scrollIntoView({ block: "nearest", behavior: "instant" })
  }, [])
  return <section role="region" aria-label="操作确认" data-testid="service-confirmation" className={`w-full min-w-0 space-y-3 rounded-lg border p-4 ${destructive ? "border-destructive/30 bg-destructive/5" : "bg-muted/40"}`} onKeyDown={event => {
    if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); settle(false) }
  }}>
    <p id={id} className="break-words text-sm leading-relaxed">{message}</p>
    {expectedName !== undefined && <label className="block space-y-2 text-sm"><span>输入节点名称「{expectedName}」确认</span><Input aria-describedby={id} autoComplete="off" value={typed} onChange={event => setTyped(event.target.value)} /></label>}
    <div className="flex flex-wrap gap-2"><Button ref={cancel} type="button" variant="outline" className="min-h-11" onClick={() => settle(false)}>取消操作</Button><Button type="button" variant={destructive ? "destructive" : "default"} className="min-h-11" disabled={expectedName !== undefined && typed !== expectedName} onClick={() => settle(true)}>{label}</Button></div>
  </section>
}
