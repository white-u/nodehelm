# NodeHelm 项目与文件索引

本项目从 monitor Hub 派生，包含 Rust 后端、React 管理界面、可选 Python 节点执行器及部署工具。项目定位与调用关系见 [AGENTS.md](AGENTS.md)。

## 项目与模块文档

| 阅读范围 | 入口 | 文件索引内容 |
| --- | --- | --- |
| 全项目 | [AGENTS.md](AGENTS.md) | 项目定位、模块分工及根目录配置 |
| Rust 后端 | [src/AGENTS.md](src/AGENTS.md) | 监控、登录、数据库、分时流量与服务管理 API |
| VPS 管理执行器 | [management/AGENTS.md](management/AGENTS.md) | Agent、事务、节点生命周期与四个业务模块 |
| 管理界面 | [web-admin/AGENTS.md](web-admin/AGENTS.md) | 登录、节点、探测、服务、设置及共享组件 |
| 部署工具 | [scripts/AGENTS.md](scripts/AGENTS.md) | 安装维护、主题、离线打包与工作流 |

主要源码和配置的逐文件职责统一列在各模块文档中。

## 功能与使用文档

| 阅读目的 | 入口 |
| --- | --- |
| 使用入口及安装介绍 | [README.md](README.md) |
| 独立部署、反代、接入和恢复 | [NODE_MANAGEMENT_DEPLOY.md](NODE_MANAGEMENT_DEPLOY.md) |
| 日常运维速查 | [OPERATIONS.md](OPERATIONS.md) |
| 源码构建与联调 | [DEVELOPMENT.md](DEVELOPMENT.md) |

## 常用任务入口

| 任务 | 定位 |
| --- | --- |
| 登录、监控、数据库与分时流量 | [后端功能定位](src/AGENTS.md#按功能定位) |
| 前端业务目录与导航组合 | [业务文件索引](web-admin/AGENTS.md#业务文件索引) |
| Snell／SB／FW／PM 配置及驱动 | [四个业务模块入口](management/AGENTS.md#四个业务模块入口) |
| 节点卡片、配置与管理页签 | [服务管理页面定位](web-admin/AGENTS.md#服务管理页面定位) |
| 服务管理后端接口、模型与生命周期 | [management/](src/management/) |
| 后台安装维护、组件维护与发行打包 | [部署工具索引](scripts/AGENTS.md)、[组件安装维护](management/install.py) |

## 相关项目

外部监控 Agent 来自 `monitor-probe/agent`。

新增或移动文件时维护对应模块索引；本页维护导航，使用说明放在上表对应文档中。
