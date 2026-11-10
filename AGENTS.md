# NodeHelm 项目说明

本文件提供项目定位、模块分工和源码导航。进入子目录时继续阅读对应的 `AGENTS.md`；完整文档入口见 [INDEX.md](INDEX.md)。

## 项目定位

NodeHelm 是基于 monitor Hub 派生的独立节点管理项目，包含监控、分时流量和 Snell／singbox／realm 转发／端口流量统计服务管理。后台按节点组织服务模块；代理订阅功能已取消。

当前 Rust 包名与可执行文件名仍为 `monitor-hub`；`node-manager-*` 服务名、`/opt/node-manager` 等路径和 `X-Node-Manager-*` 请求头是已部署节点依赖的内部标识，改名时不要更动。服务端使用 Rust／axum／SQLite，管理界面与默认公开主题使用 React／TypeScript，VPS 上的可选管理执行器使用 Python。`agent/` 中的 Rust `monitor-agent` 负责监控上报，随后台发布并嵌入后台；本项目的 Python Agent 负责服务管理。

## 模块文档索引

| 模块 | 目录 | 模块说明 |
| --- | --- | --- |
| 后端与监控数据 | `src/` | [src/AGENTS.md](src/AGENTS.md) |
| VPS 管理执行器与业务模块 | `management/` | [management/AGENTS.md](management/AGENTS.md) |
| 管理界面 | `web-admin/` | [web-admin/AGENTS.md](web-admin/AGENTS.md) |
| 默认公开主题 | `web-theme/` | [web-theme/README.md](web-theme/README.md) |
| 部署、维护与打包工具 | `scripts/` | [scripts/AGENTS.md](scripts/AGENTS.md) |
| 监控 Agent | `agent/` | [agent/README.md](agent/README.md) |

## 主要调用关系

- 监控：monitor-agent（`agent/`，由后台 `/agent/{arch}` 分发）→ WebSocket → Rust Hub → SQLite／实时状态 → 管理后台与公开主题。
- 服务管理：管理界面 → 管理 API → 节点任务 → Python Agent 主动 HTTPS 轮询 → Controller → 业务驱动 → 结果回执。
- 节点卸载：后台生命周期任务 → 节点持久步骤 → 模块清理 → 全部卸载时由独立执行器清理管理组件并确认结果。
- 页面资源：管理界面与默认主题（`web-theme/`）的构建产物由 Hub 嵌入并提供；业务模块文件由管理分发接口提供。

## 根目录文件索引

| 文件 | 内容 |
| --- | --- |
| [Cargo.toml](Cargo.toml)、[Cargo.lock](Cargo.lock) | Rust 包信息与依赖 |
| [build.rs](build.rs)、[Cross.toml](Cross.toml) | 准备嵌入的监控 Agent；发布构建的环境透传 |
| [nodehelm.sh](nodehelm.sh)、[install-hub.sh](install-hub.sh)、[update-hub.sh](update-hub.sh)、[install.sh](install.sh) | 后台一键入口与本机 `nodehelm-hub` 菜单、包内安装与更新、监控 Agent 的安装入口 |
| [Dockerfile](Dockerfile)、[工作流目录](.github/workflows/) | 容器与自动化流程 |
| [.gitignore](.gitignore)、[rustfmt.toml](rustfmt.toml)、[LICENSE](LICENSE) | 忽略规则、Rust 格式配置与许可证 |

## 文档维护

源码入口、目录或模块职责变化时，同步对应模块 `AGENTS.md` 和根 [INDEX.md](INDEX.md)。模块说明保持为简洁的定位入口，规划、设计和测试记录不进入仓库。 代码按业务或主要职责成组组织，相近表单、操作和状态逻辑放在同一文件；不按单个按钮、动作或 Hook 拆文件。
