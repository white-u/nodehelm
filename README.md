# NodeHelm

基于 monitor 的独立新项目，保留监控与分时流量，新增可选节点服务管理。当前支持 Snell、SB（Reality / SS2022）和 FW（Realm TCP/UDP）实例管理、可信核心升级回退与卸载、Snell/SB 配置导出，以及受管业务端口的 PM 计量、配额、重置、到期策略、流量阈值、在线 IP 统计、告警与阻断，以及近 7 天的端口流量走势。

文档：**[快速开始](QUICK_START.md)** · [部署文档](DEPLOY.md) · [完整索引](INDEX.md) · [开发与验证](DEVELOPMENT.md) · [部署与恢复说明](NODE_MANAGEMENT_DEPLOY.md)

本项目派生自 [monitor-probe/monitor](https://github.com/monitor-probe/monitor) 与 [monitor-probe/agent](https://github.com/monitor-probe/agent)（MIT），已独立发布，不要用上游安装器覆盖或升级本项目。公开页主题与上游兼容，主题站 [monitor-themes.pages.dev](https://monitor-themes.pages.dev) 收录的第三方主题可在面板安装。主题与后台同源运行，在已登录的浏览器中能以管理员身份操作，只安装可信来源。

## 特性

- 实时监控：秒级实时数据展示
- 轻量高效：Rust 语言构建，低资源占用，极简高效
- 自托管：完全掌控数据隐私，部署简单
- 通知：节点掉线、流量、到期与登录，推送到 Telegram 或自定义 Webhook
- 分时流量历史：后台节点的「历史」可查看每日 00–08、08–14、14–20、20–24 的上传、下载与已记录合计，沿用历史保留天数。

分时流量按采样当时的 hub 本地时间记账，跨时段采样和断报补记归后一次采样所在时段，无法精确还原实际时间分布。旧每日累计不拆分回填，没有记录的时段显示「未记录」。本 fork 使用独立扩展表，不占用上游数据库迁移编号。

## 安装 NodeHelm

在 systemd Linux 上以 root 运行，按提示填写 HTTPS 地址；再次运行即更新：

```sh
curl -fsSL https://github.com/white-u/nodehelm/releases/latest/download/nodehelm.sh | sudo sh
```

离线安装和全部参数见 [部署文档](DEPLOY.md)。

后台默认仅监听 `127.0.0.1:9911`，使用独立 `node-manager-hub` 服务及数据目录。配置自己的 HTTPS 反代后访问 `/admin/`，先以应急密码登录，再设置 GitHub 登录。安装后用 `sudo nodehelm-hub` 更新、备份、恢复和卸载；不要用原 monitor 安装器升级本后台。

## 组成

| 目录 | 说明 |
|---|---|
| `src/`、`web-admin/` | hub：后台、API、公开页宿主 |
| `agent/` | Linux 监控 agent，随后台发布并嵌入后台，节点从后台下载 |
| `management/` | 节点服务管理组件（Snell／SB／FW／PM） |
| [monitor-theme-default](https://github.com/monitor-probe/monitor-theme-default) | 内置默认主题，构建时按 `web-theme.pin` 的版本与校验和取用 |

```
agent (Linux)  ──WebSocket / JSON-RPC 2.0──▶  hub (axum + SQLite)  ──▶  后台 + 状态页
```
