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
| 服务总览 | [services/Management.tsx](src/features/services/Management.tsx)：节点卡片、卡片内展开、接入筛选、状态读取、选择和结果确认调度 |
| 服务配置与维护 | [services/ServiceDetail.tsx](src/features/services/ServiceDetail.tsx)：模块配置、Snell／singbox／realm 转发页内表单、配置文本、独立节点管理（生成一行接入命令）及顶部核心维护和操作记录 |
| 服务页交互 | [services/ServiceInteractions.tsx](src/features/services/ServiceInteractions.tsx)：页内展开区、异步操作确认、放弃草稿和节点名称确认 |
| 端口流量统计与管理模型 | [services/PortPolicies.tsx](src/features/services/PortPolicies.tsx)：端口策略、配额进度与近 7 天用量图，新增在按钮下方、编辑在规则卡片内展开（与其他模块一致）；[model.ts](src/features/services/model.ts)：类型、固定文案和数据辅助 |
| 常规设置 | [settings/GeneralSettings.tsx](src/features/settings/GeneralSettings.tsx)：站点设置、图标及共享设置状态逻辑 |
| 通知与主题 | [settings/Notifications.tsx](src/features/settings/Notifications.tsx)：通知配置、离线与延迟通知的节点选择；[Themes.tsx](src/features/settings/Themes.tsx)：主题安装、选择和配置 |
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
| [tests/](tests/)、[src/lib/api.test.ts](src/lib/api.test.ts)、[services/model.test.ts](src/features/services/model.test.ts) | 浏览器流程、请求辅助及服务表单数据的回归脚本 |

## 服务管理页面定位

状态读取、目标选择及结果确认调度由 `Management.tsx` 负责；配置区、节点管理和客户端配置在 `ServiceDetail.tsx`；流量统计表单和用量逻辑在 `PortPolicies.tsx`；类型、文案与数据辅助在 `model.ts`。

**节点卡片与筛选（`Management.tsx`）**

- 每个节点是一张独立卡片，首行含状态圆点、名称、分组和节点页同样的生效地址（搜索也用它）、四个模块入口和独立的“节点管理”入口；未接入节点以“接入管理”代替模块入口，不展示默认模块配置。
- 列表按“已接入／未接入／全部”筛选，默认已接入，用户未手动选择且尚无已接入节点时显示全部；未读到状态的节点只在全部中显示。搜索和筛选都不隐藏正在展开的节点。
- 默认全部收起；点击模块后配置区在该卡片内首行下方展开，同一时间只展开一个，再次点击同一模块收起（有草稿时先确认），切换后滚动到该卡片。
- “节点管理”在对应节点卡片内展开，单独选择目标；再次点击入口或面板“收起”关闭；打开时暂时隐藏模块配置区，保留所选模块和草稿。
- 进行中的模块操作只在模块入口显示“执行中”，节点名称旁只显示整节点卸载状态；卡片展开配置时不重复卡片上的读取失败和待核实提示。

**配置区（`ServiceDetail.tsx`）**

- 不另加外框；标题行显示模块名、核心版本与可升级版本、读取时间、刷新和“模块维护”（展开回退和卸载），位于实例列表上方。未安装时可直接安装。
- 新增表单在列表上方展开，只有底部“取消”；Snell／singbox／realm 转发 的编辑嵌在对应实例内，监听端口只读，再次点击可收起。保留未保存修改、版本过期和重新载入保护。
- Snell 只填写可选名称和监听端口；singbox 将协议及 Reality 的 SNI 放在普通表单中；客户端端口默认跟随监听端口，NAT 映射时在折叠项中修改，并显示“访问地址:客户端端口 → 监听端口”。Snell／singbox 名称留空时在提交前使用端口号，保留现有节点协议与隐藏的访问地址、模式及密钥保留语义。
- 实例行展示名称、运行状态圆点和端口（名称即端口时不重复），映射时附客户端端口；删除为弱化按钮；数字输入框限宽。
- Snell 配置文本及 singbox 的 vless／ss 分享链接在对应实例下方展开，只展示配置和复制按钮。
- 操作进行中配置区只保留一行说明；结果只显示一条文字，成功提示数秒后自动消失，失败保留到下一次操作；操作记录默认折叠。提示与操作记录的颜色由 `model.ts` 的 `serviceFeedbackClass` 统一，先看结果再看动作。
- 服务操作、草稿放弃和名称确认均在页内完成（`ServiceInteractions.tsx`），不使用原生确认框或侧栏。

**节点管理（`ServiceDetail.tsx` 的 `NodeManagement`）**

- 负责接入（生成一行接入命令）、全部卸载、紧急撤销授权、失效节点处理及卸载进度恢复。
- 完整卸载进度在节点管理中查看，卸载结果默认折叠。

**端口流量统计（`PortPolicies.tsx`）**

- 规则卡片首行为名称、端口、状态和编辑／重置用量／删除，次要参数收在“规则详情”，与“用量详情”同排切换、内容在下方整行展开，统计说明收在“统计口径”。
- 每条规则显示 TCP 在线 IP（按告警／报警着色）；IP 告警按“告警阈值”填写，新规则默认 >3 告警、>5 报警、>6 阻断后续连接；白名单支持 IP 地址及 CIDR 网段。
- 表单分“基础／配额与周期／IP 告警与阻断”三组，提醒推送未启用时以琥珀色提示。新增在按钮下方、编辑在规则卡片内展开。
- “用量详情（近 7 天）”首次展开时读取小时历史，按 UTC 与账期一致，由 `model.ts` 的 `pmTrafficBars` 分组并区分无记录／未来时段。

**实例与流量统计的联动**

- `PmFields` 同时用于新增实例时可选的“同时启用流量统计”（默认不勾选）。
- 实例行在端口后提供“已统计流量／添加流量统计”入口，跳到端口流量统计：已统计时定位到对应规则，添加时打开新增表单。
- 删除带流量统计规则的实例时先删规则再删实例。
- 两步操作随第一步一起提交（请求的 `then`），后台在第一步成功后自行排入第二步，页面关闭也会完成；`Management.tsx` 依次跟踪两步结果（第二步记录的 `after` 指向第一步），期间显示进度并暂停写入；第二步未成功时保留流量统计参数供“添加流量统计”继续。

字段与动作对应 [management/tasks.rs](../src/management/tasks.rs)、[models.rs](../src/management/models.rs) 和 [Python 模型](../management/AGENTS.md)。页面功能及交互变化时，同步这些实际消费者与对应文档入口。
