# NodeHelm 部署文档

[文档索引](INDEX.md) · [部署与恢复说明](NODE_MANAGEMENT_DEPLOY.md) · [运维速查](OPERATIONS.md)

本文按顺序给出从零部署 NodeHelm 的步骤。下文域名统一写作 `https://hub.example.com`，请换成你自己的。

## 1. 部署前准备

| 项目 | 要求 |
| --- | --- |
| 后台主机 | Linux + systemd，root；已验证 Debian 13 amd64 |
| 架构 | x86_64 已实机验证；aarch64 有构建产物，先在测试机试用 |
| 软件 | `python3`（3.10+）、`curl`、`sha256sum`、`tar` |
| 域名与证书 | 一个 HTTPS 域名，证书由公共 CA 签发；不支持 IP 或明文 HTTP 接入节点 |
| 节点 VPS | Linux + systemd，root，Python 3.10+，能用 HTTPS 访问后台域名 |
| 节点 PM 模块 | 另需 `nftables`（Debian：`apt-get install nftables`） |

没有证书时，可用 Cloudflare Tunnel 或 Caddy 自动处理，见第 3 节。只想先看界面，可用 SSH 隧道 `ssh -L 9911:127.0.0.1:9911 root@服务器` 后打开 `http://127.0.0.1:9911/admin/`，但节点无法这样接入。

## 2. 安装后台

在后台主机以 root 运行：

```sh
curl -fsSL https://github.com/white-u/nodehelm/releases/latest/download/nodehelm.sh | sudo sh
```

以 root 登录且系统没有 `sudo` 时（常见于精简版 Debian），把本文命令中的 `sudo` 去掉即可，例如 `curl -fsSL … | sh`。

脚本识别架构，下载并校验最新发布包，只问两件事：后台 HTTPS 地址和本机端口（回车用默认 9911）。装完显示管理入口、应急密码（只显示这一次，不留在日志里）和反向代理要求。

不想交互时，用参数直接运行：

```sh
curl -fsSL https://github.com/white-u/nodehelm/releases/latest/download/nodehelm.sh | sudo sh -s -- --site https://hub.example.com --yes
```

| 参数 | 作用 |
| --- | --- |
| `--site` | 对外 HTTPS 地址，节点和登录回调都用它 |
| `--port` | 本机监听端口，默认 9911 |
| `--yes` | 不询问，按参数和默认值执行；无终端时必须提供 |
| `--version vX.Y.Z` | 指定版本，默认最新 |
| `--package 文件` | 使用已下载的本地发布包（离线） |
| `--reuse-data` | 复用之前卸载时保留的数据和主题 |

首次安装中断时，再次运行同一命令会按原记录继续。发现之前卸载时保留的数据，脚本会询问是否复用。

| 项目 | 路径或值 |
| --- | --- |
| 服务 | `node-manager-hub.service`（非 root 动态用户运行） |
| 程序 | `/opt/node-manager-hub/monitor-hub` |
| 数据库 | `/var/lib/node-manager-hub/hub.db` |
| 监听 | `127.0.0.1:9911`，只对本机开放 |
| 管理入口 | `https://hub.example.com/admin/` |

安装后本机多出 `nodehelm-hub` 命令，以后运行 `sudo nodehelm-hub` 即可打开菜单进行更新和维护，卸载时一并删除。安装不会改动同机的原 monitor 服务。

### 离线或手动安装

连不上 GitHub 时，把 [GitHub Releases](https://github.com/white-u/nodehelm/releases) 中对应架构的 `node-manager-*.tar.gz` 和 `nodehelm.sh` 传到服务器，运行 `sudo sh nodehelm.sh --package node-manager-x86_64-unknown-linux-musl.tar.gz`。

也可以不用入口脚本，解压后直接调用安装器（这样不会安装 `nodehelm-hub` 命令，之后运行一次 `nodehelm.sh update` 即可补上），`--binary` 必须是绝对路径：

```sh
tar xzf node-manager-$(uname -m)-unknown-linux-musl.tar.gz
cd node-manager
sha256sum --check sha256sums.txt
sudo sh install-hub.sh --binary "$PWD/monitor-hub" \
  --sha256 "$(sha256sum monitor-hub | cut -d' ' -f1)" \
  --site https://hub.example.com
```

在线下载时脚本按发布的 `sha256sums.txt` 校验发布包，解压后再校验包内文件。校验文件与程序来自同一发布，只能发现下载损坏；需要防篡改时，另行核对发布来源。

## 3. 配置 HTTPS 反向代理

后台只监听本机，由你自己的反向代理提供 HTTPS，把 `https://hub.example.com` 转发到 `http://127.0.0.1:9911`。无论用哪种代理，都要满足下面五项：

| 要求 | 不满足时的现象 |
| --- | --- |
| 转发 WebSocket（`Upgrade`、`Connection` 头），读写超时不低于 150 秒，关闭缓冲与缓存 | 实时数据不刷新、节点周期性掉线，握手返回 400 |
| 请求体上限不低于 8 MiB | 备份导入、主题上传返回 413（Nginx 默认只有 1 MB） |
| 设置 `X-Forwarded-Proto: https`，并用真实对端地址覆盖 `X-Forwarded-For`，不能透传用户自填的值 | 登录 Cookie 缺少安全标记；登录限流把所有访客算作同一来源 |
| 放行 GET、POST、PUT、DELETE | 后台只能读不能写 |
| 不记录 Authorization、Cookie 和请求体，不缓存 `/api/` | 凭据泄露到日志，或页面数据错乱 |

常见选择：

- **Caddy**：自动申请证书，WebSocket 和转发头默认正确，配置最简单。
- **Nginx / 宝塔**：需要自行补齐上表各项，特别是请求体上限和 `X-Forwarded-Proto`。
- **Cloudflare Tunnel**：不用开放入站端口，也不用自备证书，域名须托管在 Cloudflare。启用 WAF 或 Access 时，节点访问的 `/api/management-agent/`、`/api/agent/`、`/agent/`、`/management-files/` 和 `/install.sh` 须放行；SSL/TLS 使用 Full (strict)。节点请求的客户端标识是 `NodeManager/1`，若被拦截返回 403，在安全事件里放行。

节点只接受公共 CA 签发的有效证书，不支持跳过校验。配好后，浏览器能打开 `https://hub.example.com/admin/` 即表示链路已通。

## 4. 首次登录与启用服务管理

1. 打开 `https://hub.example.com/admin/`，用安装时显示的应急密码登录。
2. 在 GitHub 创建 OAuth App，回调地址填 `https://hub.example.com/api/auth/github/callback`。
3. 在后台设置中填入 Client ID、Client Secret 和允许登录的 GitHub 账号，退出后验证 GitHub 登录。
4. 进入「服务管理」，点击顶部「启用服务管理」，只需一次。

应急入口保留，不要删除；密码不要贴到聊天或工单里。遗失时在后台主机重置，新密码只显示在当前终端，所有已登录会话退出：

```sh
sudo nodehelm-hub reset-password
```

## 5. 接入节点

每台 VPS 先装监控 Agent 成为节点，再装服务管理组件。两条命令都由后台生成，在 VPS 上以 root 执行。

1. 后台添加节点，复制监控 Agent 安装命令到 VPS 执行，等待节点在线。
2. 「服务管理」→ 选择该节点 →「管理」页 →「安装并接入此节点」，复制命令到 VPS 执行。
3. 命令返回成功后，点「刷新此节点」，确认管理在线。
4. 按需安装 Snell、SB、FW 核心或启用 PM，再新增实例或规则。

多台机器可用节点页的「批量添加」：同一条命令在窗口期内发给所有机器执行（例如 `for h in node1 node2; do ssh root@$h '命令'; done`），每台自动换取自己的令牌。

接入命令含一次性登记凭据，10 分钟内有效，过期后在同一节点重新生成即可。组件安装在 `/opt/node-manager`，服务名 `node-manager-agent`；节点主动通过 HTTPS 轮询后台，不开放入站端口。

接入后节点上多出 `nodehelm` 命令。`sudo nodehelm` 打开节点菜单：升级管理组件和监控 Agent、运行状态、最近日志、恢复中断的操作。接入、配置和卸载仍在后台网页完成。

## 6. 更新与回退

所有更新都由人在机器上执行命令，后台不会向节点推送程序。先更新后台，再更新节点组件；业务核心在页面内单独升级。更新后台或组件不会重启节点上的业务实例。

| 层 | 更新方式 | 失败时 |
| --- | --- | --- |
| 后台 | 后台主机上 `sudo nodehelm-hub update` | 自动回退到原程序 |
| 节点管理组件和监控 Agent | 节点上 `sudo nodehelm upgrade` | 组件自动回退；Agent 保留原版本 |

监控 Agent 随后台发布并嵌入后台，节点从后台的 `/agent/{arch}` 下载，不访问 GitHub；后台更新后，「更新」页的 agent 一栏列出需要升级的节点。未接入服务管理的节点没有 `nodehelm` 命令，用该栏给出的升级命令。
| 业务核心 | 服务管理 → 模块「管理」页 → 升级核心 | 自动恢复原核心；页面可显式回退 |

更新后台：先核对发布的校验清单，已是最新会直接退出，不下载发布包；否则先做一致性备份，再切换并检查新程序。还没有 `nodehelm-hub` 命令时，用安装时的同一条命令，已安装时它显示菜单。后台「更新」页提供命令的复制按钮。

更新节点：后台「更新」页的「管理组件」一栏列出组件与后台不一致的节点，在这些节点上执行 `sudo nodehelm upgrade`。1.0.0 接入的节点还没有这个命令，第一次用下面的命令升级，之后就有了：

```sh
sudo python3 /opt/node-manager/install.py upgrade
```

多台节点可在源码仓库中用 `scripts/upgrade-nodes.sh root@node1 root@node2` 逐台 SSH 执行，遇到失败即停。组件回退使用升级输出中的备份编号：

```sh
sudo python3 /opt/node-manager/install.py rollback --backup <备份编号>
```

后台与节点协议版本不一致时，节点仍显示在线，但页面提示不兼容并暂停下发操作；升级该节点组件即可恢复。

## 7. 后台维护与卸载

在后台主机运行 `sudo nodehelm-hub` 打开菜单，也可以直接带命令：

| 命令 | 作用 |
| --- | --- |
| `update` | 更新到最新版本 |
| `status` | 服务状态、本机检查和是否有新版本 |
| `logs` | 最近 100 行日志，应急密码会被隐藏 |
| `reset-password` | 重置应急密码，所有会话退出 |
| `backup` | 一致性备份，保存在 `/var/backups/node-manager-hub` |
| `recover` | 完成中断的更新或卸载，不需要联网 |
| `uninstall` | 卸载并删除全部数据：数据库、密钥、主题、维护记录、服务器上的备份和本命令；需输入 `uninstall` 确认 |
| `uninstall --keep-data` | 卸载但保留数据，之后可用 `--reuse-data` 重装 |

日常备份建议用后台「数据」页下载，文件离开服务器才算灾备。网页备份不含服务管理密钥，恢复后节点需要重新接入。

从服务器上的备份恢复：

```sh
sudo python3 -B /opt/node-manager-hub/tools/hub-lifecycle.py restore --backup /var/backups/node-manager-hub/<备份目录>
```

恢复会替换当前数据，先做一次现状备份。中断的更新或卸载只用 `recover` 处理，不要手工删除恢复记录。维护工具随本机安装的版本保存在 `/opt/node-manager-hub/tools`，备份、恢复和卸载都不需要访问 GitHub。

卸载顺序：先卸载全部节点（见下一节），再卸载后台。仍有节点接入时，`uninstall` 会提示数量，并拒绝无人值守的 `--yes`。以下内容不在卸载范围内，需要时自行处理：反向代理配置与证书、你下载到本地的网页备份、同机的原 monitor 服务。

## 8. 节点卸载

| 方式 | 在哪操作 | 清理范围 |
| --- | --- | --- |
| 模块卸载 | 服务管理 → 节点 → 模块「管理」页 | 该模块全部实例、核心和关联 PM 规则 |
| 全部卸载此节点 | 服务管理 → 节点 →「管理」页 | 全部受管实例、核心、PM 规则与运行时、管理组件和 `nodehelm` 命令；进度保存在后台，中断可继续 |
| 紧急撤销授权 | 服务管理 → 节点 | 只撤管理凭据，业务服务继续运行 |
| 卸载监控 Agent | 节点上执行后台删除节点对话框给出的命令 | 只删监控 Agent |

完整移除一台节点：先「全部卸载此节点」，再卸载监控 Agent，最后在后台删除节点。

收尾阶段卡住时查看：

```sh
sudo systemctl status node-manager-cleanup.service
sudo journalctl -u node-manager-cleanup.service -n 50 --no-pager
```

后台已经无法使用时，节点上只能停止受管理，不能清理业务实例：先运行 `sudo python3 /opt/node-manager/agent.py unlink` 解绑，再运行 `sudo python3 /opt/node-manager/install.py uninstall` 移除轮询服务，业务实例继续运行。要彻底清理，需先恢复后台（用 `--reuse-data` 重装或从备份恢复），再在网页里「全部卸载此节点」。

## 9. 后台机器同时作为节点

后台主机也可以装监控 Agent 和服务管理组件，按第 5 节正常接入即可。三者的目录、服务和本机命令互不重叠：后台用 `nodehelm-hub`，节点用 `nodehelm`。

- 业务端口不能与后台本机端口（默认 9911）和反向代理的 80/443 相同，新建实例时会提示“监听端口已被占用”。
- 本机组件经公网域名连回后台。个别云厂商不支持访问自己的公网 IP，此时用 Cloudflare 代理或 Tunnel。
- 更新后台不会重启本机的业务实例和 Agent。
- 卸载时先在网页「全部卸载此节点」，再卸载后台；顺序反了，本机组件会停在收尾阶段，需要先恢复后台。

## 10. 常见问题

| 现象 | 原因与处理 |
| --- | --- |
| 打不开 `/admin/` | 运行 `sudo nodehelm-hub status`；确认代理指向本机端口、证书有效 |
| 实时数据不刷新，节点周期性掉线 | 代理未转发 WebSocket 或超时太短，见第 3 节 |
| 备份导入、主题上传返回 413 | 代理请求体上限不足 8 MiB |
| 「添加节点」不可用 | 用 IP 或明文 HTTP 打开了后台；改用 HTTPS 域名访问 |
| 节点日志提示证书无效 | 证书不是公共 CA 签发或已过期；节点不信任自签证书 |
| 节点安装时 403 | CDN 或 WAF 拦截了 `NodeManager/1` 请求，检查安全事件与放行规则 |
| 管理离线 | 节点上 `sudo nodehelm status` 和 `sudo nodehelm logs`，再检查 VPS 到后台的 HTTPS |
| 登记凭据过期 | 在同一节点重新生成安装命令，原待登记身份保留 |
| 页面提示组件不兼容 | 在该节点运行 `sudo nodehelm upgrade` |
| 卸载已暂停 | 先看日志处理原因，再点「继续卸载」 |
| PM 提示受管表缺失 | 核实是否被 `flush ruleset`，再执行 `python3 /opt/node-manager/pm.py repair --confirm-checkpoint-loss` |
| 后台更新中断 | `sudo nodehelm-hub recover` |
| 节点操作中断 | 节点上 `sudo nodehelm recover` |

不要通过关闭证书校验绕过问题。更细的恢复流程见 [部署与恢复说明](NODE_MANAGEMENT_DEPLOY.md)。
