#!/bin/sh
# Upgrades the management component on each SSH host in turn, stopping at the first
# failure so a bad release reaches one node. Run it after the Hub itself is upgraded.
set -u
[ $# -gt 0 ] || { echo "用法：$0 [user@]host ..." >&2; exit 2; }
for host in "$@"; do
  echo "== $host"
  if ! ssh -o BatchMode=yes "$host" sudo python3 /opt/node-manager/install.py upgrade; then
    echo "$host 升级失败，未处理后续节点；组件已自动恢复原版本或保留恢复记录，请先核实。" >&2
    exit 1
  fi
done
echo "全部 $# 个节点已升级。"
