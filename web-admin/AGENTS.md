# 管理界面模块

[项目说明](../AGENTS.md) · [总索引](../INDEX.md) · [后端入口](../src/AGENTS.md)

管理界面使用 React、TypeScript、Vite 和 Tailwind，页面位于 `/admin/`，业务 API 位于 `/api/`。`App.tsx` 处理身份和顶层状态，`Admin.tsx` 组合导航与业务页面；具体功能按业务放在 `features/`，通用基础组件仍放在 `components/ui/`。

## 业务文件索引

| 模块 | 文件与职责 |
| --- | --- |
| 登录 | [auth/Login.tsx](src/features/auth/Login.tsx)：密码与 GitHub 登录页面 |
| 节点 | [nodes/Nodes.tsx](src/features/nodes/Nodes.tsx)：列表与节点操作；[NodeForms.tsx](src/features/nodes/NodeForms.tsx)：新增、编辑、计费与分组表单 |
| 节点控件与安装 | [nodes/NodeControls.tsx](src/features/nodes/NodeControls.tsx)：地址、筛选与节点选择；[AgentInstall.tsx](src/features/nodes/AgentInstall.tsx)：监控 Agent 安装、注册及命令辅助 |
| 延迟探测 | [monitoring/Ping.tsx](src/features/monitoring/Ping.tsx)：探测列表、表单、节点关联与视图切换 |
| 服务总览 | [services/Management.tsx](src/features/services/Management.tsx)：节点卡片、状态读取、选择和结果确认调度 |
| 服务配置与维护 | [services/ServiceDetail.tsx](src/features/services/ServiceDetail.tsx)：详情页签、Snell／SB／FW 表单、配置弹窗、节点和核心维护及操作记录 |
| PM 与管理模型 | [services/PortPolicies.tsx](src/features/services/PortPolicies.tsx)：端口策略；[model.ts](src/features/services/model.ts)：类型、固定文案和数据辅助 |
| 常规设置 | [settings/GeneralSettings.tsx](src/features/settings/GeneralSettings.tsx)：站点设置、图标及共享设置状态逻辑 |
| 通知与主题 | [settings/Notifications.tsx](src/features/settings/Notifications.tsx)：通知配置与离线策略；[Themes.tsx](src/features/settings/Themes.tsx)：主题安装、选择和配置 |
| 系统维护 | [settings/Maintenance.tsx](src/features/settings/Maintenance.tsx)：会话、安全、数据库及版本更新页面 |
| 共享控件 | [shared/controls.tsx](src/features/shared/controls.tsx)：现有多页面共用的表单、选择器、确认弹窗、复制和拖拽控件 |

相近的页面、表单和操作保留在同一文件；业务模块通过共享控件及请求层组合，不引用导航入口 `Admin.tsx`。

## 入口与配置索引

| 文件或目录 | 职责 |
| --- | --- |
| [index.html](index.html)、[src/main.tsx](src/main.tsx) | HTML 入口与 React 挂载 |
| [src/App.tsx](src/App.tsx)、[src/components/Admin.tsx](src/components/Admin.tsx) | 身份、主题、路径切换及后台导航组合 |
| [src/components/TrafficHistory.tsx](src/components/TrafficHistory.tsx) | 分时流量历史展示 |
| [src/components/ui/](src/components/ui/) | Button、Dialog、Input 等基础组件 |
| [src/lib/api.ts](src/lib/api.ts) | API、数据类型、监控帧订阅及安装辅助逻辑 |
| [src/lib/format.ts](src/lib/format.ts)、[src/lib/utils.ts](src/lib/utils.ts)、[src/index.css](src/index.css) | 格式化、样式辅助及主题样式 |
| [vite.config.ts](vite.config.ts) | `/admin/` 基路径、`@` 别名与开发代理 |
| [package.json](package.json)、[package-lock.json](package-lock.json) | 前端依赖、脚本和依赖锁定 |
| [tsconfig.json](tsconfig.json)、[tsconfig.app.json](tsconfig.app.json)、[tsconfig.node.json](tsconfig.node.json) | TypeScript 入口、应用和工具配置 |
| [components.json](components.json)、[.oxlintrc.json](.oxlintrc.json)、[public/](public/) | UI 组件、代码规则与静态资源 |
| [tests/](tests/)、[src/lib/api.test.ts](src/lib/api.test.ts) | 浏览器流程与请求辅助逻辑的回归脚本 |

## 服务管理页面定位

节点卡片内每个模块独占一行，名称和状态横向排列；卡片保留读取时间，选择后在下方同一张详情卡片内使用“配置”和“管理”页签。组件最后通信时间及默认折叠的已完成卸载记录放在“管理”页签，未完成或异常的卸载进度在两页签中持续可见。Snell／SB 的实例行通过弹窗展示配置，FW 展示转发目标，PM 使用端口策略面板。状态读取与结果确认在 `Management.tsx`，表单与维护在 `ServiceDetail.tsx`，页面布局与交互由两者组合。

字段与动作对应 [management/mod.rs](../src/management/mod.rs)、[models.rs](../src/management/models.rs) 和 [Python 模型](../management/AGENTS.md)。页面功能及交互变化时，同步这些实际消费者与对应文档入口。
