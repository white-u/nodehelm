import { memo, useState } from "react"
import { Copy, Search } from "lucide-react"

import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { groupsOf, inGroup, shortAddress, type Node, type Source } from "@/lib/api"
import { copy, Picker } from "@/features/shared/controls"

const SOURCES: Record<Source, string> = {
  manual: "手动填写",
  interface: "网卡地址",
  exit: "hub 看到的出口，不在节点网卡上（NAT 或代理）",
  connection: "hub 看到的连接地址",
}

// The address a node is reached by, one per family, each click-to-copy: pasting
// one into an ssh command is why they are shown. Where each came from is in the
// tooltip, keeping the column to addresses alone.
//
// Drawn again only when the addresses change. The table re-renders on every
// push, and redrawing a tooltip per address would raise the panel's script time
// at a hundred nodes from 66 to 128 ms a second.
export const Addresses = memo(
  function Addresses({ list }: { list: NonNullable<Node["addresses"]> }) {
    if (!list.length) return <span className="text-sm text-muted-foreground">—</span>
    return (
      // As wide as the longer address, so both tooltips open from one right
      // edge and the one for a short IPv4 does not cover the IPv6 below it.
      <div className="grid w-fit gap-y-0.5">
        {list.map(({ address, source }) => (
          // Beside the addresses rather than under one, where it would cover
          // the other; and gone once the pointer leaves it.
          <Tooltip key={address} disableHoverableContent>
            <TooltipTrigger asChild>
              <button
                type="button"
                // The toast names what was copied: a tap shows no tooltip, and
                // the cell may show the address shortened.
                onClick={() => copy(address, `已复制 ${address}`)}
                aria-label={`复制 ${address}`}
                className="tnum group inline-flex items-center gap-1 text-sm hover:text-foreground"
              >
                {shortAddress(address)}
                <Copy className="size-3 shrink-0 opacity-0 transition-opacity group-hover:opacity-100" />
              </button>
            </TooltipTrigger>
            <TooltipContent side="right" sideOffset={6} className="max-w-xs">
              <div className="tnum">{address}</div>
              <div className="opacity-70">{SOURCES[source]}，点击复制</div>
            </TooltipContent>
          </Tooltip>
        ))}
      </div>
    )
  },
  // Rows arrive as fresh objects on every push, so the list is compared by value.
  (a, b) => JSON.stringify(a.list) === JSON.stringify(b.list),
)

// Name, address, country and group: what a node is looked up by, in every node list.
export function searchNodes(nodes: Node[], query: string) {
  const needle = query.trim().toLowerCase()
  if (!needle) return nodes
  return nodes.filter((n) =>
    [n.name, n.ip, n.ipv4, n.ipv6, n.ipv4_pin, n.ipv6_pin, n.country, n.group].some((v) => v?.toLowerCase().includes(needle)))
}

// A filter naming a group no node carries any more -- renamed, or its last node
// deleted -- falls back to all rather than showing an empty list; so does 未分组
// once no group is left, since the dropdown that would clear it is hidden then.
// Reset rather than masked, so the old filter does not return with a later group
// of the same name.
export function useGroupFilter(nodes: Node[]) {
  const [filter, setFilter] = useState("all")
  const valid = filter === "all"
    || (filter === "none" ? nodes.some((n) => n.group) : nodes.some((n) => n.group === filter.slice(1)))
  if (!valid) setFilter("all")
  return [valid ? filter : "all", setFilter] as const
}

// Offered once some node has a group. 未分组 is where a batch of freshly
// registered machines waits to be assigned one.
export function GroupFilter({ nodes, value, onChange, className = "" }: {
  nodes: Node[]
  value: string
  onChange: (value: string) => void
  className?: string
}) {
  const groups = groupsOf(nodes)
  if (!groups.length) return null
  return (
    <Select value={value} onValueChange={onChange}>
      <SelectTrigger className={className} aria-label="按分组筛选"><SelectValue /></SelectTrigger>
      <SelectContent position="popper">
        <SelectItem value="all">全部分组</SelectItem>
        {groups.map((g) => <SelectItem key={g} value={`=${g}`}>{g}</SelectItem>)}
        <SelectItem value="none">未分组</SelectItem>
      </SelectContent>
    </Select>
  )
}

export function NodeSearch({ value, onChange, className = "", placeholder = "名称/地址/地区/分组", label = "搜索节点" }: {
  value: string
  onChange: (value: string) => void
  className?: string
  placeholder?: string
  label?: string
}) {
  return (
    <div className={`relative ${className}`}>
      <Search className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
      <Input
        className="pl-8"
        placeholder={placeholder}
        aria-label={label}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        // Inside a dialog's form, Enter would otherwise save the dialog.
        onKeyDown={(e) => e.key === "Enter" && e.preventDefault()}
      />
    </div>
  )
}

// Offline nodes are dimmed but remain selectable.
export function NodePicker({ nodes, chosen, onPick, disabled = false }: {
  nodes: Node[]
  chosen: Set<number>
  onPick: (list: Node[], on: boolean) => void
  disabled?: boolean
}) {
  const [query, setQuery] = useState("")
  const [group, setGroup] = useGroupFilter(nodes)
  return (
    <Picker
      visible={inGroup(searchNodes(nodes, query), group)}
      chosen={chosen}
      onPick={onPick}
      disabled={disabled}
      toolbar={<>
        <NodeSearch className="min-w-0 flex-1 basis-40" value={query} onChange={setQuery} />
        <GroupFilter nodes={nodes} value={group} onChange={setGroup} className="w-32" />
      </>}
      empty={nodes.length ? "没有匹配的节点" : "先添加节点"}
      // Three columns keep a few dozen nodes within one scroll. A phone gets
      // one: two cut a name to a few characters, and a tap shows no title.
      listClass="grid-cols-1 min-[480px]:grid-cols-2 sm:grid-cols-3"
      title={(n) => (n.group ? `${n.name} · ${n.group}` : n.name)}
    >
      {(n) => <>
        <span className={`truncate ${n.online ? "" : "text-muted-foreground"}`}>{n.name}</span>
        {n.country && <span className="ml-auto shrink-0 text-xs text-muted-foreground">{n.country}</span>}
      </>}
    </Picker>
  )
}

// Width in ems, near enough: a CJK character is one, anything else about half.
const ems = (word: string) => [...word].reduce((n, c) => n + (c > "\u2e7f" ? 1 : 0.55), 0)

// A node name, broken at its spaces and after the dots of a hostname --
// registered nodes are named after theirs. A word up to eight ems stays whole,
// a city, a label or a hyphenated one such as GIA-E, where the browser would
// leave its last letter to open the next line. A wider word, as a name typed
// without spaces usually is, breaks between its CJK characters, and a run of
// letters only where nothing else fits: kept whole, it would set the column's
// minimum width and push the table past the screen. A separator such as the ·
// in 香港 09 · DMIT, or a dash, is held to the word before it, so no line opens
// with one.
export function nameText(name: string) {
  return name
    .replace(/ (?=[·|/｜—–-] )/g, "\u00a0")
    .split(/( )/)
    .map((word, i) => {
      const parts = word.split(".").flatMap((part, j) => (j ? [".", <wbr key={j} />, part] : [part]))
      if (ems(word) > 8) return <span key={i} className="wrap-anywhere [word-break:normal]">{parts}</span>
      return word.includes("-") ? <span key={i} className="whitespace-nowrap">{word}</span> : parts
    })
}
