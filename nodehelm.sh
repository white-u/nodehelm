#!/bin/sh
# NodeHelm hub entry point: installs on a fresh host, updates an installed one,
# and runs the usual maintenance.
#   curl -fsSL https://github.com/white-u/nodehelm/releases/latest/download/nodehelm.sh | sudo sh
#   ... | sudo sh -s -- --site https://hub.example.com --yes
#   ... | sudo sh -s -- status
# Prompts read the terminal, so a piped script still asks; without one, pass
# the options. The work itself stays in the package's install-hub.sh,
# update-hub.sh and hub-lifecycle.py.
set -eu

REPO=white-u/nodehelm
CODE=/opt/node-manager-hub
DATA=/var/lib/node-manager-hub
UNIT=node-manager-hub
PLAN=/opt/.node-manager-hub-install.json
SITE='' PORT='' YES='' PACKAGE='' VERSION='' EXTRA='' KEEP='' ACTION=''

usage() {
	cat >&2 <<'EOF'
用法: nodehelm-hub [命令] [选项]（安装前用 nodehelm.sh）

命令（省略时：未安装则安装，已安装则显示菜单；无终端时直接更新）
  install          安装后台
  update           更新到最新版本，失败自动回退
  status           运行状态与版本
  logs             最近日志
  reset-password   重置应急密码，所有会话退出
  backup           一致性备份（日常备份建议用后台「数据」页下载）
  recover          完成中断的更新或卸载
  uninstall        卸载并删除全部数据；加 --keep-data 则保留数据以便重装

选项
  --site https://域名   后台 HTTPS 地址（安装）
  --port 端口           本机监听端口，默认 9911（安装）
  --version vX.Y.Z      指定版本，默认最新
  --package 文件        使用本地发布包（离线）
  --reuse-data          复用卸载时保留的数据（安装）
  --keep-data           卸载时保留数据库、密钥和主题（卸载）
  --yes                 不询问，按参数和默认值执行
EOF
	exit 2
}
die() { echo "NodeHelm: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
	case $1 in
	install | update | status | logs | reset-password | backup | recover | uninstall)
		[ -z "$ACTION" ] || usage
		ACTION=$1; shift ;;
	--site | --port | --version | --package)
		[ $# -ge 2 ] || usage
		case $1 in
		--site) SITE=$2 ;;
		--port) PORT=$2 ;;
		--version) VERSION=$2 ;;
		--package) PACKAGE=$2 ;;
		esac
		shift 2 ;;
	--yes | -y) YES=1; shift ;;
	--reuse-data) EXTRA=$1; shift ;;
	--keep-data) KEEP=1; shift ;;
	-h | --help) usage ;;
	*) echo "未知参数: $1" >&2; usage ;;
	esac
done

[ "$(id -u)" = 0 ] || die "请以 root 运行（sudo sh）"
[ -d /run/systemd/system ] || die "需要 systemd"
for c in python3 curl sha256sum tar; do
	command -v "$c" >/dev/null 2>&1 || die "缺少 $c，请先安装"
done
case $(uname -m) in
x86_64 | amd64) ARCH=x86_64 ;;
aarch64 | arm64) ARCH=aarch64 ;;
*) die "不支持的架构 $(uname -m)" ;;
esac

# A terminal to ask on; --yes or none at all means every answer comes from options.
interactive() { [ -z "$YES" ] && (exec </dev/tty) 2>/dev/null; }
ask() {
	printf '%s' "$1" >/dev/tty
	read -r REPLY </dev/tty || REPLY=''
	[ -n "$REPLY" ] || REPLY=$2
}
confirm() {
	[ -z "$YES" ] || return 0
	interactive || die "无终端时请加 --yes 确认"
	ask "$1 [Y/n] " y
	case $REPLY in [Yy]*) ;; *) die "已取消" ;; esac
}

# Installed = a running installation's unit and program. A kept-data uninstall
# leaves only the ownership marker, which is a fresh install with --reuse-data.
installed() { [ -f "/etc/systemd/system/$UNIT.service" ] && [ -f "$CODE/monitor-hub" ] && [ ! -e "$PLAN" ]; }
need_installed() { installed || die "本机未安装 NodeHelm 后台"; }
listen() {
	systemctl show -p ExecStart "$UNIT" | sed -n 's/.*--listen \([^ ;]*\).*/\1/p' | head -n 1
}
site() {
	systemctl show -p ExecStart "$UNIT" | sed -n 's/.*--site \([^ ;]*\).*/\1/p' | head -n 1
}

latest_tag() {
	[ -z "$VERSION" ] || { echo "$VERSION"; return; }
	tag=$(curl -fsSIL --proto '=https' -o /dev/null -w '%{url_effective}' \
		"https://github.com/$REPO/releases/latest") || return 1
	echo "${tag##*/}"
}

# Whether the installed hub is release $1, read from that release's checksum
# list alone: 0 yes, 1 no, 2 the list could not be read.
is_release() {
	SUMS=$(curl -fsSL --proto '=https' -m 20 "https://github.com/$REPO/releases/download/$1/sha256sums.txt" 2>/dev/null) || return 2
	echo "$SUMS" | grep -q "^$(sha256sum "$CODE/monitor-hub" | cut -d' ' -f1)  monitor-hub-$ARCH-unknown-linux-musl\$"
}

# Fetches and verifies the release package into $PKG.
fetch() {
	WORK=$(mktemp -d)
	trap 'rm -rf "$WORK"' EXIT
	trap 'exit 130' INT TERM
	NAME=node-manager-$ARCH-unknown-linux-musl.tar.gz
	if [ -n "$PACKAGE" ]; then
		# Offline: only the checksums inside the package, which catch a damaged copy.
		cp -- "$PACKAGE" "$WORK/$NAME"
		TAG=$(basename -- "$PACKAGE")
	else
		TAG=$(latest_tag) || die "无法访问 GitHub；可下载发布包后用 --package"
		URL=https://github.com/$REPO/releases/download/$TAG
		echo "下载 NodeHelm $TAG（$ARCH）"
		# A progress bar on a terminal; quiet in logs and unattended runs.
		if [ -t 2 ]; then PROGRESS=--progress-bar; else PROGRESS=--silent; fi
		curl "$PROGRESS" -fSL --proto '=https' -o "$WORK/$NAME" "$URL/$NAME" || die "下载发布包失败"
		curl -fsSL --proto '=https' -o "$WORK/sha256sums.txt" "$URL/sha256sums.txt" || die "下载校验文件失败"
		(cd "$WORK" && grep "  $NAME\$" sha256sums.txt | sha256sum --check --quiet) || die "发布包校验失败"
	fi
	tar -xzf "$WORK/$NAME" -C "$WORK"
	PKG=$WORK/node-manager
	(cd "$PKG" && sha256sum --check --quiet sha256sums.txt) || die "包内文件校验失败"
}
# The installed version's own maintenance tools, so backup, removal and recovery
# need neither a download nor GitHub. Older installs fetch them instead.
TOOLS=$CODE/tools
lifecycle() {
	if [ -f "$TOOLS/hub-lifecycle.py" ]; then
		python3 -B "$TOOLS/hub-lifecycle.py" "$@"
	else
		[ -n "${PKG:-}" ] || fetch
		python3 -B "$PKG/scripts/hub-lifecycle.py" "$@"
	fi
}

# Renamed into place: a running copy keeps reading the file it opened.
put() {
	cp "$1" "$2.new"
	chmod "$3" "$2.new"
	mv -f "$2.new" "$2"
}

# `sudo nodehelm-hub` reopens this menu. Its files live beside the program, and
# hub-lifecycle.py removes them and the link with the installation.
COMMAND_LINK=/usr/local/sbin/nodehelm-hub
place_command() {
	[ -f "$PKG/nodehelm.sh" ] || return 0
	mkdir -p "$TOOLS"
	chmod 755 "$TOOLS"
	put "$PKG/scripts/hub-lifecycle.py" "$TOOLS/hub-lifecycle.py" 644
	put "$PKG/scripts/hub_health.py" "$TOOLS/hub_health.py" 644
	put "$PKG/nodehelm.sh" "$CODE/nodehelm-hub" 755
	[ -d "${COMMAND_LINK%/*}" ] || return 0
	if [ -e "$COMMAND_LINK" ] || [ -L "$COMMAND_LINK" ]; then
		[ "$(readlink "$COMMAND_LINK" || true)" = "$CODE/nodehelm-hub" ] ||
			{ echo "$COMMAND_LINK 已被其他程序占用，未安装 nodehelm-hub 命令"; return 0; }
	fi
	ln -sfn "$CODE/nodehelm-hub" "$COMMAND_LINK"
}

proxy_notes() {
	cat <<EOF

下一步：在反向代理把 $1 转发到 http://$2，并确认：
  1. 支持 WebSocket（Upgrade/Connection 头），读写超时不低于 150 秒，关闭缓冲与缓存
  2. 请求体上限不低于 8 MiB（备份导入、主题上传；Nginx 默认 1 MB 会返回 413）
  3. 设置 X-Forwarded-Proto: https，并用真实对端地址覆盖 X-Forwarded-For
  4. 放行 GET/POST/PUT/DELETE，不记录 Authorization、Cookie 和请求体
证书须由公共 CA 签发；节点只接受有效证书。Caddy 或 Cloudflare Tunnel 可自动处理证书。
EOF
}

do_install() {
	if [ -e "$PLAN" ]; then
		# An interrupted first install continues with what it recorded.
		R=$(python3 -I -c 'import json,re,sys
m=re.search(r"--listen 127\.0\.0\.1:(\d+) .* --site (\S+)",json.load(open(sys.argv[1]))["unit"])
print(m[1],m[2]) if m else sys.exit(1)' "$PLAN") || die "安装恢复记录无法识别，请按部署说明手动 --resume"
		PORT=${R%% *} SITE=${R#* } EXTRA=--resume
		echo "继续上次中断的安装：$SITE，端口 $PORT"
	elif [ -z "$EXTRA" ] && { [ -e "$DATA" ] || [ -L "$DATA" ]; }; then
		interactive || die "发现保留的后台数据；复用请加 --reuse-data"
		ask "发现之前卸载时保留的数据，是否复用？[Y/n] " y
		case $REPLY in [Yy]*) EXTRA=--reuse-data ;; *) die "已取消；数据保留在 $DATA" ;; esac
	fi
	# Every question comes before the download, so a missing option fails at once.
	while [ -z "$SITE" ]; do
		interactive || die "请用 --site 指定后台 HTTPS 地址"
		ask "后台 HTTPS 地址（如 https://hub.example.com）: " ''
		case $REPLY in https://*) SITE=$REPLY ;; *) echo "需以 https:// 开头" >/dev/tty ;; esac
	done
	if [ -z "$PORT" ]; then
		PORT=9911
		if interactive; then ask "本机监听端口 [9911]: " 9911; PORT=$REPLY; fi
	fi
	[ -n "$YES" ] || interactive || die "无终端时请加 --yes 确认"

	fetch
	confirm "将安装 NodeHelm $TAG：$SITE，本机端口 $PORT。继续？"
	# The installer prints the only valid password; it stays in the private
	# download workspace, which the exit trap removes, until shown once below.
	OUT=$WORK/install.out
	if ! sh "$PKG/install-hub.sh" --binary "$PKG/monitor-hub" \
		--sha256 "$(sha256sum "$PKG/monitor-hub" | cut -d' ' -f1)" \
		--site "$SITE" --port "$PORT" ${EXTRA:+"$EXTRA"} >"$OUT" 2>&1; then
		cat "$OUT"; die "安装未完成，再次运行同一命令可继续"
	fi
	grep -v -e '^应急密码: ' -e '^（只显示这一次' "$OUT" || true
	PASS=$(sed -n 's/^应急密码: //p' "$OUT")
	echo
	echo "==== NodeHelm 已安装 ===="
	echo "管理入口: $SITE/admin/"
	if [ -n "$PASS" ]; then
		echo "应急密码: $PASS"
		echo "（只显示这一次，请妥善保存，勿外传；首次启动写入日志的密码已失效）"
	elif [ "$EXTRA" = --reuse-data ]; then
		echo "沿用原有登录方式。"
	fi
	place_command
	echo "以后运行 sudo nodehelm-hub 可更新、查看状态和维护；遗失应急密码时运行 sudo nodehelm-hub reset-password。"
	proxy_notes "$SITE" "127.0.0.1:$PORT"
}

do_update() {
	need_installed
	# Already current is answered from the checksum list, without the package.
	# Still downloaded when the local command or tools are missing, as after a
	# manual install-hub.sh, since this is how they are added.
	if [ -z "$PACKAGE" ] && [ -x "$CODE/nodehelm-hub" ] && [ -f "$TOOLS/hub-lifecycle.py" ] &&
		TAG=$(latest_tag) && is_release "$TAG"; then
		echo "后台已是 $TAG，无需更新。"
		return
	fi
	fetch
	if cmp -s "$PKG/monitor-hub" "$CODE/monitor-hub"; then
		echo "后台已是 $TAG，无需更新。"
		place_command
		return
	fi
	confirm "将后台更新到 $TAG，节点与业务不受影响，失败自动回退。继续？"
	sh "$PKG/update-hub.sh"
	place_command
}

do_status() {
	need_installed
	L=$(listen)
	echo "服务: $(systemctl is-active "$UNIT" || true)"
	echo "地址: $(site)/admin/（本机 $L）"
	echo "本机检查: HTTP $(curl -s -o /dev/null -m 5 -w '%{http_code}' "http://$L/api/me" || true)"
	if ! TAG=$(latest_tag 2>/dev/null); then
		echo "版本: 无法访问 GitHub，未检查"
	elif is_release "$TAG"; then
		echo "版本: 已是最新 $TAG"
	elif [ $? = 2 ]; then
		echo "版本: 无法访问 GitHub，未检查"
	else
		echo "版本: 有新版本 $TAG，运行 sudo nodehelm-hub update 更新"
	fi
}

do_logs() {
	need_installed
	# The first start logs the emergency password; it stays out of a shared screen.
	journalctl -u "$UNIT" -n 100 --no-pager | sed 's/Emergency password: .*/Emergency password: <已隐藏>/'
}

do_reset() {
	need_installed
	# The hub keeps the database's WAL files open and owned by its own user.
	[ "$(systemctl is-active "$UNIT" || true)" = active ] || die "请先启动后台再重置"
	confirm "重置应急密码，所有已登录会话将退出。继续？"
	"$CODE/monitor-hub" --db "$DATA/hub.db" --reset-password
	echo "新密码只显示这一次，请妥善保存，勿外传。"
}

do_recover() {
	[ -f "/etc/systemd/system/$UNIT.service" ] || [ -d "$CODE" ] || die "本机未安装 NodeHelm 后台"
	lifecycle recover
}

do_backup() {
	need_installed
	lifecycle backup
}

# Nodes still bound keep a component that can no longer reach this hub.
bound_nodes() {
	python3 -I -c 'import sqlite3,sys
try:
    db=sqlite3.connect("file:"+sys.argv[1]+"?mode=ro",uri=True)
    print(db.execute("SELECT count(*) FROM managed_node WHERE credential!=\x27\x27").fetchone()[0])
except sqlite3.Error:
    print(0)' "$DATA/hub.db" 2>/dev/null || echo 0
}

do_uninstall() {
	need_installed
	N=$(bound_nodes)
	if [ -n "$KEEP" ]; then
		[ "$N" = 0 ] || echo "还有 $N 个节点接入服务管理，用保留的数据重装后会自动重连。"
		confirm "将卸载后台，数据保留在 $DATA，之后可用 --reuse-data 重装。继续？"
		lifecycle uninstall --keep-data
	else
		if [ "$N" != 0 ]; then
			echo "注意：还有 $N 个节点接入服务管理。请先在后台对它们「全部卸载此节点」，否则节点上的组件将无法再连接，只能在节点本机兜底清理。"
			# Unattended runs cannot weigh that; a person at the terminal still can.
			[ -z "$YES" ] || die "有节点仍接入时不接受 --yes，请先卸载这些节点，或在终端中交互确认"
		fi
		if [ -z "$YES" ]; then
			interactive || die "无终端时请加 --yes 确认"
			echo "将卸载后台并删除全部数据：数据库、密钥、主题、维护记录和服务器上的备份。" >/dev/tty
			echo "如需留存，请先在后台「数据」页下载备份。" >/dev/tty
			ask "输入 uninstall 确认: " ''
			[ "$REPLY" = uninstall ] || die "已取消"
		fi
		lifecycle uninstall --purge
	fi
	echo "反向代理配置与证书由你维护，请按需自行移除。"
}

menu() {
	cat >/dev/tty <<'EOF'
NodeHelm 后台已安装：
  1) 更新到最新版本
  2) 运行状态与版本
  3) 最近日志
  4) 重置应急密码
  5) 备份
  6) 完成中断的更新或卸载
  7) 卸载并删除全部数据
  8) 卸载但保留数据（以便重装）
  0) 退出
EOF
	ask "请选择 [1]: " 1
	case $REPLY in
	1) ACTION=update ;;
	2) ACTION=status ;;
	3) ACTION=logs ;;
	4) ACTION=reset-password ;;
	5) ACTION=backup ;;
	6) ACTION=recover ;;
	7) ACTION=uninstall ;;
	8) ACTION=uninstall KEEP=1 ;;
	0) exit 0 ;;
	*) die "无效选择" ;;
	esac
}

if [ -z "$ACTION" ]; then
	if ! installed; then
		ACTION=install
	elif interactive; then
		menu
	elif [ -n "$YES" ]; then
		# The install one-liner rerun unattended, as the update page offers it.
		ACTION=update
	else
		ACTION=status
	fi
fi
case $ACTION in
install)
	! installed || die "后台已安装；更新请用 update"
	do_install ;;
update) do_update ;;
status) do_status ;;
logs) do_logs ;;
reset-password) do_reset ;;
backup) do_backup ;;
recover) do_recover ;;
uninstall) do_uninstall ;;
esac
