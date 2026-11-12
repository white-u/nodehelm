# 开发与验证

[文档索引](INDEX.md)

## 构建顺序

在仓库根目录操作。需要 Rust stable / Cargo、Node.js 24（与 CI 一致，最低 22.18）、npm，以及 `sh`、curl、tar。SQLite 以 bundled 方式构建，需要可用的本地编译工具链。

```sh
(cd web-admin && npm ci && npm run build)
(cd web-theme && npm ci && npm run build)
cargo build --locked
```

默认主题源码在 [web-theme/](web-theme/README.md)，与后台同仓库维护、随后台发布。Rust 的 `rust-embed` 读取 `web-admin/dist/` 和 `web-theme/dist/`，`theme.json` 与 `preview.png` 直接从 `web-theme/` 嵌入。只运行 `cargo build` 不会构建这两个前端，缺少 dist 时构建不完整；不要通过空目录绕过错误。

构建会访问依赖仓库，网络或包下载失败造成的构建失败不能当作成功。

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

打开 Vite 输出地址下的 `/admin/`；[vite.config.ts](web-admin/vite.config.ts) 将 `/api`（含 WebSocket）和图标代理到 `127.0.0.1:9911`。也可直接访问 hub 的 `/admin/` 查看已嵌入的构建版本。默认主题同样在 `web-theme/` 下 `npm run dev`，代理方式见 [web-theme/README.md](web-theme/README.md)。纯 HTTP 本地入口可做界面和已有数据联调，但添加/安装节点要求 HTTPS 域名入口；真实 agent 联调应按 [部署文档](DEPLOY.md) 配置可访问的域名。

## 检查与测试

以下顺序与 [CI](.github/workflows/ci.yml) 一致，前提是两个前端的依赖已安装：

```sh
(cd web-admin && npm run lint && npm test && npm run build)
(cd web-theme && npm run lint && npm test && npm run build)
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
python3 -m unittest discover -s management/tests
sudo unshare -n python3 management/tests/integration_pm.py   # Linux，需要 nftables
cargo test
(cd agent && cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked)
shellcheck -S warning install.sh install-hub.sh update-hub.sh nodehelm.sh scripts/*.sh
cargo build
python3 management/tests/integration_http.py target/debug/monitor-hub
for t in management management-lifecycle admin; do node web-admin/tests/$t.browser.mjs target/debug/monitor-hub; done
(cd agent && cargo build --release --locked)
scripts/e2e.sh target/debug/monitor-hub agent/target/release/monitor-agent
```

`integration_http.py` 用真实后台和管理组件走一遍登录、绑定、上报与回执；三个 `*.browser.mjs` 用 Playwright 在临时后台上走管理界面流程（Playwright 由 `PLAYWRIGHT_PACKAGE` 指向其 `package.json`，CI 在临时目录安装，不进入 `web-admin` 依赖；修改服务页面或管理接口时必跑）；`e2e.sh` 用后台与监控 agent 在回环上核对节点上报和公开字段。

`web-admin` 的 `npm test` 使用 Node 直接运行 `api.test.ts` 和服务表单 `model.test.ts`，`web-theme` 的运行格式化与实时指标测试，应使用 CI 同代的 Node（22.18 以下不能直接运行 `.ts`）。它验证请求和辅助数据逻辑，并非浏览器端到端覆盖。Rust 用例主要放在各模块内，分时流量数据库测试通过 `db/mod.rs` 引入 `src/testdata/traffic-period.rs`。

| 修改范围 | 必要关联检查 |
| --- | --- |
| 流量基线或分时表 | `cargo test traffic_period`，并检查 `db/tests.rs` 的累计、恢复、裁剪测试 |
| API / 身份 / 公开字段 | 对应 Rust 模块测试、`npm test`，必要时跑 agent 集成 |
| 管理后台 | lint、test、build；布局交互仍需实际页面验证 |
| 默认主题或嵌入流程 | `web-theme` 的 lint、test、build 与版本一致，Cargo build，检查默认与磁盘主题；不得调用契约以外的接口 |
| 安装器 | ShellCheck 和语法检查；实际服务生命周期在专用 Linux 测试机验证 |

定向测试通过不代替相关集成检查，测试筛选无匹配时也不视为覆盖成功。

### CI 之外的验收

| 范围 | 运行方式 | 说明 |
| --- | --- | --- |
| 业务模块 | 在一次性 Linux 测试机以 root 运行 `python3 management/tests/integration_{snell,modules}.py` | 使用隔离的名称、路径与单元，不安装管理组件，不影响已有服务；`integration_modules` 同时核对 Reality 探测日志仍能被端口流量统计识别，升级 sing-box 核心时必跑 |
| 核心升级回退 | `unshare -n python3 management/tests/integration_core_lifecycle.py <发布归档目录>` | 同上，需要预先下载的核心归档 |

## 修改模块与协议

- **实例模板**：直接修改 `config()`／`service()`（端口流量统计为 `unit_files()`）。组件升级时按已确认状态重写变化的文件，并只重启变化且运行中的实例，无需写迁移。新增字段用 `.get(键, 默认值)` 读取，已确认状态里不会有它。
- **状态结构**：只有重命名、拆分字段这类 `.get` 解决不了的改动才需要迁移代码。
- **协议**：请求或快照的形状变化时，同步增加 `src/management/mod.rs` 与 `management/protocol.py` 的 `PROTOCOL`。不增加就要保证新旧两侧都能读懂对方。
- **组件文件**：新增文件时加入 Hub 的 `include_str!` 清单，安装器按后台发布的清单工作，无需改动；`test_transport.py` 校验清单与 `management/` 一致。
- **操作校验**：Hub 与组件分别校验操作内容，修改校验时在 `management/tests/operations.json` 补样例，两侧测试都会读取。

## 监控 agent

agent 源码在 [agent/](agent/)，独立 Cargo 包，在该目录内运行 `cargo test`、`cargo clippy --all-targets`。发布时先构建两个架构的 agent 放到 `target/agents/monitor-agent-{x86_64,aarch64}-unknown-linux-musl`，`build.rs` 将其嵌入后台并校验 ELF 架构；设置 `NODEHELM_REQUIRE_AGENTS=1` 时缺少任一文件即构建失败。普通开发构建不含 agent，`/agent/{arch}` 返回 404 并说明原因，更新页显示「此后台构建不含 agent」。

## 与 agent 做集成验证

在 Linux 上：

```sh
cargo build --locked
(cd agent && cargo build --release --locked)
scripts/e2e.sh target/debug/monitor-hub agent/target/release/monitor-agent
```

脚本需要 curl、jq，默认使用回环端口 `9951`，可通过 `E2E_PORT` 调整。它创建临时数据库、启动两个子进程、登录、添加节点，检查真实上报字段和公开/管理数据隔离，退出时结束进程并清理临时目录。脚本直接运行二进制，不安装系统服务。

CI 在 Linux 上将当前 hub 与同一提交的 agent 配对；macOS 不能直接运行该 Linux 文件。实际 agent 接入、代理 WebSocket 和安装器仍应在对应 Linux 环境验证。

## 发行与供应链

[release.yml](.github/workflows/release.yml) 先构建两个架构的 agent，再构建嵌入它们的 x86_64 / aarch64 musl 后台二进制，`v*` 标签必须匹配 Cargo 版本；发布二进制、各架构离线包 `node-manager-*.tar.gz`、入口脚本 `nodehelm.sh` 与 `sha256sums.txt`，再将同一批二进制打入容器镜像。在普通分支上手动触发只构建；发布步骤以标签 ref 为条件，手动选择标签运行也可能进入发布流程。

发布新版本：

1. 一批修改在 `main` 上完成后，修改 `Cargo.toml` 的版本，运行 `cargo update -p monitor-hub --offline` 同步 `Cargo.lock`，提交 `release: x.y.z` 并推送 `main`。
2. 等该提交的 ci 通过后，在同一提交上推送标签：`git push origin "$(git rev-parse HEAD):refs/tags/vx.y.z"`。
3. 发布页出现 `sha256sums.txt` 即完成；随后在后台主机运行 `sudo nodehelm-hub update`，节点运行 `sudo nodehelm upgrade`。

已发布的标签不移动、不复用，有问题发布下一个版本。本地交叉编译的后台只用于测试：其中的 agent 与正式发布的字节不同，换回正式版本后节点会重装一次 agent。

[audit.yml](.github/workflows/audit.yml) 检查依赖漏洞，并在定时/手动运行时核对最新发行版。修改默认主题时升高 `web-theme/theme.json` 的版本，CI 核对它与 `package.json`、`package-lock.json` 一致；默认主题只调用其 README 列出的主题契约接口。文档修改只需回读、检查本地链接与差异，无需为此下载所有依赖或发布版本。
