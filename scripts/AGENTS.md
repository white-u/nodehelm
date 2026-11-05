# 部署与工具模块

[项目说明](../AGENTS.md) · [总索引](../INDEX.md) · [部署说明](../NODE_MANAGEMENT_DEPLOY.md)

本目录包含后台本机安装维护、默认主题准备和发行包工具。VPS 管理组件自身的安装维护在 [management/install.py](../management/install.py)。

## 文件索引

| 文件 | 职责 |
| --- | --- |
| [hub-install.py](hub-install.py) | 后台首次安装、安装进度、续装与保留数据重装 |
| [hub-lifecycle.py](hub-lifecycle.py) | 后台备份、恢复、升级、失败回退及卸载 |
| [upgrade-nodes.sh](upgrade-nodes.sh) | 从操作机经 SSH 依次升级多台节点的管理组件，首个失败即停止 |
| [hub_health.py](hub_health.py) | 后台实际进程、监听与 HTTP 健康状态辅助 |
| [package-release.py](package-release.py) | 将后台二进制、安装维护工具、说明与内部摘要打为离线包 |
| [theme.sh](theme.sh) | 根据根目录 `web-theme.pin` 准备默认主题 |
| [e2e.sh](e2e.sh) | Hub 与外部监控 Agent 的端到端联调入口 |

## 相关入口

| 位置 | 关系 |
| --- | --- |
| [install-hub.sh](../install-hub.sh) | 根目录后台安装入口，调用 `hub-install.py` |
| [install.sh](../install.sh) | 外部 monitor-agent 安装入口 |
| [build.rs](../build.rs) | Cargo 构建时调用 `theme.sh` |
| [自动化工作流](../.github/workflows/) | `ci.yml` 自动化检查、`audit.yml` 依赖审计、`release.yml` 发行构建与打包 |
| [Dockerfile](../Dockerfile) | 使用构建完成的后台二进制生成容器镜像 |

`hub-install.py`、`hub-lifecycle.py` 都使用 `hub_health.py`。打包入口在 `package-release.py`，具体收录清单定义在其 `files` 中。安装目录、服务单元和状态参数以对应工具中的定义为准；使用说明统一维护在 [NODE_MANAGEMENT_DEPLOY.md](../NODE_MANAGEMENT_DEPLOY.md)。
