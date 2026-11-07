#!/usr/bin/env python3
"""Install and maintain the optional node manager on Linux systemd."""
import argparse
import contextlib
import fcntl
import fnmatch
import hashlib
import json
import os
import pathlib
import platform
import re
import secrets
import stat
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = pathlib.Path('/var/lib/node-manager')
CODE = pathlib.Path('/opt/node-manager')
UNITS = pathlib.Path('/etc/systemd/system')
SERVICE = 'node-manager-agent.service'
MANIFEST = 'component-manifest.json'
JOURNAL = 'component-recovery.json'
LIFECYCLE_LOCK = pathlib.Path('/run/node-manager-component-lifecycle.lock')
PURGE_JOURNAL = pathlib.Path('/var/lib/.node-manager-component-purge.json')
MODULES = ('sb', 'fw', 'pm')
PM_SERVICE = 'node-manager-pm.service'
PM_TIMER = 'node-manager-pm.timer'
DIGEST = re.compile(r'[0-9a-f]{64}')
BACKUP_ID = re.compile(r'[0-9a-f]{32}')
# The Hub decides what a release contains, so adding a module needs no installer change.
COMPONENT_FILE = re.compile(r'[a-z][a-z0-9_]*\.(?:py|json)')

SERVICE_UNIT = '''[Unit]
Description=Node Manager optional management component
After=network-online.target
Wants=network-online.target
[Service]
ExecStart=/usr/bin/python3 /opt/node-manager/agent.py run
Restart=on-failure
RestartSec=5
UMask=0077
Environment=PYTHONDONTWRITEBYTECODE=1
MemoryMax=256M
TasksMax=64
CPUQuota=25%
[Install]
WantedBy=multi-user.target
'''


# `sudo nodehelm` opens the node menu. It lives and goes with the component, and
# the marker line is how it is told apart from a command someone else installed.
COMMAND = pathlib.Path('/usr/local/sbin/nodehelm')
COMMAND_MARKER = b'# NodeHelm node menu; installed and removed with the management component.'
COMMAND_TEXT = (b'#!/bin/sh\n' + COMMAND_MARKER + b'\n'
                b'[ -f /opt/node-manager/menu.py ] || { echo "nodehelm: this component version has no menu" >&2; exit 1; }\n'
                b'exec /usr/bin/python3 -B /opt/node-manager/menu.py "$@"\n')


def own_command():
    try:
        return stat.S_ISREG(COMMAND.lstat().st_mode) and COMMAND_MARKER in COMMAND.read_bytes()[:200]
    except OSError:
        return False


def place_command():
    if not COMMAND.parent.is_dir():
        return
    if present(COMMAND) and not own_command():
        print(f'{COMMAND} 已被其他程序占用，未安装 nodehelm 命令')
        return
    if not present(COMMAND) or COMMAND.read_bytes() != COMMAND_TEXT:
        write_file(COMMAND, COMMAND_TEXT, 0o755)


def remove_command():
    if own_command():
        COMMAND.unlink()
        sync(COMMAND.parent)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args):
        raise ValueError('拒绝下载地址跳转')


def digest(data):
    return hashlib.sha256(data).hexdigest()


def sync(directory):
    fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def present(path):
    return path.exists() or path.is_symlink()


def owned_file(path, private=False):
    meta = path.lstat()
    if (not stat.S_ISREG(meta.st_mode) or meta.st_uid != os.geteuid()
            or meta.st_mode & (0o077 if private else 0o022) or meta.st_nlink != 1):
        raise ValueError(f'{path.name} 不是权限受限的独立普通文件')
    return path


def owned_directory(directory, private, owner=None):
    meta = directory.lstat()
    if (not stat.S_ISDIR(meta.st_mode) or meta.st_uid != (os.geteuid() if owner is None else owner)
            or meta.st_mode & (0o077 if private else 0o022)):
        raise ValueError(f'{directory.name} 目录所有者或权限不符合要求')
    return directory


def prepare_directories(root, code, owner=0):
    for directory, mode in ((root, 0o700), (root / 'configs', 0o700), (code, 0o755)):
        if not present(directory):
            directory.mkdir(mode=mode, parents=True, exist_ok=True)
        owned_directory(directory, mode == 0o700, owner)
    code.chmod(0o755)  # DynamicUser needs to execute installed business cores.


def write_file(path, data, mode=0o600):
    if present(path):
        owned_file(path)
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix='.node-manager-')
    try:
        with os.fdopen(fd, 'wb') as out:
            out.write(data)
            os.fchmod(out.fileno(), mode)
            out.flush()
            os.fsync(out.fileno())
        os.replace(tmp, path)
        sync(path.parent)
    finally:
        pathlib.Path(tmp).unlink(missing_ok=True)


def write_json(path, value):
    write_file(path, json.dumps(value, sort_keys=True).encode())


def read_json(path, default=None):
    return json.loads(owned_file(path, private=True).read_bytes()) if present(path) else default


def remove(path):
    if present(path):
        owned_file(path).unlink()
        sync(path.parent)


@contextlib.contextmanager
def lock(path):
    if present(path):
        owned_file(path, private=True)
    fd = os.open(path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    with os.fdopen(fd, 'r+') as held:
        try:
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise ValueError('管理组件正在执行操作，请稍后重试') from None
        yield


def base_url(value):
    u = urllib.parse.urlsplit(value)
    if u.scheme != 'https' or not u.hostname or u.username or u.password or u.query or u.fragment or u.path not in ('', '/'):
        raise ValueError('后台地址须为 HTTPS 根地址')
    return value.rstrip('/')


def request(server, path, payload=None, credential=None):
    headers = {'User-Agent': 'NodeManager/1'}
    if payload is not None:
        headers['Content-Type'] = 'application/json'
        headers['Authorization'] = 'Bearer ' + credential
    req = urllib.request.Request(base_url(server) + path, headers=headers,
                                 data=json.dumps(payload).encode() if payload is not None else None)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    with opener.open(req, timeout=25) as response:
        data = response.read(1024 * 1024 + 1)
    if len(data) > 1024 * 1024:
        raise ValueError('下载文件过大')
    return data


def manifest(value):
    if (not isinstance(value, dict) or 'agent.py' not in value
            or not all(isinstance(name, str) and COMPONENT_FILE.fullmatch(name) for name in value)
            or not all(isinstance(sha, str) and DIGEST.fullmatch(sha) for sha in value.values())):
        raise ValueError('组件清单文件集合或摘要无效')
    return value


def download(server):
    expected = manifest(json.loads(request(server, '/management-files/manifest.json')))
    data = {}
    for name, sha in expected.items():
        payload = request(server, '/management-files/' + name)
        if digest(payload) != sha:
            raise ValueError(f'组件 {name} 下载校验失败')
        if name.endswith('.py'):
            compile(payload, name, 'exec')
        else:
            json.loads(payload)
        data[name] = payload
    return expected, data


def check_current(expected, alternate=None, allow_missing=False):
    for name in {*expected, *(alternate or ())}:
        path = CODE / name
        valid = [m.get(name) for m in (expected, alternate) if m is not None]
        if not present(path):
            if None not in valid and not allow_missing:
                raise ValueError(f'已安装组件缺少 {name}')
        elif digest(owned_file(path).read_bytes()) not in valid:
            raise ValueError(f'组件 {name} 未通过清单校验，拒绝替换')


def installed_manifest():
    current = read_json(ROOT / MANIFEST)
    if current is None:
        raise ValueError('缺少安装清单，请核实组件安装状态')
    return manifest(current)


def check_owned_unit(path, content=SERVICE_UNIT):
    if present(path) and owned_file(path).read_bytes() != content.encode():
        raise ValueError(f'发现未知或已修改的服务文件 {path.name}，拒绝修改')


def ctl(*args, check=True):
    result = subprocess.run(('/usr/bin/systemctl', *args), capture_output=True, text=True,
                            stdin=subprocess.DEVNULL, timeout=45, check=False,
                            env={'PATH': '/usr/sbin:/usr/bin:/sbin:/bin', 'LC_ALL': 'C'})
    if check and result.returncode:
        raise ValueError(f'系统服务操作失败（{args[0]}），请检查服务日志')
    return result


def unit_info(name):
    keys = ('LoadState', 'FragmentPath', 'DropInPaths', 'ActiveState', 'MainPID', 'NRestarts')
    result = ctl('show', '--all', '--property=' + ','.join(keys), name, check=False)
    info = dict(line.split('=', 1) for line in result.stdout.splitlines() if '=' in line)
    if info.get('LoadState') == 'not-found' and not present(UNITS / name):
        return info
    if (result.returncode or info.get('LoadState') != 'loaded'
            or info.get('FragmentPath') != str(UNITS / name) or info.get('DropInPaths') != ''):
        raise ValueError(f'{name} 的实际加载路径或覆盖配置不符合要求')
    return info


def pm_info(current):
    expected = {}
    if any(present(UNITS / name) for name in (PM_SERVICE, PM_TIMER)):
        if 'pm.py' not in current:
            raise ValueError('发现不属于当前组件版本的 PM 服务')
        sys.path.insert(0, str(CODE))
        from pm import SERVICE_UNIT as pm_service, TIMER_UNIT as pm_timer
        expected = {PM_SERVICE: digest(pm_service.encode()), PM_TIMER: digest(pm_timer.encode())}
    check_pm_units(expected)
    return expected, unit_info(PM_TIMER).get('ActiveState') == 'active'


def check_pm_units(expected):
    for name in (PM_SERVICE, PM_TIMER):
        path = UNITS / name
        if (name in expected) != present(path) or (name in expected and digest(owned_file(path).read_bytes()) != expected[name]):
            raise ValueError(f'{name} 与已确认的 PM 服务文件不一致')
        unit_info(name)


def wait_for(predicate, message, timeout=40):
    deadline = time.monotonic() + timeout
    while not predicate():
        if time.monotonic() >= deadline:
            raise ValueError(message)
        time.sleep(1)


def ensure_idle():
    for directory in (ROOT, *(ROOT / module for module in MODULES)):
        if present(directory):
            owned_directory(directory, True)
            if any(present(directory / name) for name in ('recovery.json', 'nft-transition.json')):
                raise ValueError('有未恢复业务操作，请先执行 agent.py recover')
    for name in ('pending-result.json', 'enrollment.json'):
        if present(ROOT / name):
            raise ValueError('有未确认回执或登记，请先核实')


def ensure_no_journal():
    if present(PURGE_JOURNAL):
        raise ValueError('有未完成的彻底清理，请使用相同命令重试 uninstall --purge')
    if present(ROOT / JOURNAL):
        raise ValueError('有未结束的组件切换，请先执行 install.py recover')
    job = read_json(ROOT / 'lifecycle-job.json')
    if (job and job.get('status') in ('running', 'paused', 'cleaning')) or present(pathlib.Path('/var/lib/node-manager-cleanup')):
        raise ValueError('节点卸载尚未完成，请先恢复生命周期操作')


def maintenance(binding, action, token):
    try:
        result = json.loads(request(binding['server'], '/api/management-agent/v1/maintenance',
                                    {'action': action, 'token': token}, binding['credential']))
    except urllib.error.HTTPError as error:
        raise ValueError(f'后台拒绝维护请求（HTTP {error.code}）；请核实绑定、任务及后台版本') from None
    if type(result.get('last_seen')) is not int or type(result.get('owned')) is not bool:
        raise ValueError('后台维护接口响应不符合要求')
    if action == 'begin' and not result['owned']:
        raise ValueError('后台未授予维护租约')
    return result


def check_agent_unit():
    check_owned_unit(UNITS / SERVICE)
    return unit_info(SERVICE)


def stop_runtimes(record, *, recovering=False):
    check_agent_unit()
    # A refresh may already have rewritten PM's units for the version being undone.
    if not recovering:
        check_pm_units(record['pm_units'])
    if PM_TIMER in record['pm_units']:
        ctl('stop', PM_TIMER)
        wait_for(lambda: unit_info(PM_SERVICE).get('ActiveState') in ('inactive', 'failed'),
                 'PM 正在执行，等待超时，未切换组件文件')
    ctl('stop', SERVICE)


def start_runtimes(record, binding):
    before = maintenance(binding, 'status', record['token'])
    if not before['owned']:
        raise ValueError('后台维护租约已失效')
    ctl('start', SERVICE)
    initial = unit_info(SERVICE)
    pid = initial.get('MainPID')
    def healthy():
        current = unit_info(SERVICE)
        if (current.get('MainPID') != pid or pid in (None, '', '0')
                or current.get('NRestarts') != initial.get('NRestarts')):
            raise ValueError('管理组件进程启动失败或反复重启')
        current_status = maintenance(binding, 'status', record['token'])
        if not current_status['owned']:
            raise ValueError('健康检查期间后台维护租约失效')
        return (current.get('ActiveState') == 'active'
                and current_status['last_seen'] > before['last_seen'])
    wait_for(healthy, '管理组件未在后台完成成功轮询')
    start_pm(record)


def start_pm(record):
    if record['pm_timer']:
        ctl('start', PM_TIMER)
        if unit_info(PM_TIMER).get('ActiveState') != 'active':
            raise ValueError('PM 定时器未恢复')


def read_version(directory, expected):
    owned_directory(directory, True)
    if {path.name for path in directory.iterdir()} != set(expected):
        raise ValueError('版本备份文件集合不符合清单')
    data = {name: owned_file(directory / name, private=True).read_bytes() for name in expected}
    if any(digest(data[name]) != sha for name, sha in expected.items()):
        raise ValueError('版本备份未通过摘要校验')
    return data


def backup_path(backup):
    if not isinstance(backup, str) or not BACKUP_ID.fullmatch(backup):
        raise ValueError('备份编号无效')
    return owned_directory(owned_directory(ROOT / 'component-backups', True) / backup, True)


def record_versions(before, after, incoming, record):
    backups = ROOT / 'component-backups'
    backups.mkdir(mode=0o700, exist_ok=True)
    owned_directory(backups, True)
    backup = backups / record['id']
    staged = backups / ('.creating-' + record['id'])
    staged.mkdir(mode=0o700)
    previous = {name: owned_file(CODE / name).read_bytes() for name in before}
    if any(digest(previous[name]) != sha for name, sha in before.items()):
        raise ValueError('组件文件在备份期间发生变化')
    for version, contents in (('before', previous), ('after', incoming)):
        directory = staged / version
        directory.mkdir(mode=0o700)
        for name, data in contents.items():
            write_file(directory / name, data)
    write_json(staged / 'versions.json', {'before': before, 'after': after})
    os.rename(staged, backup)
    sync(backups)
    return backup


def discard_incomplete_backup(record):
    backup_id = record['id']
    if not BACKUP_ID.fullmatch(backup_id):
        raise ValueError('备份编号无效')
    backups = ROOT / 'component-backups'
    staged = backups / ('.creating-' + backup_id)
    if present(staged):
        owned_directory(backups, True)
        owned_directory(staged, True)
        files, directories = [], []
        for path in staged.iterdir():
            if path.name in ('before', 'after'):
                owned_directory(path, True)
                for item in path.iterdir():
                    if not COMPONENT_FILE.fullmatch(item.name) and not re.fullmatch(r'\.node-manager-[a-z0-9_]{8}', item.name):
                        raise ValueError('未完成的组件备份含未知文件，请保留现场')
                    files.append(owned_file(item, private=True))
                directories.append(path)
            elif path.name == 'versions.json' or re.fullmatch(r'\.node-manager-[a-z0-9_]{8}', path.name):
                files.append(owned_file(path, private=True))
            else:
                raise ValueError('未完成的组件备份含未知文件，请保留现场')
        for path in files:
            remove(path)
        for path in directories:
            path.rmdir()
        staged.rmdir()
        sync(backups)
    remove(ROOT / JOURNAL)


def write_version(expected, data):
    previous = read_json(ROOT / MANIFEST) or {}
    # New agents can refuse an incomplete switch; the old agent cannot.
    for name in sorted(data, key=lambda name: name != 'agent.py'):
        write_file(CODE / name, data[name], 0o644)
    for name in set(previous) - set(data):
        remove(CODE / name)
    write_json(ROOT / MANIFEST, expected)


def refresh_instances():
    """Instance files follow the code that now owns them, or a template change would read as drift."""
    result = subprocess.run((sys.executable, str(CODE / 'agent.py'), 'refresh'), capture_output=True, text=True,
                            stdin=subprocess.DEVNULL, timeout=300, check=False,
                            env={'PATH': '/usr/sbin:/usr/bin:/sbin:/bin', 'LC_ALL': 'C', 'PYTHONDONTWRITEBYTECODE': '1'})
    # 2: argparse in a version that predates refresh, reached only by rolling back to it.
    if result.returncode not in (0, 2):
        raise ValueError('实例配置更新失败：' + (result.stderr.strip().splitlines() or ['未知错误'])[-1][:200])


def managed_drivers():
    sys.path.insert(0, str(CODE))
    from snell import Driver as Snell
    from services import Driver as Service
    snell = Snell()
    snell.binary = CODE / 'snell-server'
    snell.configs = ROOT / 'configs'
    return {driver.binary.name: driver for driver in (snell, Service('sb', ROOT, CODE), Service('fw', ROOT, CODE))}


def check_unmodified():
    """An upgrade rewrites instance files, so a manual edit must surface first."""
    for driver in managed_drivers().values():
        state = read_json(driver.configs.parent / 'state.json', {'installed': False, 'resources': []})
        if not driver.configs_match(state):
            raise ValueError('实例配置被手工修改或与确认状态不一致，请先核实，升级未执行')


def finish(record, binding, phase):
    record['phase'] = phase
    write_json(ROOT / JOURNAL, record)
    maintenance(binding, 'end', record['token'])
    remove(ROOT / JOURNAL)


def check_core_versions(incoming):
    """A component switch must keep understanding the installed core state."""
    versions = {'snell': ('6.0.0', 'snell-server'), 'sb': ('1.14.4', 'sing-box'),
                'fw': ('2.9.6', 'realm')}
    for module, (legacy, filename) in versions.items():
        directory = ROOT if module == 'snell' else ROOT / module
        state = read_json(directory / 'state.json', {})
        if not state.get('installed'):
            continue
        catalog = json.loads(incoming['core_assets.json'])
        record = state.get('core') or {'version': legacy, 'previous_version': None}
        current, previous = record['version'], record['previous_version']
        sha = digest(owned_file(CODE / filename).read_bytes())
        assets = catalog.get(module, {})
        machine = platform.machine()
        expected = assets.get(current, {}).get(machine, {}).get('sha256')
        compatible = sha == expected and (previous is None or machine in assets.get(previous, {}))
        if not compatible or record.get('sha256', sha) != sha:
            raise ValueError(f'目标组件无法确认 {module} 当前核心或回退版本，请先核实核心状态')


def restore_before(record, binding):
    backup = backup_path(record['id'])
    versions = read_json(backup / 'versions.json')
    before = manifest(versions['before'])
    after = manifest(versions['after'])
    old = read_version(backup / 'before', before)
    if binding is not None:
        maintenance(binding, 'begin', record['token'])
    record['phase'] = 'switching'
    write_json(ROOT / JOURNAL, record)
    stop_runtimes(record, recovering=True)
    if binding is None:
        ctl('disable', SERVICE)
    with lock(ROOT / 'agent.lock'):
        ensure_idle()
        check_current(before, after)
        check_core_versions(old)
        write_version(before, old)
        record['phase'] = 'starting'
        write_json(ROOT / JOURNAL, record)
    refresh_instances()
    if binding is None:
        start_pm(record)
        remove(ROOT / JOURNAL)
    else:
        start_runtimes(record, binding)
        finish(record, binding, 'restored')


def resume_unswitched(record, binding):
    check_agent_unit()
    check_pm_units(record['pm_units'])
    if binding is None:
        ctl('disable', '--now', SERVICE)
        start_pm(record)
        remove(ROOT / JOURNAL)
    else:
        maintenance(binding, 'begin', record['token'])
        start_runtimes(record, binding)
        finish(record, binding, 'restored')


def change(binding, before, after, incoming):
    check_current(before)
    if check_agent_unit().get('ActiveState') != 'active':
        raise ValueError('组件未在线，请先恢复管理服务')
    ensure_idle()
    check_core_versions(incoming)
    check_unmodified()
    pm_units, pm_timer = pm_info(before)
    if pm_units and 'pm.py' not in after:
        raise ValueError('PM 服务仍依赖当前组件，无法回退至无 PM 运行时的版本')
    record = {'id': secrets.token_hex(16), 'token': secrets.token_hex(32), 'phase': 'backing_up',
              'pm_units': pm_units, 'pm_timer': pm_timer}
    write_json(ROOT / JOURNAL, record)
    record_versions(before, after, incoming, record)
    record['phase'] = 'prepared'
    write_json(ROOT / JOURNAL, record)
    try:
        maintenance(binding, 'begin', record['token'])
    except Exception:
        # A timed-out request can already have acquired the lease. Keep its
        # token for recover unless we can confirm that no lease remains.
        if not maintenance(binding, 'status', record['token'])['owned']:
            remove(ROOT / JOURNAL)
        raise
    record['phase'] = 'paused'
    write_json(ROOT / JOURNAL, record)
    try:
        stop_runtimes(record)
        with lock(ROOT / 'agent.lock'):
            ensure_idle()
            check_current(before)
            check_core_versions(incoming)
            record['phase'] = 'switching'
            write_json(ROOT / JOURNAL, record)
            write_version(after, incoming)
            record['phase'] = 'starting'
            write_json(ROOT / JOURNAL, record)
        refresh_instances()
        start_runtimes(record, binding)
    except Exception as error:
        try:
            if record['phase'] == 'paused':
                resume_unswitched(record, binding)
            else:
                restore_before(record, binding)
        except Exception:
            raise ValueError(f'组件切换失败且未完成回退，备份 {record["id"]}；请检查日志后执行 recover') from None
        raise ValueError(f'组件切换失败，已恢复原版本；备份 {record["id"]}（{reason(error)}）') from None
    finish(record, binding, 'ready')
    print(f'管理组件已完成切换并在后台轮询成功，回退备份编号 {record["id"]}')


def install(args):
    ensure_no_journal()
    server = base_url(args.server)
    existing = read_json(ROOT / 'binding.json')
    if existing and (existing['server'] != server or existing['node'] != args.node):
        raise ValueError('已绑定其他后台或节点，请先 unlink')
    check_owned_unit(UNITS / SERVICE)
    ctl('daemon-reload')
    unit_info(SERVICE)
    for directory in (ROOT, *(ROOT / module for module in MODULES)):
        if present(directory):
            owned_directory(directory, True)
            if present(directory / 'recovery.json'):
                raise ValueError('有未恢复操作，请先 recover')
    enrollment = json.loads(sys.stdin.read(4096))['enrollment']
    pending = read_json(ROOT / 'enrollment.json')
    if pending and (pending['server'] != server or pending['node'] != args.node):
        raise ValueError('存在其他后台或节点的待核实登记，请先核实原登记')
    expected, data = download(server)
    previous = read_json(ROOT / MANIFEST)
    if previous is not None and previous != expected:
        raise ValueError('已有组件与当前版本不同，安装入口不执行升级')
    check_current(expected, allow_missing=True)
    # Matching live files need no replacement during retries and rebinds.
    for name in sorted(data):
        if not present(CODE / name):
            write_file(CODE / name, data[name], 0o644)
    write_json(ROOT / MANIFEST, expected)
    if existing is None:
        # A replacement authorization code must keep the identity already sent
        # to this node's Hub, including after a lost response or local write.
        binding = ({**pending, 'enrollment': enrollment} if pending else
                   {'server': server, 'node': args.node, 'credential': secrets.token_hex(32), 'enrollment': enrollment})
        write_json(ROOT / 'enrollment.json', binding)
        request(server, '/api/management-agent/v1/enroll',
                {key: binding[key] for key in ('node', 'enrollment', 'credential')}, binding['credential'])
        write_json(ROOT / 'binding.json', {key: binding[key] for key in ('server', 'node', 'credential')})
    remove(ROOT / 'enrollment.json')
    write_file(UNITS / SERVICE, SERVICE_UNIT.encode(), 0o644)
    ctl('daemon-reload')
    binding = read_json(ROOT / 'binding.json')
    expected_confirmation = {'server': binding['server'], 'node': binding['node'],
                             'credential_sha256': digest(binding['credential'].encode()), 'nonce': secrets.token_hex(16)}
    # A new nonce proves a successful Hub poll after this installation attempt.
    write_json(ROOT / 'connection-request.json', expected_confirmation)
    ctl('enable', '--now', SERVICE)
    ctl('is-active', '--quiet', SERVICE)
    wait_for(lambda: read_json(ROOT / 'connection-confirmed.json') == expected_confirmation,
             '管理组件已安装并保留原身份，但首次上报结果未确认；请检查后台与日志，使用原安装命令重试', timeout=35)
    remove(ROOT / 'connection-request.json')
    remove(ROOT / 'connection-confirmed.json')
    place_command()
    print('管理组件已安装，节点授权及后台首次上报已确认。以后在本机运行 sudo nodehelm 可升级和查看状态。')


def check_no_pm_table():
    try:
        result = subprocess.run(('nft', '-j', '-n', 'list', 'tables'),
                                capture_output=True, text=True, stdin=subprocess.DEVNULL,
                                timeout=15, env={'PATH': '/usr/sbin:/usr/bin:/sbin:/bin', 'LC_ALL': 'C'})
    except (OSError, subprocess.TimeoutExpired):
        raise ValueError('无法执行 nftables，不能确认 PM 表已移除') from None
    if result.returncode:
        raise ValueError('无法确认 nftables 状态，请检查本机 nftables')
    try:
        tables = json.loads(result.stdout)['nftables']
        if not isinstance(tables, list):
            raise ValueError()
        for row in tables:
            if not isinstance(row, dict) or len(row) != 1:
                raise ValueError()
            if 'metainfo' in row:
                continue
            table = row['table']
            if not isinstance(table['family'], str) or not isinstance(table['name'], str):
                raise ValueError()
    except (ValueError, KeyError, TypeError):
        raise ValueError('无法识别 nftables 表清单') from None
    if any((row.get('table', {}).get('family'), row.get('table', {}).get('name'))
           in (('ip', 'node_manager_pm'), ('inet', 'node_manager_pm_block')) for row in tables):
        raise ValueError('仍存在受管 PM 表，请先执行 pm.py disable-empty')


def check_purge_runtimes(check_pm):
    check_pm_units({})
    patterns = tuple(f'node-manager-{module}-*.service' for module in ('snell', 'sb', 'fw'))
    for operation in ('list-unit-files', 'list-units'):
        result = ctl(operation, '--all', '--no-legend', '--plain', '--type=service')
        names = (row.split()[0] for row in result.stdout.splitlines() if row.strip())
        if any(fnmatch.fnmatchcase(name, pattern) for name in names for pattern in patterns):
            raise ValueError('仍存在受管实例的服务文件或已加载单元，拒绝移除业务核心')
    if check_pm:
        check_no_pm_table()


def purge_plan(*, lifecycle=False, preflight=False):
    current = installed_manifest()
    check_current(current)
    check_pm = present(ROOT / 'pm' / 'owner.json')
    if not preflight:
        check_purge_runtimes(check_pm)
    drivers = managed_drivers()
    for driver in drivers.values():
        state = read_json(driver.configs.parent / 'state.json', {'installed': False, 'resources': []})
        if state['resources'] and not preflight:
            raise ValueError('请先在后台删除所有受管实例')
        if hasattr(driver, 'require_core'):
            driver.require_core(state)
        elif present(driver.binary) and (
            not state['installed'] or digest(owned_file(driver.binary).read_bytes()) != driver.expected_hash()
        ):
            raise ValueError(f'核心 {driver.binary.name} 无法确认归属，拒绝删除')
    module_files = {'state.json', 'dedup-key.json', 'write.lock', 'last-error.json'}
    files, directories = [], []

    def validate(directory, allowed, private=True):
        owned_directory(directory, private)
        contents = sorted(directory.iterdir())
        for path in contents:
            if not allowed(path):
                raise ValueError(f'发现未登记文件 {path.name}，拒绝彻底清理')
        directories.append(directory)
        return contents

    def leaf(path, private=True):
        owned_file(path, private=private)
        files.append(path)

    for path in validate(CODE, lambda p: p.name in set(current) | set(drivers) | {'__pycache__'}, False):
        if path.name == '__pycache__':
            known_pyc = re.compile(r'(?:' + '|'.join(re.escape(name[:-3]) for name in current if name.endswith('.py'))
                                   + r')\.cpython-[0-9]+(?:\.opt-[12])?\.pyc')
            for cached in validate(path, lambda p: known_pyc.fullmatch(p.name), False):
                leaf(cached, False)
        else:
            leaf(path, False)

    def module_tree(directory, module=None):
        if module is None and present(directory / 'lifecycle-job.json'):
            job = read_json(directory / 'lifecycle-job.json')
            if (not isinstance(job, dict) or not isinstance(job.get('id'), str) or not BACKUP_ID.fullmatch(job['id'])
                    or (not lifecycle and (job.get('status') != 'succeeded' or job.get('stage') != 'finished'))):
                raise ValueError('节点卸载记录尚未确认，拒绝彻底清理')
        state = read_json(directory / 'state.json', {'installed': False, 'resources': []})
        if not preflight and (state['resources'] or (module == 'pm' and state['installed'])):
            raise ValueError('请先在后台删除所有受管实例和 PM 规则，再执行 pm.py disable-empty')
        extra = ({MANIFEST, 'agent.lock', 'lifecycle.lock', 'connection-confirmed.json', 'connection-request.json', 'lifecycle-job.json', 'component-backups', *MODULES}
                 | ({'binding.json', 'lifecycle-job.json'} if lifecycle else set()) if module is None
                 else {'owner.json', 'checkpoint.json'} if module == 'pm' else set())
        for path in validate(directory, lambda p: p.name in module_files | extra | {'configs', 'receipts'}):
            if path.name == 'configs':
                names = {str(item['port']) + '.conf' for item in state['resources']} if preflight else set()
                for config in validate(path, lambda item: item.name in names):
                    leaf(config)
            elif path.name == 'receipts':
                for receipt in validate(path, lambda p: re.fullmatch(r'[0-9a-f]{32}\.json', p.name)):
                    leaf(receipt)
            elif path.name in MODULES:
                module_tree(path, path.name)
            elif path.name == 'component-backups':
                for backup in validate(path, lambda p: BACKUP_ID.fullmatch(p.name)):
                    versions = read_json(backup / 'versions.json')
                    for version in validate(backup, lambda p: p.name in ('versions.json', 'before', 'after')):
                        if version.name == 'versions.json':
                            leaf(version)
                            continue
                        expected = manifest(versions[version.name])
                        read_version(version, expected)
                        for item in validate(version, lambda p: p.name in expected):
                            leaf(item)
            else:
                leaf(path)

    module_tree(ROOT)
    return {'files': [purge_entry(path) for path in files],
            'directories': [purge_entry(path) for path in reversed(directories)],
            'check_pm': check_pm}


def purge_entry(path):
    info = path.lstat()
    identity = [info.st_dev, info.st_ino]
    if stat.S_ISREG(info.st_mode):
        identity.extend((info.st_size, info.st_mtime_ns))
    return [str(path), identity]


def check_purge_entry(entry, directory):
    path = pathlib.Path(entry[0])
    if '..' in path.parts or not any(path.is_relative_to(base) for base in (ROOT, CODE)):
        raise ValueError('彻底清理记录中的路径无效')
    if present(path):
        if directory:
            owned_directory(path, path.is_relative_to(ROOT))
        else:
            owned_file(path, private=path.is_relative_to(ROOT))
        if purge_entry(path) != entry:
            raise ValueError(f'{path.name} 在清理期间被替换，请保留现场核实')
    return path


def remove_agent_service():
    if present(UNITS / SERVICE):
        ctl('disable', SERVICE)
        remove(UNITS / SERVICE)
        ctl('daemon-reload')
        check_agent_unit()


def finish_purge(plan):
    # Validate directories from the outside in before examining any leaf.
    directories = [check_purge_entry(entry, True) for entry in reversed(plan['directories'])]
    files = [check_purge_entry(entry, False) for entry in plan['files']]
    expected = set(directories) | set(files)
    # Reload first so a crash after unlinking the unit cannot leave a stale
    # systemd fragment that prevents a retry.
    ctl('daemon-reload')
    check_agent_unit()
    if present(UNITS / SERVICE):
        ctl('stop', SERVICE)
        remove_agent_service()
    check_purge_runtimes(plan['check_pm'])
    local_locks = (ROOT / 'lifecycle.lock', ROOT / 'agent.lock')
    with contextlib.ExitStack() as held:
        for path in local_locks:
            if present(path):
                held.enter_context(lock(path))
        for directory in directories:
            if present(directory) and set(directory.iterdir()) - expected:
                raise ValueError(f'{directory.name} 出现未登记内容，拒绝继续彻底清理')
        for path in files:
            if path not in local_locks:
                remove(path)
        for path in reversed(directories):
            if path != ROOT and present(path):
                path.rmdir()
                sync(path.parent)
    # The stable outer lifecycle lock covers removal of the two local locks.
    for path in local_locks:
        if path in files:
            remove(path)
    remove_command()
    if present(ROOT):
        ROOT.rmdir()
        sync(ROOT.parent)
    remove(PURGE_JOURNAL)
    print('已清理已核实归属的组件文件、业务核心和私有状态。')


def uninstall(args):
    if args.purge and present(PURGE_JOURNAL):
        return finish_purge(read_json(PURGE_JOURNAL))
    ensure_no_journal()
    with lock(ROOT / 'lifecycle.lock'):
        ensure_idle()
        if present(ROOT / 'binding.json'):
            raise ValueError('请先在后台撤销绑定，再执行 agent.py unlink')
        ctl('daemon-reload')
        check_agent_unit()
        if present(UNITS / SERVICE):
            ctl('stop', SERVICE)
        with lock(ROOT / 'agent.lock'):
            ensure_idle()
            if args.purge:
                plan = purge_plan()
                write_json(PURGE_JOURNAL, plan)
            else:
                remove_agent_service()
    if args.purge:
        finish_purge(plan)
    else:
        print('已移除管理轮询服务；保留业务核心、服务、状态和管理组件文件。')


def reason(error):
    if isinstance(error, urllib.error.HTTPError):
        return f'HTTP {error.code}'
    # JSONDecodeError and SyntaxError may include downloaded source or values.
    return str(error) if type(error) is ValueError else type(error).__name__


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='action', required=True)
    fresh = commands.add_parser('install')
    fresh.add_argument('--server', required=True)
    fresh.add_argument('--node', type=int, required=True)
    commands.add_parser('upgrade')
    rollback = commands.add_parser('rollback')
    rollback.add_argument('--backup', required=True)
    recover = commands.add_parser('recover')
    recover.add_argument('--offline', action='store_true')
    uninstall_command = commands.add_parser('uninstall')
    uninstall_command.add_argument('--purge', action='store_true')
    if argv and argv[0] in ('--server', '--node'):
        argv.insert(0, 'install')  # Existing panel-generated command.
    args = parser.parse_args(argv)
    if os.geteuid() != 0 or not pathlib.Path('/run/systemd/system').is_dir():
        raise ValueError('请在 Linux systemd VPS 上使用 root 执行')
    os.umask(0o077)
    sys.dont_write_bytecode = True
    with lock(LIFECYCLE_LOCK):
        if args.action == 'uninstall' and args.purge and present(PURGE_JOURNAL):
            return uninstall(args)
        if present(PURGE_JOURNAL):
            raise ValueError('有未完成的彻底清理，请使用相同命令重试 uninstall --purge')
        if args.action == 'install':
            prepare_directories(ROOT, CODE)
        else:
            owned_directory(ROOT, True)
            owned_directory(CODE, False)
        if args.action == 'uninstall':
            return uninstall(args)
        with lock(ROOT / 'lifecycle.lock'):
            return component_action(args)


def component_action(args):
    if args.action == 'install':
        return install(args)
    binding = read_json(ROOT / 'binding.json')
    if args.action == 'recover':
        record = read_json(ROOT / JOURNAL)
        if record is None:
            print('没有待恢复的组件切换')
        elif record['phase'] == 'backing_up':
            discard_incomplete_backup(record)
            print('已清理未完成的组件版本备份')
        elif not binding and not args.offline:
            raise ValueError('组件身份不可用，请使用 recover --offline')
        elif record['phase'] == 'paused':
            resume_unswitched(record, None if args.offline else binding)
            print('已恢复切换前运行状态' + ('；轮询已停用' if args.offline else ''))
        elif record['phase'] in ('prepared', 'ready', 'restored'):
            if args.offline:
                check_agent_unit()
                ctl('disable', '--now', SERVICE)
            elif maintenance(binding, 'status', record['token'])['owned']:
                maintenance(binding, 'end', record['token'])
            remove(ROOT / JOURNAL)
            print('已结束组件维护' + ('；轮询已停用' if args.offline else ''))
        elif record['phase'] in ('switching', 'starting'):
            restore_before(record, None if args.offline else binding)
            print('已恢复切换前组件，' + ('轮询保持停用' if args.offline else '后台轮询已确认'))
        else:
            raise ValueError('发现未知组件恢复阶段，请保留现场')
        return
    if not binding:
        raise ValueError('组件尚未绑定')
    ensure_no_journal()
    ensure_idle()
    if args.action == 'upgrade':
        before = installed_manifest()
        check_current(before)
        after, incoming = download(binding['server'])
        if before == after:
            print('已安装当前后台提供的组件版本')
            return
    else:
        before = installed_manifest()
        backup = backup_path(args.backup)
        versions = read_json(backup / 'versions.json')
        if before != manifest(versions['after']):
            raise ValueError('该备份与当前安装版本不匹配')
        after = manifest(versions['before'])
        incoming = read_version(backup / 'before', after)
    change(binding, before, after, incoming)


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(f'管理组件操作未完成：{reason(error)}', file=sys.stderr)
        sys.exit(1)
