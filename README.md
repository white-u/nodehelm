# NodeHelm

自托管的 VPS 节点监控与服务管理后台：一个 Rust 后台（hub）加上节点上的监控 Agent 和可选的服务管理组件。监控、分时流量和公开状态页开箱即用；需要时再为节点接入服务管理，在网页上维护 Snell、singbox、realm 转发和端口流量统计。

文档：**[快速开始](QUICK_START.md)** · [部署文档](DEPLOY.md) · [服务管理与恢复](NODE_MANAGEMENT_DEPLOY.md) · [运维速查](OPERATIONS.md) · [开发与验证](DEVELOPMENT.md) · [完整索引](INDEX.md)

## 功能

**监控**

- 实时指标：CPU、内存、磁盘、网速、连接数等秒级上报，历史曲线按保留天数查看
- 延迟探测：节点对指定目标做 TCP 握手探测，记录延迟与丢包
- 流量与计费：累计与账期流量、额度、到期日和自动续期
- 分时流量历史：每日 00–08、08–14、14–20、20–24 四个时段的上传、下载与合计
- 公开状态页：内置默认主题，可安装第三方主题；匿名访问只看到公开节点，不含地址、主机名和备注
- 通知：节点离线与恢复、延迟持续异常、流量、到期与续期、登录、服务管理事件，推送到 Telegram 或自定义 Webhook
- 后台维护：GitHub 登录与应急密码、会话管理、数据库备份与恢复、版本更新提示

**服务管理（可选，按节点接入）**

- Snell、singbox（Reality／SS2022）、realm 转发（TCP/UDP）的实例新增、修改和删除
- 可信核心的安装、升级、回退与卸载，下载地址和摘要由组件内的清单固定
- Snell 的 Surge 配置与 singbox 的 vless／ss 分享链接，按实例查看和复制
- 端口流量统计：受管业务端口的用量、配额、按月重置、到期、出站限速、流量阈值提醒，在线 IP 统计、告警与阻断，近 7 天流量走势
- 节点模块卸载与整节点卸载，进度持久保存，中断后可继续
- 节点主动经 HTTPS 轮询后台，不开放入站端口；后台不向节点推送程序

分时流量按采样当时的 hub 本地时间记账，跨时段采样和断报补记归后一次采样所在时段，无法精确还原实际时间分布；没有记录的时段显示「未记录」。

## 安装

在 systemd Linux 上以 root 运行，按提示填写 HTTPS 地址：

```sh
curl -fsSL https://github.com/white-u/nodehelm/releases/latest/download/nodehelm.sh | sudo sh
```

后台默认仅监听 `127.0.0.1:9911`，使用独立的 `node-manager-hub` 服务及数据目录。配置自己的 HTTPS 反向代理后访问 `/admin/`，先以安装时显示的应急密码登录，再设置 GitHub 登录。安装后用 `sudo nodehelm-hub` 更新、查看日志、重置密码和卸载；节点接入服务管理后用 `sudo nodehelm` 升级、换绑和卸载；备份与恢复在后台「数据」页。

四步上手见 [快速开始](QUICK_START.md)，离线安装和全部参数见 [部署文档](DEPLOY.md)。

## 组成

| 目录 | 说明 |
|---|---|
| `src/`、`web-admin/` | hub：后台、API、公开页宿主（Rust／axum／SQLite，React／TypeScript） |
| `agent/` | Linux 监控 Agent，随后台发布并嵌入后台，节点从后台下载 |
| `management/` | 节点服务管理组件（Python，Snell／singbox／realm 转发／端口流量统计） |
| `web-theme/` | 内置默认主题，随后台发布并嵌入后台 |
| `scripts/` | 后台安装、更新、卸载与发行打包工具 |

```
监控：    monitor-agent ──WebSocket / JSON-RPC 2.0──▶ hub ──▶ 后台 + 状态页
服务管理：后台页面 ──▶ hub 任务 ◀──HTTPS 轮询── 节点管理组件 ──▶ 业务核心与 systemd 服务
```

程序名 `monitor-hub`、服务名 `node-manager-*` 和 `/opt/node-manager` 等路径是已部署节点依赖的内部标识，与项目名 NodeHelm 不同属正常。

## 来源与许可

本项目派生自 [monitor-probe/monitor](https://github.com/monitor-probe/monitor)、[monitor-probe/agent](https://github.com/monitor-probe/agent) 与 [monitor-theme-default](https://github.com/monitor-probe/monitor-theme-default)（MIT），现已独立维护和发布，不要用上游安装器覆盖或升级本项目。公开页的主题契约与上游兼容，主题站 [monitor-themes.pages.dev](https://monitor-themes.pages.dev) 收录的第三方主题可在面板安装。主题与后台同源运行，在已登录的浏览器中能以管理员身份操作，只安装可信来源。

许可证见 [LICENSE](LICENSE)。
