# 开发与验证

[文档索引](INDEX.md)

## 构建顺序

在仓库根目录操作。需要 Rust stable / Cargo、Node.js 24（与 CI 一致，最低 22.18）、npm，以及 `sh`、curl、tar、sha256sum。SQLite 以 bundled 方式构建，需要可用的本地编译工具链。

```sh
./scripts/theme.sh
cd web-admin
npm ci
npm run build
cd ..
cargo build --locked
```

`theme.sh` 读取 `web-theme.pin` 中的版本和 SHA256，将发行包下载到 `target/theme/`，检查布局并写入 `.pin`。`build.rs` 也会调用此脚本；显式提前运行便于尽早发现网络或校验问题。macOS 若没有 `sha256sum`，需先提供该命令，例如安装 GNU coreutils 后将其加入 PATH。

Rust 的 `rust-embed` 读取 `web-admin/dist/` 和 `target/theme/dist/`。只运行 `cargo build` 不会构建 React 后台，缺少前端 dist 时构建不完整；不要通过空目录或伪造 `.pin` 绕过错误。

构建会访问依赖仓库与默认主题发行地址。网络或包下载失败时先保留失败证据，不能把缺少资源造成的失败当成功。以上命令描述构建流程，不代表当前环境已经通过构建验证。

## 本地运行与前端联调

先启动 hub：

```sh
cargo run --locked -- --listen 127.0.0.1:9911 --db /tmp/monitor-dev/monitor.db
```

首次日志会给出应急密码。开发数据库应使用独立目录，不连接生产数据。在另一个终端：

```sh
cd web-admin
npm run dev
```

打开 Vite 输出地址下的 `/admin/`；[vite.config.ts](web-admin/vite.config.ts) 将 `/api`（含 WebSocket）和图标代理到 `127.0.0.1:9911`。也可直接访问 hub 的 `/admin/` 查看已嵌入的构建版本。纯 HTTP 本地入口可做界面和已有数据联调，但添加/安装节点要求 HTTPS 域名入口；真实 agent 联调应按 [部署说明](OPERATIONS.md) 配置可访问的域名。

## 检查与测试

以下顺序与 [CI](.github/workflows/ci.yml) 一致，前提是主题和后台资源已构建：

```sh
cd web-admin
npm run lint
npm test
npm run build
cd ..
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
shellcheck -S warning install.sh install-hub.sh update-hub.sh scripts/*.sh
```

`npm test` 使用 Node 直接运行 `api.test.ts`，应使用 CI 同代的 Node。它验证请求和辅助数据逻辑，并非浏览器端到端覆盖。Rust 用例主要放在各模块内，分时流量数据库测试通过 `db.rs` 引入 `src/testdata/traffic-period.rs`。

| 修改范围 | 必要关联检查 |
| --- | --- |
| 流量基线或分时表 | `cargo test traffic_period`，并检查 `db.rs` 的累计、恢复、裁剪测试 |
| API / 身份 / 公开字段 | 对应 Rust 模块测试、`npm test`，必要时跑 agent 集成 |
| 管理后台 | lint、test、build；布局交互仍需实际页面验证 |
| 主题或嵌入流程 | 固定主题下载校验、后台 build、Cargo build，检查默认与磁盘主题 |
| 安装器 | ShellCheck 和语法检查；实际服务生命周期在专用 Linux 测试机验证 |

定向测试通过不代替相关集成检查，测试筛选无匹配时也不视为覆盖成功。

## 修改模块与协议

- **实例模板**：直接修改 `config()`／`service()`（PM 为 `unit_files()`）。组件升级时按已确认状态重写变化的文件，并只重启变化且运行中的实例，无需写迁移。新增字段用 `.get(键, 默认值)` 读取，已确认状态里不会有它。
- **状态结构**：只有重命名、拆分字段这类 `.get` 解决不了的改动才需要迁移代码。
- **协议**：请求或快照的形状变化时，同步增加 `src/management/mod.rs` 与 `management/model.py` 的 `PROTOCOL`。不增加就要保证新旧两侧都能读懂对方。
- **组件文件**：新增文件时加入 Hub 的 `include_str!` 清单，安装器按后台发布的清单工作，无需改动；`test_transport.py` 校验清单与 `management/` 一致。
- **操作校验**：Hub 与组件分别校验操作内容，修改校验时在 `management/tests/operations.json` 补样例，两侧测试都会读取。

## 与外部 agent 做集成验证

仓库不包含 agent 二进制。准备可在当前系统运行、来源可信的 agent 后：

```sh
cargo build --locked
scripts/e2e.sh target/debug/monitor-hub /path/to/monitor-agent
```

脚本需要 curl、jq，默认使用回环端口 `9951`，可通过 `E2E_PORT` 调整。它创建临时数据库、启动两个子进程、登录、添加节点，检查真实上报字段和公开/管理数据隔离，退出时结束进程并清理临时目录。脚本直接运行二进制，不安装系统服务。

CI 在 Linux 上将当前 hub 与上游最新发行的 Linux agent 配对；macOS 不能直接运行该 Linux 文件。实际 agent 接入、代理 WebSocket 和安装器仍应在对应 Linux 环境验证。

## 发行与供应链

[release.yml](.github/workflows/release.yml) 构建 x86_64 / aarch64 musl 二进制，`v*` 标签必须匹配 Cargo 版本；发布二进制、hub 安装器与 `sha256sums.txt`，再将同一批二进制打入容器镜像。在普通分支上手动触发只构建；发布步骤以标签 ref 为条件，手动选择标签运行也可能进入发布流程。

[audit.yml](.github/workflows/audit.yml) 检查依赖漏洞，并在定时/手动运行时核对最新发行版。修改默认主题固定版本时同时核对 `web-theme.pin` 的版本与哈希；不要只改下载 URL。文档修改只需回读、检查本地链接与差异，无需为此下载所有依赖或发布版本。
