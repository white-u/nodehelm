# Rust 后端模块

[项目说明](../AGENTS.md) · [总索引](../INDEX.md)

本目录负责 Hub 启动、HTTP／WebSocket、登录、监控、流量记账、通知、页面资源及服务管理接口。管理界面位于 [web-admin](../web-admin/AGENTS.md)，节点执行器位于 [management](../management/AGENTS.md)。

## 文件索引

| 文件 | 职责 |
| --- | --- |
| [main.rs](main.rs) | 启动参数、共享 `App`、路由组合、定时维护与进程生命周期 |
| [auth.rs](auth.rs) | 密码、GitHub OAuth、Cookie 会话、登录节流和来源解析 |
| [api.rs](api.rs) | 节点、设置、探测、主题、数据库维护及公开／管理视图 |
| [agent_ws.rs](agent_ws.rs) | 外部监控 Agent 的 WebSocket／JSON-RPC、连接和读数处理 |
| [db.rs](db.rs) | SQLite 初始化、迁移、节点与历史数据、流量累计及备份恢复 |
| [traffic_period.rs](traffic_period.rs) | 四时段归类、扩展表、历史读取与裁剪 |
| [management/mod.rs](management/mod.rs) | 管理开关、登记绑定、快照、普通任务、回执、配置展示和组件文件分发 |
| [management/lifecycle.rs](management/lifecycle.rs) | 节点模块／全部卸载任务、进度、继续操作及最终确认 |
| [management/models.rs](management/models.rs) | 模块字段、核心元数据、业务／原 PM 快照模型与 sing-box 客户端配置生成 |
| [notify.rs](notify.rs) | 通知模板、队列及 Telegram／Webhook 发送 |
| [frontend.rs](frontend.rs) | 管理静态资源、公开主题、主题包与图标 |
| [testdata/](testdata/) | 旧数据库 schema 和分时流量回归材料 |

服务管理集中在 `management/`：`mod.rs` 提供接口与任务逻辑，`models.rs` 和 `lifecycle.rs` 为其内部子模块。顶层模块由 `main.rs` 组合，服务管理路由由 `main.rs` 与 `management/mod.rs` 共同挂载。

## 按功能定位

| 功能 | 关联入口 |
| --- | --- |
| 登录与后台导航 | `auth.rs` → [Login.tsx](../web-admin/src/features/auth/Login.tsx)／[App.tsx](../web-admin/src/App.tsx) |
| 监控展示 | `agent_ws.rs` → `db.rs`／`api.rs` → [Nodes.tsx](../web-admin/src/features/nodes/Nodes.tsx) |
| 分时流量 | `agent_ws.rs` → `db.rs` → `traffic_period.rs` → `api.rs` → [TrafficHistory.tsx](../web-admin/src/components/TrafficHistory.tsx) |
| 服务配置 | [Management.tsx](../web-admin/src/features/services/Management.tsx) → `management/mod.rs`／`management/models.rs` → [agent.py](../management/agent.py) |
| 节点卸载 | `management/lifecycle.rs` ↔ [lifecycle.py](../management/lifecycle.py) |

模块字段、动作、状态和错误提示的定义分布在 Rust、Python 模型和管理页面；修改协议时从这三处共同定位。
