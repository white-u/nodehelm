# NodeHelm 部署与维护速查

[文档索引](INDEX.md) · [开发构建](DEVELOPMENT.md)

本页用于当前独立 NodeHelm 项目。完整安装、四模块管理、升级卸载及中断恢复流程以 [部署与恢复说明](NODE_MANAGEMENT_DEPLOY.md) 为准。原 monitor 服务有独立安装器，不使用本项目命令修改它。

## 安装入口与服务位置

使用本项目离线包，按 [README](README.md#安装-node-manager) 核对摘要并执行首次安装。不要运行原 monitor 上游 Release 安装器覆盖此项目。

| 项目 | 安装器默认值 |
| --- | --- |
| hub 二进制 | `/opt/node-manager-hub/monitor-hub` |
| 数据库 | `/var/lib/node-manager-hub/hub.db` |
| 磁盘主题 | `/var/lib/node-manager-hub/themes/` |
| systemd 单元 | `/etc/systemd/system/node-manager-hub.service` |
| 监听 | `127.0.0.1:9911` |
| 后台入口 | `https://你的域名/admin/` |

默认主题随后台二进制更新。磁盘主题目录中 `short` 为 `default` 的主题会顶替内置版本；早期版本在面板中更新过默认主题会留下 `themes/default`，删除后恢复使用内置版本。

程序直接启动的默认监听是 `[::]:28080`，无双栈能力时回退 IPv4；与安装器明确生成的回环监听不同。不要用程序默认值推断安装后端口的暴露范围。

直接运行二进制时可用以下参数；其他业务设置在后台配置并存入 SQLite：

| 参数 | 默认值与作用 |
| --- | --- |
| `--listen` | `[::]:28080`，显式指定后按该地址监听 |
| `--db` | 当前目录的 `monitor.db` |
| `--themes` | 数据库旁的 `themes/` 目录 |
| `--site` | 默认留空；设置对外 HTTPS 域名入口 |
| `--reset-password` | 对已有数据库重置应急密码，登出会话后退出 |

## 代理与接入

反向代理负责 HTTPS，将站点流量送到 hub，并支持 agent `/api/agent/ws` 和页面 `/api/ws` 的 WebSocket 升级。agent 所在机器必须能访问同一个有效 HTTPS 域名。代理应正确提供请求协议和来源信息；相关信任逻辑见 `auth.rs`。

`--site https://你的域名` 可明确 agent 接入地址和 cookie 的安全标志；域名必须是 HTTPS 域名入口。正常从 HTTPS 域名打开后台时可用浏览器地址生成安装命令，但通过 SSH 隧道访问、节点入口与后台域名不同、或代理不能正确表达 HTTPS 时应显式指定。添加/安装节点有额外入口保护，仅能打开页面不代表可添加节点。

首次启动且数据库尚无密码时，后台把应急密码写入本机服务日志；独立安装器随后立即重置，新密码只显示在安装终端，日志中的那一个已失效。安装器不将密码存入发布包。遗失密码可使用下文的重置入口。登录后在后台添加节点并复制 agent 安装命令。hub 的 `/install.sh` 提供的是 [agent 安装器](install.sh)，它安装 hub 内嵌的 `agent/` 监控 agent（从 hub 的 `/agent/{arch}` 下载），不是 hub 自身。

agent 默认路径是 `/opt/monitor/monitor-agent`、`/opt/monitor/agent.env`，服务名 `monitor-agent`，支持 systemd 或 OpenRC。hub 与 agent 可在同机共存，但服务、凭据和卸载入口不同。

## 更新与日志

```sh
systemctl status node-manager-hub --no-pager
journalctl -u node-manager-hub -n 100 --no-pager
```

安装与更新是同一条命令。已安装时它下载并校验最新发布包，相同版本直接退出，否则先做更新前备份并检查新程序；失败会回退，完成后删除这份备份：

```sh
curl -fsSL https://github.com/white-u/nodehelm/releases/latest/download/nodehelm.sh | sudo sh
```

连不上 GitHub 时，把发布包和 `nodehelm.sh` 传到服务器，运行 `sudo sh nodehelm.sh --package 发布包`；也可解压后直接运行包内的 `update-hub.sh`。校验文件与程序来自同一发布，只能发现下载损坏；要防发布被篡改，需另行核对来源。

切换中断使用同一工具 `recover`，不能删除恢复记录绕过检查。管理组件用 `/opt/node-manager/install.py upgrade` 单独维护；完整参数见部署说明。

hub 与 agent 分别更新。后台显示 monitor 基础版本与上游 Agent 版本；NodeHelm 更新使用本项目独立构建，不以原 monitor 版本提示替换本项目。自建镜像时 [Dockerfile](Dockerfile) 使用预编译的两个架构二进制，不会从 Rust 源码构建；镜像使用 UID/GID `65534`，数据挂载必须可写，默认数据库路径为 `/data/monitor.db`。

## 通知

在后台「通知」页配置 Telegram 或 Webhook，配了哪个发哪个。Webhook 的 `{{event}}` 为下表事件名。

| 事件 | 事件名 | 何时提醒 | 范围 |
| --- | --- | --- | --- |
| 离线／恢复在线 | `offline`／`online` | 断开超过离线宽限期（默认 3 分钟） | 「离线通知」中勾选的节点 |
| 延迟异常／恢复 | `latency`／`latency_recovered` | 延迟探测持续明显高于平时或丢包过半，见下文 | 「延迟通知」中勾选的节点 |
| 流量 | `traffic` | 本期用量达到设定比例及 100% | 填了流量额度的节点 |
| 到期／续期 | `expiry`／`renew` | 每天 9 点汇总即将到期的节点；自动续期时 | 填了到期日的节点 |
| 登录 | `login` | 登录后台时 | 开关控制 |
| 服务管理 | `management`；PM 异常接入为 `ip_alert` | 操作失败、配置漂移、PM 阈值、到期与异常接入、管理组件离线 | 开关控制 |

延迟通知按「节点 × 探测」判断，对比该探测自身的平时水平：近 7 天小时中位数的中位数，与同一时段（±1 小时）的中位数取较高者，所以固定的晚高峰不算异常。至少有 12 小时记录才开始判断，探测改了目标后重新累计。

- 提醒：最近 5 分钟（探测间隔长时取 5 个间隔）内 80% 以上的应答高于 max(平时×2, 平时+50 ms)，或丢包达到一半。
- 恢复：中位数回到 max(平时×1.5, 平时+25 ms) 以下且丢包低于 20%。异常期间平时水平保持不变，一小时内再次异常时判断窗口延长到三倍。
- 不判断间隔超过 5 分钟的探测；节点离线时由离线通知负责。
- 同一探测半数以上（至少 3 台）节点同时异常时合并为一行，提示可能是目标侧的问题。

探测是 TCP 握手，超过 900 ms 记为丢包，所以严重的高延迟会表现为丢包。

## 备份、恢复与时间

优先使用后台的数据备份功能，它走 SQLite 备份接口。运行中只复制 `monitor.db` 可能遗漏 WAL 中已提交的数据；手工文件备份应先正常停止 hub，再保存数据目录，并核对所有权和文件完整性。

完整迁移还应保存磁盘主题目录及 systemd 中的监听/站点设置；数据库备份不会打包外部主题文件。数据库包含设置、节点 token、会话等敏感数据，应限制访问。恢复操作会替换业务数据，先留存现状备份，再使用后台恢复入口；源码会先迁移并验证候选数据库，包含 fork 扩展表兼容性检查。

日、账期与分时记录使用 hub 本地时间。容器默认 `TZ=UTC`，镜像包含 zoneinfo；要按其他时区记账，应在部署时显式设置，例如 `TZ=Asia/Singapore`。修改时区影响后续归类，不会重算旧分时记录。更改系统时间或时区也会影响之后的归类。

## 密码与卸载

遗失应急密码时，在后台主机执行，确认数据库路径确实属于本项目：

```sh
sudo /opt/node-manager-hub/monitor-hub --db /var/lib/node-manager-hub/hub.db --reset-password
```

新密码只在当前终端显示；所有旧登录会话失效。不要将终端输出分享给他人。自定义部署须使用对应程序和数据库路径。

后台主机上 `sudo nodehelm-hub`（安装前为 `nodehelm.sh`）的 `update`、`logs`、`reset-password`、`uninstall` 覆盖日常维护，无参数在终端运行时显示菜单，无终端时需加 `--yes`（更新）；备份与恢复使用后台「数据」页；节点上 `sudo nodehelm` 负责升级、状态、日志、换绑与卸载，`recover` 处理中断；`logs` 先显示服务状态与本机检查，并隐藏应急密码。中断的更新或卸载在下次运行时自动完成。卸载删除全部数据（数据库、密钥、主题、维护记录和服务器上的备份）。维护工具保存在 `/opt/node-manager-hub/tools`，恢复与卸载不需要联网。管理组件、原监控 Agent、受管业务实例各有独立生命周期，不能通过删除整个 `/opt` 目录处理。`install-hub.sh` 不支持原安装器的 `--reset-password` 或 `--uninstall` 参数。

## 故障定位

| 现象 | 优先检查 |
| --- | --- |
| 页面无法打开 | hub 服务、回环监听端口、代理上游、HTTPS 证书和 DNS |
| 页面正常但节点不上线 | agent 服务、token、节点实际连接域名、WebSocket 升级、注册窗口是否有效 |
| 添加节点被拒绝 | 使用 HTTPS 域名、`--site` 值、同源请求条件；查看返回错误和 hub 日志 |
| 流量历史为空 | 是否刚建立基线、是否为管理员、是否超出保留范围；“未记录”不等于零 |
| 时段看似错位 | hub 时区、容器 TZ、跨时段采样与断报补记规则 |
| 构建找不到静态资源 | 先构建 `web-admin` 与 `web-theme`，见 [开发文档](DEVELOPMENT.md) |
| 更新失败 | Release 资产、校验文件、GitHub 连通性、服务日志及回滚结果 |

本文描述安装器和源码行为；实际部署还需确认服务状态、代理连通性及 agent 接入，不能仅凭安装命令返回成功判断验收完成。
