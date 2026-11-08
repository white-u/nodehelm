# 快速开始

[文档索引](INDEX.md) · [完整部署文档](DEPLOY.md) · [运维速查](OPERATIONS.md)

四步从零跑起来：装后台、配反向代理、进面板、接入节点。下文域名统一写作 `hub.example.com`，换成你自己的。参数、目录和排错见[完整部署文档](DEPLOY.md)。

## 1. 装后台

要求 Linux + systemd（已验证 Debian 13 amd64），root 权限，已装 `python3`（3.10+）、`curl`、`sha256sum`、`tar`，并有一个公共 CA 证书的 HTTPS 域名。

```sh
curl -fsSL https://github.com/white-u/nodehelm/releases/latest/download/nodehelm.sh | sudo sh
```

以 root 登录且没有 `sudo` 时，去掉命令里的 `sudo`。

脚本只问两件事：后台 HTTPS 地址（填 `https://hub.example.com`）和本机端口（回车用默认 9911）。下载有进度条，发布包校验通过后才安装。

装完会显示一次**应急密码**，请立即保存，它不会留在日志里。之后在这台机器上运行 `sudo nodehelm-hub` 可打开更新与维护菜单。

后台只监听 `127.0.0.1:9911`，这时还打不开面板，下一步配反向代理。

## 2. 配反向代理

用 Caddy 时，加一段站点配置即可，证书、WebSocket 和转发头都由 Caddy 自动处理：

```
hub.example.com {
    reverse_proxy 127.0.0.1:9911
}
```

用 Nginx、宝塔或 Cloudflare Tunnel 时，需满足五项要求：WebSocket 转发且超时不低于 150 秒、请求体上限不低于 8 MiB、正确的 `X-Forwarded-Proto` 和 `X-Forwarded-For` 等，详见[部署文档第 3 节](DEPLOY.md#3-配置-https-反向代理)。

节点只接受有效的公共证书。浏览器能打开 `https://hub.example.com/admin/`，就说明链路已通。

## 3. 进面板

1. 打开 `https://hub.example.com/admin/`，用应急密码登录。
2. 进入「服务管理」，点顶部「启用服务管理」，只需一次。
3. 可选：在设置中配置 GitHub 登录和通知渠道。应急密码保留作备用入口；遗失时在后台主机运行 `sudo nodehelm-hub reset-password` 重置。

## 4. 接入节点

每台 VPS 需要两条命令，都由后台生成，在 VPS 上以 root 执行：

1. **监控**：「节点」页点「添加节点」，填名称，复制安装命令到 VPS 执行，几秒内节点上线。
2. **服务管理**：「服务管理」→ 选中该节点 →「节点管理」→「安装并接入此节点」，复制一行命令到 VPS 执行：

   ```sh
   curl -fsS 'https://hub.example.com/management-files/enroll.sh' | sh -s -- <节点编号> <登记码>
   ```

   节点编号和登记码由页面自动填好，直接复制即可，不要手改。脚本先校验组件安装器再执行；登记码一次性使用，10 分钟内有效，过期后重新生成即可。

   命令只能在对应的那台机器上执行：后台会核对本机与该节点监控 Agent 所在的机器是否一致，不一致时拒绝并说明命令属于哪个节点，不会绑定。成功时显示「已接入为节点「名称」」，请核对名称。

3. 命令返回成功后点「刷新此节点」，确认管理在线。

接入后，在「服务管理」中按需安装 Snell、SB、FW 核心，新增实例，或启用 PM 统计流量与接入 IP（PM 需节点预装 `nftables`）。节点上多出 `nodehelm` 命令，用于升级组件、查看状态和日志。

多台机器可用「节点」页的「批量添加」，一条命令发给所有机器执行。

---

下一步：[完整部署文档](DEPLOY.md)（更新、卸载、常见问题） · [部署与恢复说明](NODE_MANAGEMENT_DEPLOY.md)（各模块细节）
