import { useEffect, useState } from "react"

import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { api, type Node, type TrafficBytes, type TrafficPeriods } from "@/lib/api"
import { bytes } from "@/lib/format"

const PERIODS = ["00–08", "08–14", "14–20", "20–24"]
const MODES = [
  ["rx", "下载"],
  ["tx", "上传"],
  ["total", "双向合计"],
] as const

type Mode = (typeof MODES)[number][0]

function valueOf(value: TrafficBytes | null, mode: Mode): number | null {
  return value?.[mode] ?? null
}

function shown(value: TrafficBytes | null, mode: Mode): string {
  const count = valueOf(value, mode)
  return count === null ? "未记录" : bytes(count)
}

function utcOffset(seconds: number): string {
  const sign = seconds < 0 ? "−" : "+"
  const minutes = Math.abs(seconds) / 60
  const hours = Math.floor(minutes / 60)
  const rest = minutes % 60
  return `UTC${sign}${String(hours).padStart(2, "0")}:${String(rest).padStart(2, "0")}`
}

export function TrafficHistory({ node, onClose }: { node: Node; onClose: () => void }) {
  const [days, setDays] = useState("7")
  const [mode, setMode] = useState<Mode>("total")
  const [data, setData] = useState<TrafficPeriods | null>(null)
  const [error, setError] = useState("")
  const [loading, setLoading] = useState(true)
  const [retry, setRetry] = useState(0)

  useEffect(() => {
    let current = true
    let controller: AbortController | null = null
    let timer: ReturnType<typeof setTimeout> | undefined

    const load = async () => {
      if (!current || document.visibilityState !== "visible" || controller) return
      const request = new AbortController()
      controller = request
      setLoading(true)
      setError("")
      try {
        const queryDays = days === "all" ? 365 : days
        const result = await api<TrafficPeriods>(`/nodes/${node.id}/traffic/periods?days=${queryDays}`, { signal: request.signal })
        if (current && !request.signal.aborted) setData(result)
      } catch (e) {
        if (current && !request.signal.aborted) setError((e as Error).message)
      } finally {
        if (controller === request) controller = null
        if (current) {
          if (request.signal.aborted) {
            if (document.visibilityState === "visible") void load()
          } else {
            setLoading(false)
            timer = setTimeout(load, 60_000)
          }
        }
      }
    }

    const visibility = () => {
      if (document.visibilityState === "visible") void load()
      else {
        clearTimeout(timer)
        timer = undefined
        controller?.abort()
        setLoading(false)
      }
    }

    void load()
    document.addEventListener("visibilitychange", visibility)
    return () => {
      current = false
      clearTimeout(timer)
      document.removeEventListener("visibilitychange", visibility)
      controller?.abort()
    }
  }, [node.id, days, retry])

  const selectedLabel = MODES.find(([key]) => key === mode)?.[1]

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent onOpenAutoFocus={(e) => e.preventDefault()} className="w-[calc(100%-2rem)] max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-6xl">
        <DialogHeader>
          <DialogTitle>{node.name} · 流量历史</DialogTitle>
          <DialogDescription>按 hub 采样时间记账，跨时段采样及断报补记归后一次采样所在时段，无法精确还原实际时间分布。计费模式不影响双向合计。</DialogDescription>
        </DialogHeader>

        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="flex items-center gap-2">
            <span className="text-sm text-muted-foreground">范围</span>
            <Select value={days} onValueChange={(value) => { setData(null); setError(""); setLoading(true); setDays(value) }}>
              <SelectTrigger className="w-28" aria-label="历史范围"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="7">最近 7 天</SelectItem>
                <SelectItem value="30">最近 30 天</SelectItem>
                <SelectItem value="all">全部</SelectItem>
              </SelectContent>
            </Select>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-sm text-muted-foreground">显示</span>
            <Select value={mode} onValueChange={(value) => setMode(value as Mode)}>
              <SelectTrigger className="w-32" aria-label="流量类型"><SelectValue /></SelectTrigger>
              <SelectContent>
                {MODES.map(([value, label]) => <SelectItem key={value} value={value}>{label}</SelectItem>)}
              </SelectContent>
            </Select>
          </div>
        </div>

        <div className="text-xs text-muted-foreground">
          {data ? `hub 时区 ${utcOffset(data.utc_offset_seconds)} · 实际范围 ${data.since}–${data.until} · 保留 ${data.retention_days} 天 · 已记录合计（${selectedLabel}）：${shown(data.summary, mode)}` : "按 hub 时钟统计"}
          {loading && <span> · 更新中</span>}
        </div>
        {error && (
          <div role="alert" className="flex flex-wrap items-center gap-2 text-sm text-destructive">
            <span>{error}</span>
            <Button variant="outline" size="sm" className="h-7" onClick={() => { setData(null); setLoading(true); setRetry((value) => value + 1) }}>重试</Button>
          </div>
        )}

        <div className="overflow-x-auto rounded-md border">
          <Table className="min-w-[760px]">
            <TableHeader>
              <TableRow>
                <TableHead>日期</TableHead>
                {PERIODS.map((period) => <TableHead key={period} className="text-right">{period}</TableHead>)}
                <TableHead className="text-right">已记录合计</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {data?.rows.map((row) => (
                <TableRow key={row.day}>
                  <TableCell className="whitespace-nowrap">{row.day}</TableCell>
                  {row.periods.map((period, index) => {
                    const count = valueOf(period, mode)
                    const isToday = row.day === data.today
                    const isCurrent = isToday && index === data.current_slot
                    const hasValue = count !== null
                    const label = hasValue
                      ? `${bytes(count)}${isCurrent ? " · 统计中" : ""}`
                      : isCurrent ? "统计中" : isToday && index > data.current_slot ? "尚未开始" : "未记录"
                    return <TableCell key={PERIODS[index]} className="tnum text-right whitespace-nowrap">{label}</TableCell>
                  })}
                  <TableCell className="tnum text-right whitespace-nowrap">{shown(row.total, mode)}</TableCell>
                </TableRow>
              ))}
              {!loading && data?.rows.length === 0 && (
                <TableRow><TableCell colSpan={6} className="py-8 text-center text-sm text-muted-foreground">暂无流量记录</TableCell></TableRow>
              )}
              {!data && loading && (
                <TableRow><TableCell colSpan={6} className="py-8 text-center text-sm text-muted-foreground">正在加载流量记录</TableCell></TableRow>
              )}
            </TableBody>
          </Table>
        </div>
      </DialogContent>
    </Dialog>
  )
}
