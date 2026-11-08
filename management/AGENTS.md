# VPS 管理执行器模块

[项目说明](../AGENTS.md) · [总索引](../INDEX.md)

本目录是部署到 VPS 的 Python 管理组件。Agent 主动轮询后台并交给 Controller 执行，业务模块驱动处理配置、核心与 systemd 服务。这里的 `agent.py` 与 `agent/` 的 monitor-agent 是两个进程。

## 文件索引

入口文件名被 systemd 单元、`enroll.sh`、清理执行器和 `nodehelm` 命令引用，不要改名：`agent.py`、`install.py`、`lifecycle.py`、`menu.py`、`pm.py`。`install.py` 与 `lifecycle.py` 还会被复制到独立清理目录单独运行，只能依赖标准库和彼此。

| 文件 | 职责 |
| --- | --- |
| [agent.py](agent.py) | 轮询、快照组合、模块任务分派及跨模块端口规则（PM 跟随业务端口）、回执确认、本地状态／恢复／解绑入口 |
| [install.py](install.py) | 安装登记（上报本机 boot ID 供后台核对机器）、首次上报确认、组件升级回退、恢复和组件移除 |
| [lifecycle.py](lifecycle.py) | 持久卸载计划、步骤推进、模块回执确认和独立清理执行器 |
| [menu.py](menu.py) | 节点本机菜单 `sudo nodehelm`：升级、状态、日志与恢复，命令随组件安装和清理 |
| [modules.py](modules.py) | 模块注册表：模块清单、请求校验入口、各模块 Controller 与核心驱动 |
| [controller.py](controller.py) | 与模块无关的事务：版本比对、去重回执、提交和失败恢复 |
| [protocol.py](protocol.py) | 协议版本、请求外层校验、配置版本摘要、错误模型及多模块共用的字段检查 |
| [core_driver.py](core_driver.py) | Snell／SB／FW 共用的核心驱动基类：可信核心下载、校验、切换与回退，实例文件及 systemd 单元归属；系统命令与原子写入 |
| [storage.py](storage.py) | 状态文件读写、原子替换与目录操作 |
| [core_assets.json](core_assets.json) | 核心版本、架构、发行地址与摘要清单 |
| [tests/](tests/) | 执行器、业务驱动、安装及后台维护工具的回归和集成脚本；[operations.json](tests/operations.json) 是后台与组件共用的操作校验样例 |

## 四个业务模块入口

每个模块一个文件，包含 `MODULE`、支持的 `ACTIONS`、字段校验 `resource`、驱动 `Driver` 和 `controller(root)`，由 [modules.py](modules.py) 注册。修改一个模块只改它自己的文件；模块之间不互相引用，共用部分只放在 `protocol.py` 和 `core_driver.py`。

| 模块 | 文件 | 内容 |
| --- | --- | --- |
| Snell | [snell.py](snell.py) | Snell 6 字段、配置与服务模板 |
| SB：Reality／SS2022 | [sb.py](sb.py) | 字段、sing-box 配置、密钥生成、Reality 目标与配置校验 |
| FW：Realm 转发 | [fw.py](fw.py) | 字段、Realm 配置、自环检查与目标连通检查 |
| PM：端口策略 | [pm.py](pm.py) | 字段、计量、配额、重置、到期策略、限速、在线 IP 统计、告警与阻断、定时维护及 `PMController` |

后台对应的模型在 [models.rs](../src/management/models.rs)，页面在 [Management.tsx](../web-admin/src/features/services/Management.tsx)。新增模块：新建模块文件，在 `modules.py` 注册，并加入后台 [mod.rs](../src/management/mod.rs) 的 `FILES`；再沿后台模型及页面入口补齐。

## 执行与状态关系

- 普通操作：`agent.py` → `Controller.execute` → 业务驱动；恢复通过 `Controller.recover`。
- 模块卸载：`Lifecycle` 生成关联 PM 规则、实例、核心的顺序计划，每步复用 Controller。
- 全部卸载：模块清理后，`node-manager-cleanup.service` 使用独立材料完成组件清理与收尾确认。
- 安装登记：后台的 `enroll.sh` 校验并运行 `install.py`；`install.py` 持久待登记身份，完成绑定后，由 Agent 的本次首次上报确认安装结果。后台明确拒绝时显示其原因，并清除本次新建的待登记身份。

默认程序目录为 `/opt/node-manager`，状态目录为 `/var/lib/node-manager`；SB／FW／PM 的状态在各自子目录，Snell 位于状态根目录。组件文件集由后台 [management/mod.rs](../src/management/mod.rs) 的 `FILES` 定义并分发，`install.py` 按清单安装、替换和移除。

`state.json` 表示模块状态，`recovery.json` 记录事务，`receipts/` 保存模块回执；`lifecycle-job.json` 保存节点卸载进度及本地确认，独立收尾材料位于 `/var/lib/node-manager-cleanup`。

PM 仅管理当前受管业务端口，不读取旧脚本配置；IP 白名单支持单个地址与 IPv4／IPv6 CIDR 网段，匹配来源不计数、不阻断。
