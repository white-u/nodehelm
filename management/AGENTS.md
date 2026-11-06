# VPS 管理执行器模块

[项目说明](../AGENTS.md) · [总索引](../INDEX.md)

本目录是部署到 VPS 的 Python 管理组件。Agent 主动轮询后台并交给 Controller 执行，业务模块驱动处理配置、核心与 systemd 服务。这里的 `agent.py` 与 `agent/` 的 monitor-agent 是两个进程。

## 文件索引

| 文件 | 职责 |
| --- | --- |
| [agent.py](agent.py) | 轮询、快照组合、模块任务分派、回执确认、本地状态／恢复／解绑入口 |
| [install.py](install.py) | 安装登记、首次上报确认、组件升级回退、恢复和组件移除 |
| [controller.py](controller.py) | 模块事务、版本比对、去重回执、提交和失败恢复 |
| [lifecycle.py](lifecycle.py) | 持久卸载计划、步骤推进、模块回执确认和独立清理执行器 |
| [model.py](model.py) | 普通请求、Snell 字段、配置版本摘要和错误模型 |
| [modules_model.py](modules_model.py) | SB／FW／PM 字段及参数模型 |
| [storage.py](storage.py) | 状态文件读写、原子替换与目录操作 |
| [snell.py](snell.py) | Snell 核心、实例配置、systemd 服务及核心版本操作 |
| [services.py](services.py) | SB／FW 驱动、配置生成、目标检查、服务与核心版本操作 |
| [pm.py](pm.py) | 受管 PM 计量、配额、重置、到期策略、限速和定时维护 |
| [menu.py](menu.py) | 节点本机菜单 `sudo nodehelm`：升级、状态、日志与恢复，命令随组件安装和清理 |
| [port_monitor.py](port_monitor.py) | 原脚本 PM 配置与计数的读取和解析 |
| [core_assets.json](core_assets.json) | 核心版本、架构、发行地址与摘要清单 |
| [tests/](tests/) | 执行器、业务驱动、安装及后台维护工具的回归和集成脚本；[operations.json](tests/operations.json) 是后台与组件共用的操作校验样例 |

## 四个业务模块入口

| 模块 | 字段与协议 | 执行驱动 |
| --- | --- | --- |
| Snell | [model.py](model.py) | [snell.py](snell.py) |
| SB：Reality／SS2022 | [modules_model.py](modules_model.py) | [services.py](services.py) 的 `sb` 分支 |
| FW：Realm 转发 | [modules_model.py](modules_model.py) | [services.py](services.py) 的 `fw` 分支 |
| PM：端口策略 | [modules_model.py](modules_model.py) | [pm.py](pm.py) 与 `agent.py` 的 `PMController` |

后台对应的模型在 [models.rs](../src/management/models.rs)，页面在 [Management.tsx](../web-admin/src/features/services/Management.tsx)。新增模块或调整字段时，沿模型、驱动、任务分派、后台模型及页面入口定位。

## 执行与状态关系

- 普通操作：`agent.py` → `Controller.execute` → 业务驱动；恢复通过 `Controller.recover`。
- 模块卸载：`Lifecycle` 生成关联 PM 规则、实例、核心的顺序计划，每步复用 Controller。
- 全部卸载：模块清理后，`node-manager-cleanup.service` 使用独立材料完成组件清理与收尾确认。
- 安装登记：`install.py` 持久待登记身份，完成绑定后，由 Agent 的本次首次上报确认安装结果。

默认程序目录为 `/opt/node-manager`，状态目录为 `/var/lib/node-manager`；SB／FW／PM 的状态在各自子目录，Snell 位于状态根目录。`management/install.py` 的 `FILES` 定义组件文件集；后台 [management/mod.rs](../src/management/mod.rs) 的 `files` 处理分发。

`state.json` 表示模块状态，`recovery.json` 记录事务，`receipts/` 保存模块回执；`lifecycle-job.json` 保存节点卸载进度及本地确认，独立收尾材料位于 `/var/lib/node-manager-cleanup`。
