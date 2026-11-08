"""Owned IPv4 port policies, with nftables transactions and reboot checkpoints."""
import calendar
from contextlib import ExitStack, contextmanager
import datetime as dt
import fcntl
import ipaddress
import json
import os
import pathlib
import re
import secrets
import subprocess
import sys
import time

from controller import Controller
from core_driver import atomic, command
from protocol import Rejected, fields, name, port, revision
from storage import private_directory, read, remove, write

MODULE = 'pm'
ACTIONS = frozenset({'install', 'apply', 'delete', 'reset', 'core_uninstall'})

ROOT = pathlib.Path('/var/lib/node-manager/pm')
SYSTEMD = pathlib.Path('/etc/systemd/system')
SERVICE_NAME = 'node-manager-pm.service'
TIMER_NAME = 'node-manager-pm.timer'
SERVICE_UNIT = """[Unit]
Description=Node Manager port policy maintenance
After=nftables.service
[Service]
Type=oneshot
ExecStart=/usr/bin/python3 /opt/node-manager/pm.py
CapabilityBoundingSet=CAP_NET_ADMIN
NoNewPrivileges=yes
ProtectHome=yes
ProtectSystem=strict
ReadWritePaths=/var/lib/node-manager/pm
"""
TIMER_UNIT = """[Unit]
Description=Maintain Node Manager port policies
[Timer]
OnBootSec=1s
OnUnitInactiveSec=60s
AccuracySec=1s
Unit=node-manager-pm.service
[Install]
WantedBy=timers.target
"""
TABLE = 'ip node_manager_pm'
# Blocked client addresses per port, in a table of their own: its elements come
# and go every minute, which the PM table's fingerprint must not see.
BLOCK_TABLE = 'inet node_manager_pm_block'
BLOCK_TIMEOUT = '30m'
# The source of a connection that failed REALITY authentication, in sing-box's log.
REALITY_PROBE = re.compile(r'process connection from \[?([0-9A-Fa-f:.]+?)\]?:\d+: .*REALITY: processed invalid connection')
GIB = 1073741824
BYTES_PER_MEGABIT = 125000
MAX_COUNTER = (1 << 63) - 1
# Addresses kept per port for the next minute's comparison.
IP_SEEN = 4096


def ip_limit(value):
    """Present only when on: rules without it keep their earlier revision, and
    so does `block`, present only as true."""
    if (not isinstance(value, dict) or set(value) - {'block'} != {'max_ips', 'cooldown_min', 'whitelist'}
            or value.get('block', True) is not True):
        raise Rejected('IP 告警设置格式错误')
    limit, cooldown, allowed = value['max_ips'], value['cooldown_min'], value['whitelist']
    if (type(limit) is not int or not 1 <= limit <= 65535 or type(cooldown) is not int
            or not 1 <= cooldown <= 525600 or not isinstance(allowed, list) or len(allowed) > 32):
        raise Rejected('IP 上限须为 1–65535，冷却 1–525600 分钟，白名单最多 32 个')
    try:
        if any(not isinstance(item, str) or '%' in item for item in allowed):
            raise ValueError()
        entries = set()
        for item in allowed:
            # Only a numeric prefix, as the Hub accepts; never a netmask.
            if '/' in item and not re.fullmatch(r'[0-9]{1,3}', item.split('/', 1)[1]):
                raise ValueError()
            network = ipaddress.ip_network(item, strict=False)
            mapped = getattr(network.network_address, 'ipv4_mapped', None)
            if mapped and network.prefixlen >= 96:
                network = ipaddress.ip_network((mapped, network.prefixlen - 96))
            entries.add(network)
    except ValueError:
        raise Rejected('白名单只能填写 IP 地址或 CIDR 网段') from None
    # A single address (or /32, /128) is written bare, keeping earlier revisions.
    whitelist = [str(n.network_address) if n.prefixlen == n.max_prefixlen else str(n)
                 for n in sorted(entries, key=lambda n: (n.version, n.network_address, n.prefixlen))]
    return dict(max_ips=limit, cooldown_min=cooldown, whitelist=whitelist, **({'block': True} if 'block' in value else {}))


def resource(value):
    fields(value, {'port', 'name', 'quota_gb', 'quota_mode', 'reset_day', 'expiry_date', 'egress_mbps', 'ip_limit'})
    listen = port(value.get('port'))
    label = name(value.get('name', ''))
    quota, reset_day, rate = (value.get(key) for key in ('quota_gb', 'reset_day', 'egress_mbps'))
    mode, expiry = value.get('quota_mode'), value.get('expiry_date')
    if (listen < 1024 or type(quota) is not int or not 0 <= quota <= 8589934591
            or type(reset_day) is not int or not 0 <= reset_day <= 31
            or type(rate) is not int or not 0 <= rate <= 10000
            or mode not in ('in_out', 'out_only') or not isinstance(expiry, str)):
        raise Rejected('PM 端口或策略值无效')
    if expiry:
        try:
            if dt.date.fromisoformat(expiry).isoformat() != expiry:
                raise ValueError()
        except ValueError:
            raise Rejected('PM 到期日期须为 YYYY-MM-DD') from None
    rule = dict(port=listen, name=label, quota_gb=quota, quota_mode=mode, reset_day=reset_day,
                expiry_date=expiry, egress_mbps=rate)
    if 'ip_limit' in value:
        rule['ip_limit'] = ip_limit(value['ip_limit'])
    return rule


def period_start(today, day):
    if not day:
        return ''
    candidate = today.replace(day=min(day, calendar.monthrange(today.year, today.month)[1]))
    if candidate > today:
        previous = today.replace(day=1) - dt.timedelta(days=1)
        candidate = previous.replace(day=min(day, previous.day))
    return candidate.isoformat()


def plan(state, today):
    # `suspended` is always false now; kept so saved plans still compare equal.
    return {
        str(rule['port']): {
            'suspended': False,
            'period': period_start(today, rule['reset_day']),
            'expired': bool(rule['expiry_date'] and today.isoformat() > rule['expiry_date']),
            'quota_bytes': rule['quota_gb'] * GIB,
            'mode': rule['quota_mode'],
            'rate': rule['egress_mbps'] * BYTES_PER_MEGABIT,
        } for rule in state['resources']
    }


def counted(values, mode):
    return values['tx'] if mode == 'out_only' else values['tx'] + values['rx']


def peers(output, ports):
    """Peer addresses per watched local port in `ss -Hnt state established`."""
    found = {}
    for line in output.splitlines():
        fields = line.split()
        if len(fields) < 4:
            continue
        port = fields[2].rpartition(':')[2]
        if port not in ports:
            continue
        host = fields[3].rpartition(':')[0].strip('[]').partition('%')[0]
        try:
            ip = ipaddress.ip_address(host)
        except ValueError:
            continue
        found.setdefault(port, set()).add(str(getattr(ip, 'ipv4_mapped', None) or ip))
    return found


def address_order(value):
    ip = ipaddress.ip_address(value)
    return ip.version, ip


def whitelisted(address, networks):
    ip = ipaddress.ip_address(address)
    return any(ip in network for network in networks)


def whitelist_networks(items):
    return tuple(ipaddress.ip_network(item, strict=False) for item in items)


def watch(rules, output, previous, now, excluded=None):
    """Online addresses of every rule's port. An address counts only once seen
    in two consecutive minutes, so a port scan or a single short visit does not;
    `excluded` (blocked, or failed authentication) does not count at all.
    A rule with an IP limit adds the original PM's check: level 1 over the
    limit, 2 over limit + 2; an alert on a rise or after the cooldown, a silent
    reset at or under the limit. With `block`, the addresses past limit + 3 that
    came online last are returned in `block`, the earliest keep their place."""
    current = peers(output, {str(rule['port']) for rule in rules})
    result = {}
    for rule in rules:
        port, limit = str(rule['port']), rule.get('ip_limit')
        old = previous.get(port, {})
        allowed = whitelist_networks(limit['whitelist'] if limit else ())
        skipped = (excluded or {}).get(port, set())
        # Each address with the minute it was first seen; 1.1.4 kept a list.
        before = old.get('seen', {})
        before = before if isinstance(before, dict) else dict.fromkeys(before, now)
        seen = {ip: before.get(ip, now)
                for ip in sorted(current.get(port, set()) - skipped, key=address_order)
                if not whitelisted(ip, allowed)}
        seen = dict(list(seen.items())[:IP_SEEN])
        online = [ip for ip in seen if ip in before]
        count = len(online)
        level = 0 if not limit else 2 if count > limit['max_ips'] + 2 else 1 if count > limit['max_ips'] else 0
        entry = {'seen': seen, 'count': count, 'level': level, 'ips': online[:15],
                 'seq': old.get('seq', 0), 'last': old.get('last', 0) if level else 0}
        if limit:
            entry['max_ips'] = limit['max_ips']
        if limit and limit.get('block') and count > limit['max_ips'] + 3:
            order = sorted(online, key=lambda ip: (seen[ip], address_order(ip)))
            entry['block'] = order[limit['max_ips'] + 3:]
        if level and (level > old.get('level', 0) or now - entry['last'] >= limit['cooldown_min'] * 60):
            entry.update(seq=entry['seq'] + 1, last=now)
        result[port] = entry
    return result


def connections():
    try:
        result = subprocess.run(
            ['ss', '-Hnt', 'state', 'established'], text=True, capture_output=True, timeout=15,
            env={'PATH': '/usr/sbin:/usr/bin:/sbin:/bin', 'LC_ALL': 'C'},
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    return None if result.returncode else result.stdout


def reality_probes(ports, units):
    """Sources of failed REALITY handshakes on managed SB ports in the last few
    minutes. REALITY hands such a connection to its cover site, where it can
    stay established long enough to count as a client."""
    found = {}
    for port in ports:
        unit = f'node-manager-sb-{port}.service'
        if not (units / unit).exists():
            continue
        try:
            result = subprocess.run(
                ['journalctl', '-u', unit, '--since', '-3min', '-o', 'cat', '-q', '--no-pager'],
                text=True, capture_output=True, timeout=15, env={'PATH': '/usr/sbin:/usr/bin:/sbin:/bin', 'LC_ALL': 'C'},
            )
        except (OSError, subprocess.TimeoutExpired):
            continue
        for match in REALITY_PROBE.finditer(result.stdout if not result.returncode else ''):
            try:
                ip = ipaddress.ip_address(match[1])
            except ValueError:
                continue
            found.setdefault(port, set()).add(str(getattr(ip, 'ipv4_mapped', None) or ip))
    return found


def block_set(ip):
    return 'v6' if ':' in ip else 'v4'


def block_script(owner, exists, add, drop, keep):
    """Create the table at the first block, then delete and add (address, port)
    elements; without `keep` the whole table goes. Every rule is created once:
    `add rule` on a table that exists would repeat it."""
    if not keep:
        return f'delete table {BLOCK_TABLE}\n' if exists else ''
    if not exists and not add:
        return ''
    lines = [] if exists else [
        f'add table {BLOCK_TABLE} {{ comment "{owner}"; }}',
        f'add set {BLOCK_TABLE} v4 {{ type ipv4_addr . inet_service; flags timeout; }}',
        f'add set {BLOCK_TABLE} v6 {{ type ipv6_addr . inet_service; flags timeout; }}',
        # Before the PM counters at priority 10: blocked traffic is not metered.
        f'add chain {BLOCK_TABLE} input {{ type filter hook input priority 0; policy accept; }}',
        f'add rule {BLOCK_TABLE} input meta l4proto {{ tcp, udp }} ip saddr . th dport @v4 drop',
        f'add rule {BLOCK_TABLE} input meta l4proto {{ tcp, udp }} ip6 saddr . th dport @v6 drop',
    ]
    lines += [f'delete element {BLOCK_TABLE} {block_set(ip)} {{ {ip} . {port} }}' for ip, port in sorted(drop)]
    lines += [f'add element {BLOCK_TABLE} {block_set(ip)} {{ {ip} . {port} timeout {BLOCK_TIMEOUT} }}'
              for ip, port in sorted(add)]
    return '\n'.join(lines) + '\n' if lines else ''


def nft(*args, script=None):
    try:
        result = subprocess.run(
            ['nft', *args], input=script, text=True, capture_output=True, timeout=15,
            env={'PATH': '/usr/sbin:/usr/bin:/sbin:/bin', 'LC_ALL': 'C'},
        )
    except (OSError, subprocess.TimeoutExpired):
        raise Rejected('无法执行 nftables，请检查软件包、内核和权限', 'pm_nft') from None
    if result.returncode:
        print('PM nftables: ' + result.stderr.strip()[:512], file=sys.stderr, flush=True)
        raise Rejected('nftables 检查或应用失败，请查看本机日志', 'pm_nft')
    return result.stdout


def entries(*args):
    try:
        return json.loads(nft('-j', '-n', *args))['nftables']
    except (ValueError, KeyError, TypeError):
        raise Rejected('无法识别 nftables 状态', 'pm_nft') from None


def batch(owner, generation, old, new, values, reset_ports, exists):
    """Keep counters and unchanged quota objects in place across rule edits."""
    lines = []
    if exists:
        lines.extend(f'flush chain {TABLE} {direction}' for direction in ('input', 'output'))
        lines.append(f'delete chain {TABLE} epoch_{exists}')
    else:
        lines.append(f'create table {TABLE} {{ comment "{owner}"; }}')
        for direction in ('input', 'output'):
            lines.append(f'add chain {TABLE} {direction} {{ type filter hook {direction} priority 10; policy accept; }}')
    lines.append(f'add chain {TABLE} epoch_{generation}')
    for port in sorted(set(old) | set(new), key=int):
        previous, current = old.get(port), new.get(port)
        reset = port in reset_ports
        old_quota = previous and previous['quota_bytes']
        new_quota = current and current['quota_bytes']
        keep_quota = old_quota and new_quota and not reset and all(
            previous[key] == current[key] for key in ('quota_bytes', 'mode')
        ) and (not previous['expired'] or current['expired'])
        if previous and (not current or reset):
            for kind in ('rx', 'tx'):
                lines.append(f'delete counter {TABLE} {kind}_{port}')
        if old_quota and not keep_quota:
            lines.append(f'delete quota {TABLE} quota_{port}')
        if not current:
            continue
        value = values[port]
        if not previous or reset:
            for kind in ('rx', 'tx'):
                lines.append(f'add counter {TABLE} {kind}_{port} {{ packets 0 bytes {value[kind]}; }}')
        if new_quota and not keep_quota:
            used = min(new_quota, counted(value, current['mode']))
            lines.append(f'add quota {TABLE} quota_{port} {{ over {new_quota} bytes used {used} bytes; }}')
        for chain, iface, direction, field, counter in (
            ('input', 'iifname', 'original', 'dport', 'rx'),
            ('output', 'oifname', 'reply', 'sport', 'tx'),
        ):
            match = f'{iface} != "lo" ct direction {direction} meta l4proto {{ tcp, udp }} th {field} {port}'
            rule = f'add rule {TABLE} {chain} {match} counter name "{counter}_{port}"'
            if current['expired']:
                rule += ' drop'
            elif new_quota and (counter == 'tx' or current['mode'] == 'in_out'):
                rule += f' quota name "quota_{port}" drop'
            lines.append(rule)
            if counter == 'tx' and current['rate'] and not current['expired']:
                lines.append(f'add rule {TABLE} {chain} {match} limit rate over {current["rate"]} bytes/second drop')
    return '\n'.join(lines) + '\n'


class Driver:
    def __init__(self, root=ROOT, units=SYSTEMD):
        self.root = private_directory(root)
        self.units = units
        self.checkpoint = self.root / 'checkpoint.json'
        self.transition = self.root / 'nft-transition.json'
        self.ip_watch = self.root / 'ip-watch.json'
        self.boot_id = pathlib.Path('/proc/sys/kernel/random/boot_id')
        self.owner = read(self.root / 'owner.json')

    def unit_files(self):
        return ((self.units / SERVICE_NAME, SERVICE_UNIT), (self.units / TIMER_NAME, TIMER_UNIT))

    def inspect(self):
        tables = entries('list', 'tables')
        if not any(row.get('table', {}).get('family') == 'ip' and
                   row['table'].get('name') == 'node_manager_pm' for row in tables):
            return None
        fingerprint, values, quotas, epochs = [], {}, {}, []
        owned = False
        for row in entries('list', 'table', *TABLE.split()):
            kind, value = next(iter(row.items()))
            if kind == 'metainfo':
                continue
            if kind == 'table':
                owned = bool(self.owner and value.get('comment') == self.owner)
            if kind == 'chain' and re.fullmatch(r'epoch_[0-9a-f]{16}', value['name']):
                epochs.append(value['name'][6:])
            if kind == 'counter':
                match = re.fullmatch(r'(rx|tx)_([1-9][0-9]{0,4})', value['name'])
                if match:
                    counter, port = match.groups()
                    values.setdefault(port, {})[counter] = min(value['bytes'], MAX_COUNTER)
            ignored = {'handle'}
            if kind == 'counter':
                ignored.update(('packets', 'bytes'))
            if kind == 'quota':
                ignored.add('used')
                match = re.fullmatch(r'quota_([1-9][0-9]{0,4})', value['name'])
                if match:
                    quotas[match[1]] = value.get('used', 0)
            fingerprint.append({kind: {key: v for key, v in value.items() if key not in ignored}})
        if not owned or len(epochs) != 1:
            raise Rejected('存在无法确认归属的 nftables 表，已拒绝修改', 'pm_conflict')
        return {'fingerprint': revision(fingerprint), 'epoch': epochs[0], 'values': values, 'quotas': quotas}

    def sample(self, live, checkpoint):
        if not live or not checkpoint or live['fingerprint'] != checkpoint['fingerprint']:
            raise Rejected('PM 表缺失或被外部修改，请先核实本机规则', 'pm_drift')
        if set(live['values']) != set(checkpoint['plan']) or any(
            set(value) != {'rx', 'tx'} for value in live['values'].values()
        ):
            raise Rejected('PM 计数对象不完整', 'pm_drift')
        if any(value[key] < checkpoint['values'][port][key]
               for port, value in live['values'].items() for key in ('rx', 'tx')):
            raise Rejected('PM 计数器被外部重置，请先核实本机规则', 'pm_drift')
        limits = {port: rule['quota_bytes'] for port, rule in checkpoint['plan'].items() if rule['quota_bytes']}
        if set(live['quotas']) != set(limits) or any(
            type(used) is not int or not checkpoint.get('quotas', {}).get(port, 0) <= used <= limits[port]
            for port, used in live['quotas'].items()
        ):
            raise Rejected('PM 配额用量被外部重置，请先核实本机规则', 'pm_drift')
        return {**checkpoint, 'values': live['values'], 'quotas': live['quotas'], 'sampled_at': int(time.time())}

    def settle(self, live):
        """Finish an interrupted nft transaction only if its generation committed."""
        pending = read(self.transition)
        saved = read(self.checkpoint)
        if pending:
            if live and live['epoch'] == pending['epoch']:
                if set(live['values']) != set(pending['plan']) or any(
                    set(value) != {'rx', 'tx'} for value in live['values'].values()
                ):
                    raise Rejected('PM 新规则的计数对象不完整', 'pm_drift')
                saved = {**pending, **live, 'sampled_at': int(time.time())}
                write(self.checkpoint, saved)
            elif live and (not saved or live['fingerprint'] != saved['fingerprint']):
                raise Rejected('PM 待恢复事务与当前 nftables 表不一致', 'pm_drift')
            remove(self.transition)
        return saved

    def replace(self, live, saved, target, rebuild=(), zero=(), rollback_of=None):
        if not isinstance(self.owner, str) or not re.fullmatch(r'node-manager/pm/[0-9a-f]{32}', self.owner):
            raise Rejected('PM 表的本机所有权记录缺失或损坏', 'pm_drift')
        old = saved['plan'] if live else {}
        values = {
            port: {'rx': 0, 'tx': 0} if port in zero else
            (saved['values'].get(port, {'rx': 0, 'tx': 0}) if saved else {'rx': 0, 'tx': 0})
            for port in target
        }
        generation = secrets.token_hex(8)
        script = batch(self.owner, generation, old, target, values, rebuild, live and live['epoch'])
        nft('-c', '-f', '-', script=script)
        write(self.transition, {'epoch': generation, 'plan': target, 'values': values,
                                'boot': self.boot_id.read_text().strip(), 'rollback_of': rollback_of})
        nft('-f', '-', script=script)
        applied = self.inspect()
        if not applied or applied['epoch'] != generation:
            raise Rejected('无法核实 PM 新规则已应用', 'pm_drift')
        return self.settle(applied)

    def reconcile(self, state, reset_port=None, keep_period_port=None, allow_missing=False, bootstrap=False):
        live = self.inspect()
        saved = self.settle(live)
        boot = self.boot_id.read_text().strip()
        if live:
            saved = self.sample(live, saved)
        elif saved:
            if saved['boot'] == boot and not allow_missing:
                raise Rejected('本次开机期间 PM 表被删除，请先核实本机规则', 'pm_drift')
            self.check_saved_state(state, saved)
        elif not bootstrap:
            raise Rejected('PM 检查点缺失，禁止以零用量重建', 'pm_drift')
        previous = saved['plan'] if saved else {}
        target = self.expected_plan(state)
        resets = {
            port for port in previous.keys() & target.keys()
            if previous[port]['period'] != target[port]['period'] and port != keep_period_port
        }
        if reset_port is not None:
            resets.add(str(reset_port))
        if not live or previous != target or resets:
            return self.replace(live, saved, target, resets, resets)
        write(self.checkpoint, saved)
        return saved

    @staticmethod
    def expected_plan(state):
        return plan(state, dt.datetime.now(dt.timezone.utc).date())

    @staticmethod
    def unit_states():
        """Both PM units in one systemctl call, for the status every poll reads."""
        keys = ('Id', 'LoadState', 'FragmentPath', 'DropInPaths', 'ActiveState', 'UnitFileState')
        result = command('systemctl', 'show', '--all', '--property=' + ','.join(keys),
                         SERVICE_NAME, TIMER_NAME, check=False)
        states = {}
        for block in result.stdout.decode().split('\n\n'):
            info = dict(line.split('=', 1) for line in block.splitlines() if '=' in line)
            if info:
                states[info.get('Id')] = info
        if set(states) != {SERVICE_NAME, TIMER_NAME} or any(
                key not in info for info in states.values() for key in keys):
            raise Rejected('无法核实 PM 定时单元归属', 'pm_timer')
        return states

    @staticmethod
    def unit_info(name):
        keys = 'LoadState,FragmentPath,DropInPaths,ActiveState'
        result = command('systemctl', 'show', '--all', f'--property={keys}', name, check=False)
        info = dict(line.split('=', 1) for line in result.stdout.decode().splitlines() if '=' in line)
        if any(key not in info for key in keys.split(',')):
            raise Rejected('无法核实 PM 定时单元归属', 'pm_timer')
        return info

    @staticmethod
    def complete(item, _existing):
        return item

    def prepare(self, action, before, after, payload):
        if action == 'install':
            for path, _ in self.unit_files():
                info = self.unit_info(path.name)
                if (path.exists() or path.is_symlink() or info['LoadState'] != 'not-found'
                        or info['FragmentPath'] or info['DropInPaths']):
                    raise Rejected('检测到同名 PM 单元，已拒绝接管', 'pm_conflict')
            if self.inspect() or read(self.checkpoint) or read(self.transition):
                raise Rejected('检测到已有 PM 状态，已拒绝接管', 'pm_conflict')
            return {'installed': False}
        saved = self.verify(before, {})
        if action == 'core_uninstall' and (before['resources'] or saved['plan']):
            raise Rejected('请先删除所有受管 PM 规则', 'core_in_use')
        return {'installed': True, 'state': before, 'checkpoint': saved,
                'uninstall': action == 'core_uninstall',
                'reset_port': str(payload['port']) if action == 'reset' else None}

    def apply(self, action, before, after, payload):
        if action == 'install':
            if self.owner is None:
                self.owner = 'node-manager/pm/' + secrets.token_hex(16)
                write(self.root / 'owner.json', self.owner)
            self.reconcile(after, bootstrap=True)
            for path, content in self.unit_files():
                atomic(path, content.encode(), 0o644)
            command('systemctl', 'daemon-reload')
            # The oneshot waits on write.lock. Do not synchronously start it here.
            command('systemctl', 'enable', '--now', TIMER_NAME)
            return
        if action == 'core_uninstall':
            self.restore({'installed': False})
            return
        port = payload['port']
        old = next((rule for rule in before['resources'] if rule['port'] == port), None)
        new_period = action == 'apply' and old and old['reset_day'] != payload['reset_day']
        self.reconcile(after, port if action == 'reset' else None, str(port) if new_period else None)

    def verify(self, state, _payload):
        if not state['installed']:
            if self.inspect() or self.blocked() is not None or read(self.checkpoint) or read(self.transition):
                raise Rejected('PM 规则表、阻断表或检查点仍然存在', 'pm_conflict')
            for path, _ in self.unit_files():
                info = self.unit_info(path.name)
                if (path.exists() or path.is_symlink() or info['LoadState'] != 'not-found'
                        or info['FragmentPath'] or info['DropInPaths']):
                    raise Rejected('PM 定时单元仍然存在', 'pm_conflict')
            return
        if not self.units_match() or not self.timer_active():
            raise Rejected('PM 定时单元未正常启用，请查看本机服务', 'pm_timer')
        return self.sample(self.inspect(), read(self.checkpoint))

    def refresh(self, state):
        if not state['installed']:
            return
        stale = [(path, content) for path, content in self.unit_files()
                 if not path.exists() or path.read_text() != content]
        for path, content in stale:
            atomic(path, content.encode(), 0o644)
        if stale:
            command('systemctl', 'daemon-reload')
        # Rebuilds the table to the current layout; counters carry over as on any rule edit.
        self.reconcile(state)

    def units_match(self, states=None):
        try:
            for path, content in self.unit_files():
                if path.is_symlink() or path.read_text() != content:
                    return False
                info = states[path.name] if states else self.unit_info(path.name)
                if (info['LoadState'] != 'loaded' or info['FragmentPath'] != str(path)
                        or info['DropInPaths']):
                    return False
            return True
        except OSError:
            return False

    def recover_committed(self, state):
        checkpoint = read(self.checkpoint)
        if checkpoint and checkpoint['boot'] != self.boot_id.read_text().strip() and not self.inspect():
            self.reconcile(state)
        self.verify(state, {})

    @staticmethod
    def timer_active(states=None):
        if states:
            # The states for which `systemctl is-enabled` exits 0.
            timer = states[TIMER_NAME]
            return timer['ActiveState'] == 'active' and timer['UnitFileState'] in (
                'enabled', 'enabled-runtime', 'static', 'alias', 'indirect', 'generated', 'transient')
        return all(command('systemctl', action, '--quiet', TIMER_NAME, check=False).returncode == 0
                   for action in ('is-active', 'is-enabled'))

    def restore(self, backup):
        live = self.inspect()
        saved = self.settle(live)
        if not backup['installed']:
            if live:
                self.sample(live, saved)
            for path, content in self.unit_files():
                if path.is_symlink() or (path.exists() and path.read_text() != content):
                    raise Rejected('PM 定时单元已被外部修改', 'pm_conflict')
            for path, _ in self.unit_files():
                info = self.unit_info(path.name)
                if info['DropInPaths'] or (info['FragmentPath'] and info['FragmentPath'] != str(path)):
                    raise Rejected('PM 定时单元的实际加载路径不匹配', 'pm_conflict')
            if (self.units / TIMER_NAME).exists():
                command('systemctl', 'disable', '--now', TIMER_NAME)
            if (self.units / SERVICE_NAME).exists():
                # A queued oneshot may be waiting for our writer lock.
                command('systemctl', 'stop', SERVICE_NAME)
            for path, _ in self.unit_files():
                remove(path)
            command('systemctl', 'daemon-reload')
            if live:
                # The epoch must still exist in the same atomic teardown transaction.
                nft('-f', '-', script=f'delete chain {TABLE} epoch_{live["epoch"]}\ndelete table {TABLE}\n')
            # Unlike the minute's check, uninstall fails rather than leave it behind.
            if self.blocked() is not None:
                nft('-f', '-', script=block_script(self.owner, True, (), (), False))
            remove(self.checkpoint)
            remove(self.ip_watch)
            return
        prior = backup['checkpoint']
        if backup.get('uninstall'):
            self.restore_empty_runtime(live, saved, prior)
            self.verify(backup['state'], {})
            return
        target = prior['plan']
        reset_port = backup.get('reset_port')
        if saved and saved.get('rollback_of') == prior['epoch']:
            if not live and saved['boot'] != self.boot_id.read_text().strip():
                self.replace(None, saved, target, rollback_of=prior['epoch'])
            self.verify(backup['state'], {})
            return
        if live:
            saved = self.sample(live, saved)
            if (not reset_port or saved['epoch'] == prior['epoch']) and saved['plan'] == target and all(
                port in saved['values'] and
                all(saved['values'][port][key] >= value[key] for key in ('rx', 'tx'))
                for port, value in prior['values'].items()
            ):
                self.verify(backup['state'], {})
                return
        else:
            saved = None
        # A failed delete or reset can remove counters. Restore their durable pre-change values.
        changed = set()
        merged = dict(saved['values']) if saved else {}
        for port, value in prior['values'].items():
            if not saved or port not in saved['plan'] or saved['plan'][port]['period'] != target[port]['period']:
                merged[port] = value
                changed.add(port)
            elif port == reset_port and saved['epoch'] != prior['epoch']:
                merged[port] = {key: min(MAX_COUNTER, merged[port][key] + value[key]) for key in ('rx', 'tx')}
                changed.add(port)
            elif any(merged[port][key] < value[key] for key in ('rx', 'tx')):
                merged[port] = {key: max(merged[port][key], value[key]) for key in ('rx', 'tx')}
                changed.add(port)
        self.replace(live, {'plan': saved['plan'] if saved else {}, 'values': merged}, target, changed,
                     rollback_of=prior['epoch'])
        self.verify(backup['state'], {})

    def restore_empty_runtime(self, live, saved, prior):
        for path, content in self.unit_files():
            info = self.unit_info(path.name)
            if (path.is_symlink() or (path.exists() and path.read_text() != content)
                    or info['DropInPaths'] or (info['FragmentPath'] and info['FragmentPath'] != str(path))):
                raise Rejected('PM 定时单元归属发生变化，禁止自动恢复', 'pm_conflict')
        if live:
            if self.sample(live, saved)['plan']:
                raise Rejected('存在非空 PM 规则，禁止自动恢复', 'pm_conflict')
        else:
            self.replace(None, prior, {})
        for path, content in self.unit_files():
            if not path.exists():
                atomic(path, content.encode(), 0o644)
        command('systemctl', 'daemon-reload')
        if not self.units_match():
            raise Rejected('PM 定时单元恢复后归属不一致', 'pm_conflict')
        if not self.timer_active():
            command('systemctl', 'enable', '--now', TIMER_NAME)

    def status(self, state):
        if not state['installed']:
            return False, {}
        try:
            states = self.unit_states()
            if not self.units_match(states) or not self.timer_active(states):
                raise Rejected('PM 定时单元不可用', 'pm_timer')
            saved = read(self.checkpoint)
            data = self.sample(self.inspect(), saved)
            failed = states[SERVICE_NAME]['ActiveState'] == 'failed'
        except (Rejected, OSError, ValueError, subprocess.TimeoutExpired):
            return True, {}
        usage = {}
        expected = self.expected_plan(state)
        limited = {str(rule['port']) for rule in state['resources'] if 'ip_limit' in rule}
        try:
            recent = read(self.ip_watch, {})
        except ValueError:
            recent = {}
        # A stopped check must not keep showing its last count as current.
        ips = recent.get('ports', {}) if time.time() - recent.get('sampled_at', 0) <= 150 else {}
        for port, values in data['values'].items():
            rule = data['plan'][port]
            used = data['quotas'].get(port, 0)
            quota = rule['quota_bytes']
            if expected.get(port) != rule:
                alert = 'maintenance_pending'
            elif rule['expired']:
                alert = 'expired'
            elif quota and used >= quota:
                alert = 'quota_reached'
            elif quota and used * 100 >= quota * 80:
                alert = 'quota_warning'
            else:
                alert = 'normal'
            usage[port] = {**values, 'sampled_at': data['sampled_at'], 'period_start': rule['period'], 'alert': alert}
            if port in ips:
                entry = ips[port]
                usage[port].update(ip_count=entry['count'], ip_list=entry['ips'])
                if port in limited:
                    usage[port].update(ip_level=entry['level'], ip_alert=entry['seq'])
        return failed or time.time() - saved['sampled_at'] > 150, usage

    @staticmethod
    def check_saved_state(state, saved):
        try:
            policies = plan({'resources': [resource(rule) for rule in state['resources']]},
                            dt.datetime.now(dt.timezone.utc).date())
            if set(policies) != set(saved['plan']) or set(policies) != set(saved['values']):
                raise ValueError()
            if not re.fullmatch(r'[0-9a-f]{64}', saved['fingerprint']):
                raise ValueError()
            for port, policy in policies.items():
                if any(saved['plan'][port][key] != policy[key] for key in ('mode', 'quota_bytes', 'rate')):
                    raise ValueError()
                values = saved['values'][port]
                if set(values) != {'rx', 'tx'} or any(type(n) is not int or not 0 <= n <= MAX_COUNTER for n in values.values()):
                    raise ValueError()
        except (KeyError, TypeError, ValueError):
            raise Rejected('PM 检查点与确认的规则不一致，禁止自动恢复', 'pm_drift') from None

    @contextmanager
    def local_lock(self, *, lock_writer=True):
        from install import LIFECYCLE_LOCK, PURGE_JOURNAL, lock, present

        with ExitStack() as locks:
            if self.root == ROOT:
                locks.enter_context(lock(LIFECYCLE_LOCK))
                if present(PURGE_JOURNAL):
                    raise Rejected('组件清理尚未完成，请使用相同安装器重试 uninstall --purge')
            locks.enter_context(lock(self.root.parent / 'lifecycle.lock'))
            if lock_writer:
                writer = locks.enter_context((self.root / 'write.lock').open('a'))
                fcntl.flock(writer, fcntl.LOCK_EX)
            if (self.root.parent / 'component-recovery.json').exists():
                raise Rejected('组件升级或卸载尚未恢复，请先执行 install.py recover')
            if (self.root / 'recovery.json').exists():
                raise Rejected('PM 有未完成的操作，请先执行 agent.py recover')
            yield

    def disable_empty(self):
        from controller import Controller

        # The controller takes the writer lock. Keep the outer lifecycle locks
        # here so local uninstall cannot run during component maintenance.
        with self.local_lock(lock_writer=False):
            state = read(self.root / 'state.json', {'installed': False, 'resources': []})
            if not state['installed']:
                if any(path.exists() or path.is_symlink() for path, _ in self.unit_files()):
                    raise Rejected('存在无法确认归属的 PM 单元，请核实本机状态')
                return
            controller = Controller(self.root, self, 'pm')
            result = controller.execute({'id': secrets.token_hex(16), 'module': 'pm',
                                         'action': 'core_uninstall', 'revision': revision(state),
                                         'payload': {}}, local=True)
            if result['status'] != 'succeeded':
                raise Rejected(result['message'], result.get('code', 'pm_conflict'))

    def repair_missing(self):
        with self.local_lock():
            state = read(self.root / 'state.json')
            saved = read(self.checkpoint)
            if not state or not state['installed'] or not saved or self.transition.exists():
                raise Rejected('没有可安全恢复的 PM 已确认状态，或有待核实的规则事务')
            if self.inspect():
                raise Rejected('PM 表仍然存在，请核实外部修改；此操作只重建完全缺失的表', 'pm_drift')
            if not self.units_match() or not self.timer_active():
                raise Rejected('PM 定时单元归属或运行状态异常', 'pm_timer')
            self.reconcile(state, allow_missing=True)
            self.verify(state, {})

    def maintain(self):
        with (self.root / 'write.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            if read(self.root.parent / 'component-recovery.json', {}).get('phase') == 'switching':
                raise Rejected('组件文件正在切换，暂停 PM 维护直至升级恢复')
            if (self.root / 'recovery.json').exists():
                print('PM 等待恢复，请执行 agent.py recover', file=sys.stderr)
                return
            state = read(self.root / 'state.json')
            if state and state['installed']:
                try:
                    self.reconcile(state)
                finally:
                    self.watch_ips(state)

    def blocked(self):
        """(address, port) pairs in our block table, None without one. A table
        of that name that is not ours is never read or changed."""
        family, name = BLOCK_TABLE.split()
        if not any(row.get('table', {}).get('family') == family and row['table'].get('name') == name
                   for row in entries('list', 'tables')):
            return None
        found = set()
        for row in entries('list', 'table', family, name):
            if 'table' in row and not (self.owner and row['table'].get('comment') == self.owner):
                raise Rejected('存在同名但不属于 PM 的阻断表，已跳过阻断', 'pm_conflict')
            for item in row.get('set', {}).get('elem', []):
                value = item.get('elem', {}).get('val', item) if isinstance(item, dict) else item
                pair = value.get('concat') if isinstance(value, dict) else None
                if isinstance(pair, list) and len(pair) == 2:
                    ip = ipaddress.ip_address(pair[0])
                    found.add((str(getattr(ip, 'ipv4_mapped', None) or ip), str(pair[1])))
        return found

    def unblock_all(self):
        try:
            if self.blocked() is not None:
                nft('-f', '-', script=block_script(self.owner, True, (), (), False))
        except (Rejected, ValueError) as error:
            print(f'PM 阻断表未删除：{error}', file=sys.stderr, flush=True)

    def watch_ips(self, state):
        rules = state['resources']
        if not rules:
            remove(self.ip_watch)
            self.unblock_all()
            return
        output = connections()
        if output is None:
            print('PM 无法读取 TCP 连接，跳过本轮 IP 检查', file=sys.stderr, flush=True)
            return
        now = int(time.time())
        try:
            previous = read(self.ip_watch, {}).get('ports', {})
        except ValueError:
            previous = {}
        # Blocking is best effort: it never stops the count or the alerts.
        try:
            current = self.blocked()
        except (Rejected, ValueError) as error:
            print(f'PM 阻断状态不可读：{error}', file=sys.stderr, flush=True)
            current = False
        excluded = reality_probes({str(rule['port']) for rule in rules}, self.units)
        for ip, port in current or ():
            excluded.setdefault(port, set()).add(ip)
        ports = watch(rules, output, previous, now, excluded)
        changes = {port: entry.pop('block', []) for port, entry in ports.items()}
        write(self.ip_watch, {'sampled_at': now, 'ports': ports})
        if current is False:
            return
        blocking = {str(rule['port']): whitelist_networks(rule['ip_limit']['whitelist'])
                    for rule in rules if rule.get('ip_limit', {}).get('block')}
        # A changed limit is judged again from scratch: raising it frees the port at once.
        changed = {port for port, entry in ports.items()
                   if entry.get('max_ips') != previous.get(port, {}).get('max_ips')}
        drop = {(ip, port) for ip, port in current or ()
                if port not in blocking or whitelisted(ip, blocking[port]) or port in changed}
        add = {(ip, port) for port, ips in changes.items() if port in blocking for ip in ips}
        script = block_script(self.owner, current is not None, add, drop, bool(blocking))
        if script:
            try:
                nft('-f', '-', script=script)
            except Rejected as error:
                print(f'PM 阻断未生效：{error}', file=sys.stderr, flush=True)


class PMController(Controller):
    def snapshot(self):
        with (self.root / 'write.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_SH)
            state = read(self.state, {'installed': False, 'resources': []})
            drift, usage = self.driver.status(state)
            return {**state, 'revision': revision(state), 'blocked': self.journal.exists() or drift,
                    'usage': usage,
                    'core': {'version': None, 'sha256': None, 'previous_version': None, 'candidates': []}}


def controller(root):
    directory = root / MODULE
    return PMController(directory, Driver(directory), MODULE)


if __name__ == '__main__':
    try:
        if os.geteuid() != 0:
            raise Rejected('PM 操作需要 root 权限')
        # As under the PM unit: state written from a shell must stay private.
        os.umask(0o077)
        if sys.argv[1:] == ['disable-empty']:
            Driver().disable_empty()
            print('PM 独立定时单元和空规则表已移除。')
        elif sys.argv[1:] == ['repair', '--confirm-checkpoint-loss']:
            Driver().repair_missing()
            print('PM 表已从最近一次持久检查点恢复；检查点之后的用量可能丢失。')
        elif len(sys.argv) == 1:
            Driver().maintain()
        else:
            raise Rejected('未知 PM 操作')
    except (Rejected, OSError, ValueError, subprocess.TimeoutExpired) as error:
        print(f'PM 维护失败：{error}', file=sys.stderr)
        sys.exit(1)

