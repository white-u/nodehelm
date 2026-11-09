# 默认主题

[项目说明](../AGENTS.md) · [总索引](../INDEX.md) · [后端入口](../src/AGENTS.md)

NodeHelm 内置的公开状态页主题，同时是第三方主题的参考实现。派生自
[monitor-probe/monitor-theme-default](https://github.com/monitor-probe/monitor-theme-default) v1.4.2（MIT），
现随 NodeHelm 维护与发布，不再跟随上游。React + Vite + shadcn/ui，黑白配色。

后台编译时嵌入本目录的 `theme.json`、`preview.png` 与构建产物 `dist/`（见 `src/frontend.rs`），默认主题随后台版本更新；
`theme.json` 的 `url` 留空，面板因此不对它提供「从 GitHub 更新」。已安装的 `short` 为 `default` 的主题仍会顶替内置这份。

**本主题只调用下文「主题契约」列出的接口。** 上游主题站的第三方主题依赖同一份契约，改动契约就是改动兼容性，须单独说明，
不能为了默认主题顺手改。主题改动后升高 `theme.json` 的 `version`，`package.json` 与 `package-lock.json` 保持同一版本（CI 核对）。

## 开发

主题只读公开数据，开发服务器直接拿一个现成的 hub 当数据源，要求它开着公开状态页：

```bash
npm ci
# Vite 将 /api 与 WebSocket 代理至这个 hub
MONITOR_HUB=https://hub.example.com npm run dev
```

不设 `MONITOR_HUB` 时代理至 `http://127.0.0.1:9911`。上游文档站的
[主题开发](https://monitor-document.pages.dev/dev/theme)页说明了在本机起 hub、自己造节点的写法。

构建产物位于 `dist/`，后台编译前须先构建。提交前运行 `npm run build && npm run lint && npm test`。

`npm test` 校验数字格式化、坐标轴与时间刻度，以及实时指标的输入边界。没有测试框架，Node 自己剥掉
类型，失败时退出码非零。

## 主题包

一个可安装主题是一个目录，名字必须与 `theme.json` 的 `short` 相同：

```text
<themes-dir>/<short>/
├── theme.json
├── preview.png        # 可选，面板上的预览图
└── dist/
    └── index.html
```

`theme.json` 除 `config` 外的六个字段都要写，均为字符串，`description`、`version`、`author`、`url` 可以留空，少写一个 hub 就不认这个主题：

| 字段 | 含义 |
|---|---|
| `name` | 显示名称 |
| `short` | 唯一短名，限字母、数字、`-`、`_`，取 `default` 则顶替 hub 内置的那份 |
| `description` | 简介 |
| `version` | 主题版本 |
| `author` | 作者 |
| `url` | 源码地址 |
| `config` | 可选，数组，站长在后台可调的设置，后台按它画表单 |

本目录即是这样一个主题，`npm run build` 后连同 `dist/` 打包就是可安装的主题包。

将目录复制到 hub 的 `--themes` 位置，在后台「主题」页切换，无需重启。

## 主题契约

主题是纯静态 SPA，只能依赖下列同源接口：

| 接口 | 用途 |
|---|---|
| `GET /api/me` | 站点名、登录状态、公开页开关、历史保留天数 |
| `GET /api/nodes` | 节点列表、实时指标和累计流量 |
| `GET /api/nodes/{id}/metrics` | 历史指标和延迟记录 |
| `GET /api/ws` | 每 2 秒推送一次节点快照的 WebSocket；带 `?gzip` 时每帧 gzip 后以二进制消息发出 |
| `GET /api/themes/{short}/config` | 站长改过的主题设置，只含与默认值不同的项 |

主题自带设置界面时，站长登录后可以 `PUT` 同一地址保存。`config` 的声明格式、两个接口的规则与约定见文档站的[主题开发](https://monitor-document.pages.dev/dev/theme#主题设置)。

`metrics` 的三个查询参数都可省：

- `hours=N` 窗口宽度。**上限是 hub 的保留天数**（`/api/me` 的 `history_days`），登录与匿名相同，超出
  静默 clamp，所以档位要按它生成。7 天以内取分钟明细，更宽取每小时的汇总
- `points=W` 调用方画得下的点数，只会让 hub 抽得更稀，不会更密
- `series=metrics|ping` 只取要画的那一半，省掉的那半原本占响应的三分之一到三分之二

探测曲线的名字在响应的 `probes` 里随样本一起下发，匿名可读，所以画延迟图不需要第二个请求，也不
需要管理员身份。

整个窗口的丢包率在响应的 `loss` 里，按探测 id 给出百分比，没丢包的探测不出现。**不要拿样本行里
的 `loss` 自己平均**：那一个是所在桶的百分比，除数已经丢了，而各桶样本数天然不等——窗口首尾两桶
本来就是残缺的，探测启停、节点掉线、agent 跳过一轮都会再造几个。十三次里丢一次，平均桶百分比会
算出 50%。

匿名访问 `GET /api/nodes` 仅返回 `public=1` 的节点，响应中不含 `ip`、`hostname`、`remark`。字段定义以 hub 的 `src/api/nodes.rs` 为准。

未知路径回落到主题的 `dist/index.html`，客户端路由可用。`/admin/*` 由 hub 内置后台接管，不属于主题契约。

本主题用 `/node/{id}` 作为详情页。hub 的回落对它够用，但**hub 前面若有按路径做正向白名单的反代
或 WAF，得把这个前缀放行**：从列表点进去只是 pushState，边缘看不见，刷新详情页才会真的请求
`/node/{id}`，症状是「点进去正常，一刷新就被拦」。
图标 `/favicon.svg` 和 `/apple-touch-icon.png` 同样要放行，后者被拦时 iOS 收藏和添加到主屏幕都没有图标。

## 许可

MIT
