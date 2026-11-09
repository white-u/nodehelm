import { useCallback, useEffect, useRef, useState } from "react"
import { ChevronRight, Copy, Download, Trash2, Upload } from "lucide-react"
import { toast } from "sonner"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Switch } from "@/components/ui/switch"
import { api, behind, outdatedAgents, upload, type Node } from "@/lib/api"
import { bytes } from "@/lib/format"
import { copy, Command, Field, OptionRow, ConfirmDialog } from "@/features/shared/controls"
import { upgradeCommand } from "@/features/nodes/AgentInstall"
import { useSettings } from "@/features/settings/GeneralSettings"

type Versions = { hub: string; hub_latest: string; agent_latest: string; notice: boolean }

/**
 * What is running and what is published. Read once per panel load: the hub holds
 * the answer for six hours, so this costs a GitHub lookup a few times a day at
 * most, and nothing at all while nobody opens the panel.
 *
 * A hub that cannot reach github.com answers with empty latest fields, which
 * render as no update rather than as an error.
 */
export function useVersions() {
  const [versions, setVersions] = useState<Versions | null>(null)
  const load = useCallback(() => api<Versions>("/version").then(setVersions).catch(() => {}), [])
  useEffect(() => { load() }, [load])
  return { versions, reload: load }
}

type Session = { id: string; current: boolean; created_at: number }

function useSessions() {
  const [rows, setRows] = useState<Session[] | null>(null)
  // A failed load leaves the list empty rather than absent: the security page
  // waits for it, and the password card must stay reachable.
  const load = useCallback(
    () =>
      api<Session[]>("/sessions")
        .then(setRows)
        .catch((e: Error) => {
          toast.error(e.message)
          setRows((rows) => rows ?? [])
        }),
    [],
  )
  useEffect(() => { load() }, [load])
  return { rows, load }
}

function Sessions({ rows, reload }: { rows: Session[]; reload: () => void }) {
  const [busy, setBusy] = useState("")

  async function remove(id: string) {
    setBusy(id)
    try {
      await api(`/sessions/${id}`, { method: "DELETE" })
      toast.success("已删除会话")
      reload()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setBusy("")
    }
  }

  return (
    <Card className="gap-4 p-5">
      <div>
        <h3 className="text-sm font-medium">登录会话</h3>
        <p className="mt-1 text-xs text-muted-foreground">
          每次登录一条，14 天后过期。删除后该设备下一次请求就被登出。
        </p>
      </div>
      <div className="divide-y">
        {rows.map((s) => (
          <div key={s.id} className="flex items-center justify-between gap-3 py-2.5 first:pt-0 last:pb-0">
            <div className="flex min-w-0 items-center gap-2 text-sm">
              <span className="tnum">{new Date(s.created_at * 1000).toLocaleString("zh-CN")}</span>
              {s.current && <Badge variant="secondary">当前设备</Badge>}
            </div>
            {/* 当前会话没有删除按钮：右上角的退出登录做的就是这件事，而在这里删
                只会让已经渲染好的面板以为自己还登着。 */}
            {!s.current && (
              <Button size="icon" variant="ghost" title="删除会话" aria-label="删除会话" disabled={!!busy} onClick={() => remove(s.id)}>
                <Trash2 />
              </Button>
            )}
          </div>
        ))}
      </div>
    </Card>
  )
}

// The ways into this panel, on their own page: the sessions signed in, the
// GitHub identity it trusts and the password that works when GitHub does not.
export function Security({ site }: { site: string }) {
  const { s, set, save } = useSettings()
  const sessions = useSessions()
  const [password, setPassword] = useState("")
  // Drawn once both have arrived, so the list does not land late above the
  // cards and push them down.
  if (!s || !sessions.rows) return null
  const callback = `${site}/api/auth/github/callback`

  return (
    <div className="space-y-4">
      <Sessions rows={sessions.rows} reload={sessions.load} />

      <Card className="gap-4 p-5">
        <div>
          <h3 className="text-sm font-medium">GitHub 单点登录</h3>
          <p className="mt-1 text-xs text-muted-foreground">
            OAuth App 回调地址 <code className="rounded bg-muted px-1 break-all">{callback}</code>
          </p>
        </div>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Client ID">
            <Input value={String(s.github_client_id ?? "")} onChange={(e) => set("github_client_id", e.target.value)} />
          </Field>
          <Field label="Client Secret" hint={s.github_secret_set ? "已设置，留空不变" : "未设置"}>
            <Input
              type="password"
              autoComplete="off"
              placeholder={s.github_secret_set ? "••••••••" : ""}
              value={String(s.github_client_secret ?? "")}
              onChange={(e) => set("github_client_secret", e.target.value)}
            />
          </Field>
        </div>
        {String(s.github_client_id ?? "") !== "" && String(s.github_allowed_users ?? "").trim() === "" && (
          <p className="rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive">
            白名单为空，GitHub 登录拒绝所有人。填入用户名并保存后生效。
          </p>
        )}
        <Field label="允许登录的 GitHub 用户名" hint="逗号分隔。留空拒绝所有人。移除账号会注销其它设备的旧会话，当前管理员会话保留">
          <Input value={String(s.github_allowed_users ?? "")} onChange={(e) => set("github_allowed_users", e.target.value)} placeholder="GitHub 用户名" />
        </Field>
        <div>
          <Button
            size="sm"
            onClick={() => {
              const patch: Record<string, string> = {
                github_client_id: String(s.github_client_id ?? ""),
                github_allowed_users: String(s.github_allowed_users ?? ""),
              }
              if (typeof s.github_client_secret === "string" && s.github_client_secret) {
                patch.github_client_secret = s.github_client_secret
              }
              save(patch)
            }}
          >
            保存 GitHub 设置
          </Button>
        </div>
      </Card>

      <Card className="gap-4 p-5">
        <div>
          <h3 className="text-sm font-medium">应急密码</h3>
          <p className="mt-1 text-xs text-muted-foreground">
            GitHub 不可用时的备用入口。修改后其它设备登录立即失效，当前设备不受影响。
          </p>
        </div>
        <Field label="新密码" hint="至少 12 位">
          <Input type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="new-password" />
        </Field>
        <div>
          <Button
            size="sm"
            disabled={password.length < 12}
            // Every other session ends with the change, so the list is read again.
            onClick={() => save({ admin_password: password }, "密码已修改").then((ok) => { if (ok) { setPassword(""); sessions.load() } })}
          >
            修改密码
          </Button>
        </div>
      </Card>
    </div>
  )
}

type DbInfo = {
  path: string
  size: number
  wal: number
  free: number
  /** Seconds since the earliest history row on the hub's clock, null on a database with none. */
  oldest_ago: number | null
  retention: number
  rows: Record<string, number>
}

// The only tables whose row count indicates anything about size, each kind of
// history in both tiers. Every other holds one row per node or per key.
const DB_ROWS: [string, string][] = [
  ["metric", "历史明细"],
  ["metric_hour", "历史小时汇总"],
  ["fork_traffic_period", "分时流量记录"],
  ["ping_record", "延迟记录"],
  ["ping_hour", "延迟小时汇总"],
]

export function Data() {
  const [info, setInfo] = useState<DbInfo | null>(null)
  const [busy, setBusy] = useState("")
  const [confirm, setConfirm] = useState<"vacuum" | null>(null)
  const [pending, setPending] = useState<File | null>(null)
  const [sent, setSent] = useState(0)
  // Closing the dialog must stop the upload rather than merely hide it: restore
  // is the one irreversible action here, and it takes minutes on a large
  // backup.
  const abort = useRef<AbortController | null>(null)
  const picker = useRef<HTMLInputElement>(null)

  const load = () => api<DbInfo>("/db").then(setInfo).catch((e: Error) => toast.error(e.message))
  useEffect(() => { load() }, [])

  async function vacuum() {
    setBusy("vacuum")
    try {
      const { pruned, freed } = await api<{ pruned: number; freed: number }>("/db/vacuum", { method: "POST" })
      toast.success(`已清理 ${pruned} 行，回收 ${bytes(freed)}`)
      load()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setBusy("")
      setConfirm(null)
    }
  }

  async function restore(file: File) {
    setBusy("restore")
    setSent(0)
    abort.current = new AbortController()
    try {
      await upload("/db/restore", file, setSent, abort.current.signal)
      toast.success("已恢复，正在重新加载")
      // Every node, setting and session on the page came from the database just
      // replaced.
      setTimeout(() => location.reload(), 800)
    } catch (e) {
      // Aborting partway is not a failure: the hub replaces nothing until the
      // last chunk, so the original database remains.
      const aborted = (e as Error).name === "AbortError"
      if (aborted) toast.info("已取消，数据库没有改动")
      else toast.error((e as Error).message)
      setBusy("")
    }
    setPending(null)
  }

  if (!info) return null
  const stat = (label: string, value: string, className = "") => (
    <div key={label} className={className}>
      <div className="text-xs text-muted-foreground">{label}</div>
      <div className="tnum mt-0.5 text-sm">{value}</div>
    </div>
  )

  return (
    <div className="space-y-4">
      <Card className="gap-4 p-5">
        <h3 className="text-sm font-medium">数据库</h3>
        {/* Five columns: the file and the window on one row, the four row counts
            on the next. On two columns the free space takes a row of its own, so
            the window and each kind's two tiers still pair up. */}
        <div className="grid grid-cols-2 gap-4 sm:grid-cols-5">
          {stat("文件大小", bytes(info.size))}
          {stat("预写日志", bytes(info.wal))}
          {stat("可回收空间", bytes(info.free), "col-span-2 sm:col-span-1")}
          {stat("保留天数", `${info.retention} 天`)}
          {/* 和保留天数并排：跨度小于保留期是还没攒够，大于保留期就是每小时
              那次 prune 没在跑。 */}
          {stat("历史跨度", info.oldest_ago !== null ? `${Math.floor(info.oldest_ago / 86400)} 天` : "—")}
          {DB_ROWS.map(([key, label]) => stat(label, (info.rows[key] ?? 0).toLocaleString()))}
        </div>
        <p className="truncate text-xs text-muted-foreground" title={info.path}>
          <code>{info.path}</code>
        </p>
      </Card>

      <Card className="gap-4 p-5">
        <div>
          <h3 className="text-sm font-medium">回收空间</h3>
          <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
            清掉超出保留天数的历史，再重建数据库文件把空出来的页还给磁盘（SQLite 的 VACUUM）。重建期间需要约为数据库两倍的空闲磁盘，过程中面板和上报会短暂变慢。
          </p>
        </div>
        <div>
          <Button size="sm" variant="secondary" disabled={!!busy} onClick={() => setConfirm("vacuum")}>
            {busy === "vacuum" ? "回收中…" : "立即回收"}
          </Button>
        </div>
      </Card>

      <Card className="gap-4 p-5">
        <h3 className="text-sm font-medium">备份</h3>
        <div className="flex flex-wrap gap-2">
          {/* The browser's own download: the file is streamed straight from
              the response, never held in the page. */}
          <Button size="sm" asChild>
            <a href="/api/db/backup" download>
              <Download /> 导出备份
            </a>
          </Button>
          <Button size="sm" variant="secondary" disabled={!!busy} onClick={() => picker.current?.click()}>
            <Upload /> 导入备份
          </Button>
          <input
            ref={picker}
            type="file"
            accept=".db,application/octet-stream"
            className="hidden"
            onChange={(e) => {
              setPending(e.target.files?.[0] ?? null)
              e.target.value = ""
            }}
          />
        </div>
      </Card>

      {confirm === "vacuum" && (
        <ConfirmDialog
          title="回收空间？"
          description="超出保留天数的历史会被删除，然后重建数据库文件。累计流量不受影响。"
          confirmLabel={busy === "vacuum" ? "回收中…" : "开始回收"}
          busy={!!busy}
          onClose={() => setConfirm(null)}
          onConfirm={vacuum}
        />
      )}
      {pending && (
        <ConfirmDialog
          title="用备份覆盖当前数据？"
          description={`将用 ${pending.name}（${bytes(pending.size)}）整体替换当前数据库，无法撤销。`}
          confirmLabel={busy === "restore" ? `已上传 ${bytes(sent)} / ${bytes(pending.size)}` : "确认恢复"}
          busy={!!busy}
          onClose={() => { abort.current?.abort(); setPending(null) }}
          onConfirm={() => restore(pending)}
        />
      )}
    </div>
  )
}

// Hub and agent are published from different accounts; the hub entry matches HUB_REPO in main.rs.
const hubUpdate = 'curl -fsSL https://github.com/white-u/nodehelm/releases/latest/download/nodehelm.sh | sudo sh'
const updateDocs = "https://github.com/white-u/nodehelm/blob/main/DEPLOY.md#6-更新与回退"
const componentUpgrade = "sudo python3 /opt/node-manager/install.py upgrade"
const releaseUrl = (repo: string, version: string) => `https://github.com/${repo}/releases/tag/v${version}`

/** `v1.2.0 → v1.3.0` when something is published, the running version alone otherwise. */
function VersionPair({ current, latest }: { current: string; latest: string }) {
  if (!behind(current, latest)) {
    return <span className="text-xs text-muted-foreground">{latest ? `已是最新 v${current}` : `当前 v${current}`}</span>
  }
  return (
    <span className="text-xs text-muted-foreground">
      v{current} <span className="px-0.5">→</span>
      <span className="ml-0.5 font-medium text-foreground">v{latest}</span>
    </span>
  )
}

/** Outdated nodes grouped by the version they run, oldest first. */
function byVersion(nodes: Node[]): [string, Node[]][] {
  const groups = new Map<string, Node[]>()
  for (const n of nodes) groups.set(n.agent_version, [...(groups.get(n.agent_version) ?? []), n])
  return [...groups].sort(([a], [b]) => a.localeCompare(b, undefined, { numeric: true }))
}

/**
 * What is published for the hub and the agents. Its own route rather than a
 * banner on the node list; the dot in the navigation is what says there is
 * something here.
 *
 * The hub card gives the installer's command and names the container's way too:
 * how to take a release depends on how the hub was installed, which it cannot tell.
 */
export function Update({ versions, reload, nodes, site, refusal }: {
  versions: Versions | null
  reload: () => void
  nodes: Node[]
  site: string
  refusal: string
}) {
  const [saving, setSaving] = useState(false)
  if (!versions) return null
  const outdated = outdatedAgents(nodes, versions.agent_latest)
  const offline = outdated.filter((n) => !n.online).length
  const upgrade = refusal ? "" : upgradeCommand(site)
  const unreachable = !versions.hub_latest

  // Applied on the spot: one switch, and the navigation changes with it.
  async function setNotice(on: boolean) {
    setSaving(true)
    try {
      await api("/settings", { method: "PUT", body: JSON.stringify({ update_notice: on ? "on" : "off" }) })
      reload()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setSaving(false)
    }
  }

  return (
    <div className="space-y-4">
      {unreachable && (
        <Card className="p-5">
          <p className="text-sm text-muted-foreground">
            查不到后台最新版本：这台 hub 连不上 api.github.com。GitHub 代理不作用于这一项。
          </p>
        </Card>
      )}

      <Card className="gap-4 p-5">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h3 className="text-sm font-medium">后台</h3>
          <div className="flex items-center gap-3">
            <VersionPair current={versions.hub} latest={versions.hub_latest} />
            {behind(versions.hub, versions.hub_latest) && (
              <Button size="sm" variant="ghost" asChild>
                <a href={releaseUrl("white-u/nodehelm", versions.hub_latest)} target="_blank" rel="noreferrer">
                  发布说明
                </a>
              </Button>
            )}
          </div>
        </div>
        {behind(versions.hub, versions.hub_latest) && (
          <>
            <p className="text-xs leading-relaxed text-muted-foreground">
              在后台服务器上执行，失败会自动回退。更新后再按下方「管理组件」升级各节点。
            </p>
            <Command>{hubUpdate}</Command>
            <div>
              <Button size="sm" variant="secondary" onClick={() => copy(hubUpdate)}>
                <Copy className="size-4" /> 复制命令
              </Button>
            </div>
          </>
        )}
      </Card>

      <Card className="gap-4 p-5">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h3 className="text-sm font-medium">agent</h3>
          <span className="text-xs text-muted-foreground">
            {versions.agent_latest
              ? outdated.length
                ? <>本后台提供 v{versions.agent_latest} · <span className="font-medium text-foreground">{outdated.length} 台待升级</span></>
                : `全部已是本后台提供的 v${versions.agent_latest}`
              : "此后台构建不含 agent"}
          </span>
        </div>
        {/* The agent arrives with the hub: a development build serves none, so
            its command would only fail. */}
        {outdated.length > 0 && versions.agent_latest && (
          <>
            <p className="text-xs leading-relaxed text-muted-foreground">
              agent 随后台发布，从本后台下载。以 root 在每台机器上执行一次，不含凭证，沿用机器上已有的设置，不会新建节点；已接入服务管理的节点也可运行 sudo nodehelm upgrade。
            </p>
            {upgrade ? (
              <>
                <Command>{upgrade}</Command>
                <div className="flex flex-wrap gap-2">
                  <Button size="sm" variant="secondary" onClick={() => copy(upgrade)}>
                    <Copy className="size-4" /> 复制命令
                  </Button>
                  <Button size="sm" variant="ghost" asChild>
                    <a href={updateDocs} target="_blank" rel="noreferrer">
                      更新说明
                    </a>
                  </Button>
                </div>
              </>
            ) : (
              <p className="text-xs text-muted-foreground">{refusal}</p>
            )}
            {/* Collapsed until asked for, grouped by version, and bounded in height
                once open, so any number of nodes stays one line on the page. */}
            {outdated.length > 0 && (
              <details className="group border-t pt-3">
                <summary className="flex cursor-pointer list-none items-center justify-between gap-3 rounded-md text-xs text-muted-foreground outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50 [&::-webkit-details-marker]:hidden">
                  <span className="flex items-center gap-1.5">
                    <ChevronRight className="size-4 transition-transform group-open:rotate-90" />
                    待升级的节点
                  </span>
                  {offline > 0 && <span>其中 {offline} 台离线</span>}
                </summary>
                <div className="mt-3 max-h-60 space-y-3 overflow-auto">
                  {byVersion(outdated).map(([version, group]) => (
                    <div key={version} className="space-y-1.5">
                      <div className="text-xs text-muted-foreground">v{version} · {group.length} 台</div>
                      <div className="flex flex-wrap gap-1.5">
                        {group.map((n) => (
                          <Badge
                            key={n.id}
                            variant="secondary"
                            className={`font-normal ${n.online ? "" : "opacity-50"}`}
                            title={n.online ? undefined : "离线"}
                          >
                            {n.name}
                          </Badge>
                        ))}
                      </div>
                    </div>
                  ))}
                </div>
              </details>
            )}
          </>
        )}
      </Card>

      <Components nodes={nodes} />

      <OptionRow
        title="更新提醒"
        hint="有新版本时在导航的「更新」旁显示小圆点。关闭只是不再显示圆点，这一页照常检查"
        toggle
      >
        <Switch checked={versions.notice} disabled={saving} onCheckedChange={setNotice} />
      </OptionRow>
    </div>
  )
}

/**
 * Nodes whose management component is not the one this hub ships. Upgrading
 * stays a command run on the node: the hub names who needs it and never pushes code.
 */
function Components({ nodes }: { nodes: Node[] }) {
  const [state, setState] = useState<{ enabled: boolean; bound: number; outdated: number[]; offline: number[] } | null>(null)
  useEffect(() => {
    api<{ enabled: boolean; bound: number; outdated: number[]; offline: number[] }>("/management").then(setState, () => setState(null))
  }, [])
  if (!state?.enabled || !state.bound) return null
  const names = new Map(nodes.map((n) => [n.id, n]))
  const outdated = state.outdated.map((id) => names.get(id)).filter((n): n is Node => !!n)
  return (
    <Card className="gap-4 p-5">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h3 className="text-sm font-medium">管理组件</h3>
        <span className="text-xs text-muted-foreground">
          {outdated.length
            ? <><span className="font-medium text-foreground">{outdated.length} 台待升级</span> · 共 {state.bound} 台接入</>
            : `全部 ${state.bound} 台与后台一致`}
        </span>
      </div>
      {outdated.length > 0 && (
        <>
          <p className="text-xs leading-relaxed text-muted-foreground">
            以 root 在下列节点上执行，从本后台下载并校验组件，失败自动回退；业务实例只在配置变化时重启。已有 nodehelm 命令的节点也可运行 sudo nodehelm upgrade，同时升级监控 Agent。
          </p>
          <Command>{componentUpgrade}</Command>
          <div>
            <Button size="sm" variant="secondary" onClick={() => copy(componentUpgrade)}>
              <Copy className="size-4" /> 复制命令
            </Button>
          </div>
          <div className="flex max-h-60 flex-wrap gap-1.5 overflow-auto border-t pt-3">
            {outdated.map((n) => (
              <Badge key={n.id} variant="secondary" className={`font-normal ${state.offline.includes(n.id) ? "opacity-50" : ""}`} title={state.offline.includes(n.id) ? "管理组件未连接" : undefined}>
                {n.name}
              </Badge>
            ))}
          </div>
        </>
      )}
    </Card>
  )
}
