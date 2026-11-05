import { useEffect, useId, useRef, useState } from "react"
import { flushSync } from "react-dom"
import { CircleQuestionMark, GripVertical } from "lucide-react"
import { toast } from "sonner"

import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Label } from "@/components/ui/label"
import { Switch } from "@/components/ui/switch"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { api } from "@/lib/api"

// Displaced rows slide from where they were drawn to their new place: each is
// offset back by the distance it moved, then released. Transforms leave layout
// and stacking alone, so hit-testing mid-slide reads layout positions and the
// sticky header stays on top. A slide cut short restarts from where it was drawn.
function slide(rows: HTMLTableSectionElement | null, update: () => void) {
  const before = new Map([...(rows?.rows ?? [])].map((row) => [row, row.getBoundingClientRect().top]))
  for (const row of before.keys()) row.getAnimations().forEach((a) => a.id === "slide" && a.cancel())
  flushSync(update)
  if (matchMedia("(prefers-reduced-motion: reduce)").matches) return
  for (const [row, top] of before) {
    const dy = top - row.getBoundingClientRect().top
    if (dy) row.animate({ transform: [`translateY(${dy}px)`, "none"] }, { id: "slide", duration: 150, easing: "ease-out" })
  }
}

// Drag-to-reorder for a table whose order the hub stores at `/${path}/order`.
// Rows are displaced while the pointer is down and the whole order is saved on
// release, so a filtered table must disable its handles: the rows on screen are
// then not `order`.
//
// Pointer events, not HTML5 drag and drop: a touch never starts a native drag,
// and where Android synthesizes one from a long press, `dropEffect` reads
// "none" on every release, so each drop would count as cancelled.
export function useDragOrder<T extends { id: number }>(items: T[], path: string, reload: () => void) {
  const [manualOrder, setManualOrder] = useState<number[]>([])
  const [dragging, setDragging] = useState<number | null>(null)
  const orderBeforeDrag = useRef<number[]>([])
  const body = useRef<HTMLTableSectionElement | null>(null)
  // The pointer that owns the drag, where it was pressed, and where it last
  // was: a scroll moves rows under a pointer that holds still.
  const pointer = useRef({ id: 0, x: 0, y: 0, y0: 0 })
  // One save in flight at a time, so two quick reorders reach the hub in order.
  const saving = useRef<Promise<unknown>>(Promise.resolve())
  const byId = new Map(items.map((item) => [item.id, item]))
  const orderedIds = new Set(manualOrder)
  const order = [
    ...manualOrder.map((id) => byId.get(id)).filter((item): item is T => Boolean(item)),
    ...items.filter((item) => !orderedIds.has(item.id)),
  ]
  const ids = () => order.map((item) => item.id)

  // Once the hub lists this order, its list is followed again, so a reorder made
  // in another tab appears here instead of being overwritten by the next drag.
  if (dragging === null && manualOrder.length && manualOrder.join() === items.map((item) => item.id).join()) {
    setManualOrder([])
  }

  // Handled on the document by layout position, not by the row under the
  // pointer: a sliding row is drawn away from its place, and the one under the
  // pointer mid-slide is not the one it would displace. Re-attached on every
  // render, since `order` changes as rows are displaced.
  useEffect(() => {
    const rows = body.current
    if (dragging === null || !rows) return
    const p = pointer.current
    // The header row counts as inside: a drag to the top readily overshoots
    // onto it, and a release there would otherwise discard the drag.
    const inside = () => {
      const table = rows.parentElement!.getBoundingClientRect()
      return p.x >= table.left && p.x <= table.right && p.y >= table.top && p.y <= table.bottom
    }
    const at = () => {
      if (!inside()) return
      const list = [...rows.rows]
      // Offsets count from the rows' container, which does not slide.
      const y = p.y - list[0].offsetParent!.getBoundingClientRect().top
      const from = list.findIndex((row) => row.dataset.id === String(dragging))
      const to = list.findIndex((row) => y >= row.offsetTop && y < row.offsetTop + row.offsetHeight)
      if (from < 0 || to < 0 || from === to) return
      // Moved only where the pointer would then rest on the dragged row. Rows
      // differ in height: a short row moved past a tall one would leave the
      // tall one under the pointer, and the two would swap back and forth.
      const target = list[to]
      const height = list[from].offsetHeight
      if (to > from ? y < target.offsetTop + target.offsetHeight - height : y >= target.offsetTop + height) return
      move(dragging, to)
    }
    const onPointer = (e: PointerEvent) => {
      if (e.pointerId !== p.id) return
      p.x = e.clientX
      p.y = e.clientY
      // A move with no button down follows a release the page never saw, as
      // when a context menu took it; the drag would otherwise trail the
      // pointer until the next click saved wherever it was.
      if (e.type === "pointermove" && e.buttons) at()
      else if (e.type === "pointerup" && inside()) save(ids())
      else cancel()
    }
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && cancel()
    const types = ["pointermove", "pointerup", "pointercancel"] as const
    for (const type of types) document.addEventListener(type, onPointer)
    document.addEventListener("keydown", onKey)
    addEventListener("scroll", at)
    return () => {
      for (const type of types) document.removeEventListener(type, onPointer)
      document.removeEventListener("keydown", onKey)
      removeEventListener("scroll", at)
    }
  })

  // While a drag lasts. A touch on the handle never scrolls the page
  // (`touch-none`), so a drag held near the top or bottom edge scrolls it,
  // faster the closer it gets -- only toward the edge it has moved toward, or
  // a press on a row near the edge would scroll rows under a still pointer.
  // The cursor is flagged on the root once per drag: a `:has()` on the dragged
  // row would restyle the whole page on every displacement, 25 times the style
  // work of a drag across 200 nodes.
  useEffect(() => {
    if (dragging === null) return
    const root = document.documentElement
    root.dataset.dragging = ""
    let last = 0
    const step = (now: number) => {
      const edge = Math.min(120, innerHeight / 4)
      const { y, y0 } = pointer.current
      const depth = y > y0 ? Math.max(0, y - innerHeight + edge) : -Math.max(0, edge - y)
      scrollBy(0, (Math.max(-1, Math.min(1, depth / edge)) * 900 * Math.min(32, last && now - last)) / 1000)
      last = now
      frame = requestAnimationFrame(step)
    }
    let frame = requestAnimationFrame(step)
    return () => {
      cancelAnimationFrame(frame)
      delete root.dataset.dragging
    }
  }, [dragging])

  function move(id: number, to: number) {
    const next = [...ids()]
    const from = next.indexOf(id)
    if (from < 0 || to < 0 || to >= next.length || from === to) return
    next.splice(to, 0, ...next.splice(from, 1))
    slide(body.current, () => setManualOrder(next))
    return next
  }

  // Dropped outside the table or cancelled with Escape: the order is restored.
  function cancel() {
    setDragging(null)
    const before = orderBeforeDrag.current
    if (before.length) slide(body.current, () => setManualOrder(before))
  }

  function save(next: number[]) {
    setDragging(null)
    const before = orderBeforeDrag.current
    if (!before.length || next.join() === before.join()) return
    orderBeforeDrag.current = next
    const put = () => api(`/${path}/order`, { method: "PUT", body: JSON.stringify({ ids: next }) })
    // A refusal falls back to whatever the hub holds, which a save queued
    // behind it may still change.
    saving.current = saving.current.then(put).then(reload, (e: Error) => {
      setManualOrder([])
      reload()
      toast.error(e.message)
    })
  }

  return {
    order,
    row: (id: number) => ({
      "data-id": id,
      "data-dragging": dragging === id || undefined,
      // Opaque, with the dragged row beneath the rest: two rows crossing mid-slide
      // would otherwise draw their text over each other. No hover tint during a
      // drag, since the browser keeps it on whichever row reaches the start point.
      className: `relative z-1 bg-card transition-opacity data-[dragging]:z-0 data-[dragging]:opacity-40 ${dragging === null ? "" : "hover:bg-card"}`,
    }),
    handle: (id: number) => ({
      onPointerDown: (e: React.PointerEvent<HTMLButtonElement>) => {
        if (e.currentTarget.disabled || dragging !== null || e.button !== 0) return
        orderBeforeDrag.current = ids()
        body.current = e.currentTarget.closest("tbody")!
        // Captured by the rows' container, which stays put: the handle's row
        // is moved in the document as it is displaced, and that releases a
        // capture it holds, so a release outside the window would go unseen.
        body.current.setPointerCapture(e.pointerId)
        pointer.current = { id: e.pointerId, x: e.clientX, y: e.clientY, y0: e.clientY }
        setDragging(id)
      },
      onKeyDown: (e: React.KeyboardEvent) => {
        const delta = e.key === "ArrowUp" ? -1 : e.key === "ArrowDown" ? 1 : 0
        if (!delta) return
        e.preventDefault()
        orderBeforeDrag.current = ids()
        body.current = e.currentTarget.closest("tbody")
        const next = move(id, ids().indexOf(id) + delta)
        if (next) save(next)
      },
    }),
  }
}

export function DragHandle({ name, disabled, title = "拖动排序", ...events }: React.ComponentProps<"button"> & { name: string }) {
  return (
    <button
      type="button"
      disabled={disabled}
      // A larger target for a finger, and no text selection from a press.
      className="cursor-grab touch-none rounded p-1 text-muted-foreground select-none hover:bg-muted hover:text-foreground active:cursor-grabbing disabled:cursor-default disabled:opacity-40 disabled:hover:bg-transparent pointer-coarse:p-2"
      title={title}
      aria-label={`拖动 ${name} 排序`}
      {...events}
    >
      <GripVertical className="size-4" />
    </button>
  )
}

export function copy(text: string, done = "已复制") {
  // navigator.clipboard exists only in a secure context. Over plain http the
  // copy command still works from a click, the clipboard filled from its event.
  if (!navigator.clipboard) {
    const put = (e: ClipboardEvent) => { e.clipboardData?.setData("text/plain", text); e.preventDefault() }
    document.addEventListener("copy", put)
    const ok = document.execCommand("copy")
    document.removeEventListener("copy", put)
    return ok ? toast.success(done) : toast.error("复制失败")
  }
  navigator.clipboard.writeText(text).then(
    () => toast.success(done),
    () => toast.error("复制失败"),
  )
}

// Ticks items in a searchable list. 全选 and 全不选 act on the rows in view, so a
// search or a filter narrows what they touch: pick a group, then 全选. The
// callers own the search and pass in what it leaves visible.
export function Picker<T extends { id: number }>({ visible, chosen, onPick, disabled = false, toolbar, empty, listClass, title, children }: {
  visible: T[]
  chosen: Set<number>
  onPick: (list: T[], on: boolean) => void
  disabled?: boolean
  toolbar: React.ReactNode
  empty: string
  listClass: string
  title: (item: T) => string
  children: (item: T) => React.ReactNode
}) {
  // The unfiltered list's height, held as its floor: in a centred dialog a
  // shrinking list would move the search box out from under the cursor.
  const [listHeight, setListHeight] = useState(0)
  const visibleChosen = visible.filter((n) => chosen.has(n.id)).length
  return (
    <div className="rounded-lg border">
      <div className="flex flex-wrap items-center gap-1 border-b p-2">
        {toolbar}
        <Button type="button" size="sm" variant="ghost" className="px-2.5" disabled={disabled || visibleChosen === visible.length} onClick={() => onPick(visible, true)}>全选</Button>
        <Button type="button" size="sm" variant="ghost" className="px-2.5" disabled={disabled || visibleChosen === 0} onClick={() => onPick(visible, false)}>全不选</Button>
      </div>
      <div
        ref={(el) => { if (el && !listHeight) setListHeight(el.offsetHeight) }}
        // Capped like the height itself, which a min-height would otherwise
        // override once the viewport shrinks.
        style={{ minHeight: listHeight ? `min(${listHeight}px, 16rem, 40dvh)` : undefined }}
        className={`grid max-h-[min(16rem,40dvh)] content-start gap-0.5 overflow-y-auto p-1.5 ${listClass}`}
      >
        {visible.map((item) => (
          <label key={item.id} title={title(item)} className="flex min-w-0 cursor-pointer items-center gap-2 rounded-md px-2 py-1.5 text-sm hover:bg-muted">
            <input type="checkbox" checked={chosen.has(item.id)} disabled={disabled} onChange={(e) => onPick([item], e.target.checked)} className="shrink-0 accent-primary" />
            {children(item)}
          </label>
        ))}
        {!visible.length && <p className="col-span-full p-2 text-xs text-muted-foreground">{empty}</p>}
      </div>
    </div>
  )
}

// Every command the panel hands out. break-all because a token has no spaces to
// wrap at.
export function Command({ className = "", children }: { className?: string; children: React.ReactNode }) {
  return (
    <pre className={`overflow-auto whitespace-pre-wrap break-all rounded-lg border bg-muted/40 p-3 text-xs leading-relaxed select-all ${className}`}>
      {children}
    </pre>
  )
}

export function Field({ label, hint, help, helpWidth, className = "", children }: {
  label: string
  hint?: string
  help?: React.ReactNode
  helpWidth?: string
  className?: string
  children: React.ReactNode
}) {
  const title = <Label className="text-sm font-medium">{label}</Label>
  return (
    <div className={`space-y-2 ${className}`}>
      {help ? <div className="flex items-center gap-1.5">{title}<Help width={helpWidth}>{help}</Help></div> : title}
      {children}
      {/* Kept whole between punctuation, as in Help below. */}
      {hint && <p className="text-xs leading-relaxed text-muted-foreground break-keep wrap-anywhere">{hint}</p>}
    </div>
  )
}

// A switch with its label, in the same two registers as `Field`: what it does
// in one line underneath, and why or at what risk behind the question mark. Not
// a <label>: a tap on the words must not flip the switch, only the switch is
// pressable. aria-labelledby keeps the association for screen readers.
export function SwitchRow({ label, hint, extra, help, helpWidth, checked, onChange }: {
  label: string
  hint?: string
  extra?: React.ReactNode
  help?: React.ReactNode
  helpWidth?: string
  checked: boolean
  onChange: (on: boolean) => void
}) {
  const id = useId()
  return (
    <div className="flex items-start gap-2">
      <Switch aria-labelledby={id} checked={checked} onCheckedChange={onChange} className="mt-0.5" />
      <div className="space-y-1">
        <div className="text-sm leading-snug text-balance">
          <span id={id}>{label}</span>
          {help && (
            <span className="ml-1.5 inline-flex align-middle">
              <Help width={helpWidth}>{help}</Help>
            </span>
          )}
        </div>
        {hint && <p className="text-xs leading-relaxed text-muted-foreground break-keep wrap-anywhere">{hint}</p>}
        {extra}
      </div>
    </div>
  )
}

// A tap shows no tooltip on its own, so a click opens it as well. The trigger's
// own handlers would close it on press and on click; both are prevented.
//
// `width` is fitted to each text: the narrowest at which its paragraphs take the
// fewest lines, plus some room for a wider font. A screen too narrow for that
// gets the narrowest width holding the lines it can fit; capping the wide box
// at the screen instead would leave its lines well short of the right edge.
export function Help({ children, width = "max-w-64" }: { children: React.ReactNode; width?: string }) {
  const [open, setOpen] = useState(false)
  return (
    <Tooltip open={open} onOpenChange={setOpen}>
      <TooltipTrigger asChild>
        <button
          type="button"
          aria-label="说明"
          className="text-muted-foreground hover:text-foreground"
          onPointerDown={(e) => e.preventDefault()}
          onClick={(e) => {
            e.preventDefault()
            setOpen(true)
          }}
        >
          <CircleQuestionMark className="size-3.5" />
        </button>
      </TooltipTrigger>
      {/* text-wrap over the component's text-balance, which breaks multi-line
          Chinese halfway across the box. Chinese may also break between any
          two characters, which splits words such as 季付 across lines; kept
          whole, a line breaks at punctuation and spaces, and mid-run only when
          a run cannot fit at all. */}
      <TooltipContent
        collisionPadding={16}
        className={`${width} space-y-1 text-left text-wrap break-keep wrap-anywhere`}
      >
        {children}
      </TooltipContent>
    </Tooltip>
  )
}

// A titled option with its control at the right. `toggle` makes the whole row a
// label, so a click anywhere flips the Switch it holds; `below` opens beneath it
// in the same card.
export function OptionRow({ title, hint, toggle = false, below, children }: {
  title: React.ReactNode
  hint?: React.ReactNode
  toggle?: boolean
  below?: React.ReactNode
  children: React.ReactNode
}) {
  const Row = toggle ? "label" : "div"
  return (
    <div className="flex flex-col rounded-lg border bg-muted/30 text-sm">
      {/* flex-1: stretched by a grid, the row fills the card and stays clickable. */}
      <Row className={`flex flex-1 items-center justify-between gap-4 px-3 py-2.5 ${toggle ? "cursor-pointer" : ""}`}>
        <span>
          <span className="block font-medium">{title}</span>
          {hint && <span className="mt-0.5 block text-xs text-muted-foreground">{hint}</span>}
        </span>
        {children}
      </Row>
      {below && <div className="border-t px-3 pt-3 pb-3.5">{below}</div>}
    </div>
  )
}

export function ConfirmDialog({ title, description, confirmLabel, busy = false, onClose, onConfirm, children }: {
  title: string
  description: string
  confirmLabel: string
  busy?: boolean
  onClose: () => void
  onConfirm: () => void
  children?: React.ReactNode
}) {
  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription className="leading-relaxed">{description}</DialogDescription>
        </DialogHeader>
        {children}
        <DialogFooter className="border-t pt-4">
          <Button variant="ghost" onClick={onClose}>取消</Button>
          <Button variant="destructive" onClick={onConfirm} disabled={busy}>{confirmLabel}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

export const TEXT_BOX =
  "w-full min-w-0 rounded-md border border-input bg-transparent px-3 py-2 shadow-xs outline-none placeholder:text-muted-foreground focus-visible:border-ring focus-visible:ring-[3px] focus-visible:ring-ring/50 dark:bg-input/30"
export const TEXTAREA = `${TEXT_BOX} font-mono text-xs`
