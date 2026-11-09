# Rust 后端模块

[项目说明](../AGENTS.md) · [总索引](../INDEX.md)

本目录负责 Hub 启动、HTTP／WebSocket、登录、监控、流量记账、通知、页面资源及服务管理接口。管理界面位于 [web-admin](../web-admin/AGENTS.md)，节点执行器位于 [management](../management/AGENTS.md)。

## 文件索引

| 文件 | 职责 |
| --- | --- |
| [main.rs](main.rs) | 启动参数、共享 `App`、路由组合、定时维护与进程生命周期 |
| [auth.rs](auth.rs) | 密码、GitHub OAuth、Cookie 会话、登录节流和来源解析 |
| [api/](api/mod.rs) | 面板与公开页 HTTP 接口：`mod.rs` 管理员校验、同源写保护与错误响应；`nodes.rs` 节点读写与自动注册；`metrics.rs` 指标、分时流量、实时推送与探测任务；`themes.rs` 主题安装、更新与 GitHub 发布读取；`system.rs` 数据库维护、版本信息、会话与设置；`tests.rs` 测试 |
| [agent_ws.rs](agent_ws.rs) | 外部监控 Agent 的 WebSocket／JSON-RPC、连接和读数处理 |
| [db/](db/mod.rs) | SQLite：`mod.rs` 结构、连接、锁、设置与会话；`migrate.rs` 迁移；`nodes.rs` 节点；`metrics.rs` 流量累计、指标及小时层、探测记录；`backup.rs` 数据库文件、备份、压缩与恢复；`tests.rs` 测试 |
| [traffic_period.rs](traffic_period.rs) | 四时段归类、扩展表、历史读取与裁剪 |
| [management/mod.rs](management/mod.rs) | 管理开关、加密与身份、设置、端口流量与配置导出、路由组合 |
| [management/enroll.rs](management/enroll.rs) | 登记绑定（核对执行机器与监控 Agent 的 boot ID）、解绑与放弃、组件文件分发 `FILES` 及生成的一行接入脚本 `enroll.sh` |
| [management/tasks.rs](management/tasks.rs) | 状态读取、面板提交的普通任务、组件轮询与回执、快照事件组合（模块事件由各模块提供）、组件静默与操作记录清理 |
| [management/lifecycle.rs](management/lifecycle.rs) | 节点模块／全部卸载任务、进度、继续操作及最终确认 |
| [management/models.rs](management/models.rs) | SB／FW／PM 字段校验（每个模块一个函数）、核心元数据、业务快照模型、SB 分享链接及 sing-box 客户端配置生成 |
| [management/pm.rs](management/pm.rs) | 后台的 PM 逻辑：受管端口小时流量（由相邻两次上报算出增量，保留 7 天，删除规则时清除），以及上报引起的到期、配额阈值和 IP 告警事件 |
| [notify.rs](notify.rs) | 通知模板、队列及 Telegram／Webhook 发送 |
| [frontend.rs](frontend.rs) | 管理静态资源、公开主题、主题包与图标 |
| [testdata/](testdata/) | 旧数据库 schema 和分时流量回归材料 |

服务管理集中在 `management/`：`mod.rs` 组合路由并放共用部分，`enroll.rs`、`tasks.rs`、`models.rs`、`lifecycle.rs` 和 `pm.rs` 为其内部子模块，测试在 `tests.rs` 与 `lifecycle_tests.rs`。顶层模块由 `main.rs` 组合，服务管理路由由 `main.rs` 与 `management/mod.rs` 共同挂载。

## 按功能定位

| 功能 | 关联入口 |
| --- | --- |
| 登录与后台导航 | `auth.rs` → [Login.tsx](../web-admin/src/features/auth/Login.tsx)／[App.tsx](../web-admin/src/App.tsx) |
| 监控展示 | `agent_ws.rs` → `db/`／`api/nodes.rs`、`api/metrics.rs` → [Nodes.tsx](../web-admin/src/features/nodes/Nodes.tsx) |
| 分时流量 | `agent_ws.rs` → `db/metrics.rs` → `traffic_period.rs` → `api/metrics.rs` → [TrafficHistory.tsx](../web-admin/src/components/TrafficHistory.tsx) |
| 服务配置 | [Management.tsx](../web-admin/src/features/services/Management.tsx) → `management/tasks.rs`／`management/models.rs` → [agent.py](../management/agent.py) |
| 节点卸载 | `management/lifecycle.rs` ↔ [lifecycle.py](../management/lifecycle.py) |

模块字段、动作、状态和错误提示的定义分布在 Rust、Python 模型和管理页面；修改协议时从这三处共同定位。
