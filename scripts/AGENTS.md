# 部署与工具模块

[项目说明](../AGENTS.md) · [总索引](../INDEX.md) · [部署文档](../DEPLOY.md)

本目录包含后台本机安装维护、节点批量升级、发行打包和端到端联调工具。VPS 管理组件自身的安装维护在 [management/install.py](../management/install.py)。

## 文件索引

| 文件 | 职责 |
| --- | --- |
| [hub-install.py](hub-install.py) | 后台首次安装、安装进度与续装 |
| [hub-lifecycle.py](hub-lifecycle.py) | 后台升级、失败回退、中断恢复及卸载 |
| [upgrade-nodes.sh](upgrade-nodes.sh) | 从操作机经 SSH 依次升级多台节点的管理组件，首个失败即停止 |
| [hub_health.py](hub_health.py) | 后台实际进程、监听与 HTTP 健康状态辅助 |
| [package-release.py](package-release.py) | 将后台二进制、安装维护工具、[服务管理与恢复说明](../NODE_MANAGEMENT_DEPLOY.md)与内部摘要打为离线包 |
| [e2e.sh](e2e.sh) | Hub 与 `agent/` 监控 Agent 的端到端联调入口 |

## 相关入口

| 位置 | 关系 |
| --- | --- |
| [nodehelm.sh](../nodehelm.sh) | 后台一键入口与维护菜单，下载校验发布包后调用包内工具；安装后作为本机 `nodehelm-hub` 命令 |
| [install-hub.sh](../install-hub.sh) | 根目录后台安装入口，调用 `hub-install.py` |
| [install.sh](../install.sh) | monitor-agent 安装入口，二进制由后台 `/agent/{arch}` 提供 |
| [build.rs](../build.rs) | Cargo 构建时准备嵌入的监控 Agent |
| [自动化工作流](../.github/workflows/) | `ci.yml` 自动化检查、`audit.yml` 依赖审计、`release.yml` 发行构建与打包 |
| [Dockerfile](../Dockerfile) | 使用构建完成的后台二进制生成容器镜像 |

`hub-install.py`、`hub-lifecycle.py` 都使用 `hub_health.py`。打包入口在 `package-release.py`，具体收录清单定义在其 `files` 中。安装目录、服务单元和状态参数以对应工具中的定义为准；使用说明维护在 [DEPLOY.md](../DEPLOY.md) 与 [NODE_MANAGEMENT_DEPLOY.md](../NODE_MANAGEMENT_DEPLOY.md)；后者随离线包分发，改名时同步 `package-release.py`。
