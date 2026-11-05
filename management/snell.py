"""Owned Snell 6 services only. Never sources interactive scripts or changes firewall."""
import contextlib
import errno
import fnmatch
import hashlib
import http.client
import json
import os
import pathlib
import platform
import re
import socket
import stat
import subprocess
import tarfile
import tempfile
import time
import urllib.parse
import urllib.request
import zipfile
from model import Rejected
from storage import sync

ASSETS = json.loads(pathlib.Path(__file__).with_name('core_assets.json').read_text())
LEGACY_VERSION = {'snell': '6.0.0', 'sb': '1.14.4', 'fw': '2.9.6'}
MAX_ARCHIVE = 96 * 1024 * 1024
MAX_BINARY = 160 * 1024 * 1024
SNAPSHOT_CACHE_MIN_CTIME_AGE_NS = 1_000_000_000
SNAPSHOT_CACHE_TTL_SECONDS = 60


class ReleaseRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        url = urllib.parse.urlsplit(newurl)
        if url.scheme != 'https' or url.hostname not in ('github.com', 'release-assets.githubusercontent.com',
                                                          'objects.githubusercontent.com'):
            raise Rejected('核心下载重定向不在官方发行域名范围', 'core_download')
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def stream(source, target, limit, deadline=None):
    sha = hashlib.sha256()
    size = 0
    while block := source.read(1024 * 1024):
        size += len(block)
        if size > limit or (deadline is not None and time.monotonic() > deadline):
            raise Rejected('核心文件超过大小或下载时限', 'core_download')
        sha.update(block)
        if target is not None:
            target.write(block)
    return sha.hexdigest()


def binary_identity(meta):
    if (not stat.S_ISREG(meta.st_mode) or meta.st_uid != os.geteuid()
            or meta.st_mode & 0o022 or meta.st_nlink != 1):
        raise Rejected('核心文件归属或权限无法核实', 'config_changed')
    return (meta.st_dev, meta.st_ino, meta.st_size, meta.st_mtime_ns, meta.st_ctime_ns,
            meta.st_mode, meta.st_uid, meta.st_nlink)


def binary_hash(path, *, expected_identity=None):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return None
    except OSError as error:
        raise Rejected('无法核实核心文件归属', 'config_changed') from error
    with os.fdopen(fd, 'rb') as source:
        before = binary_identity(os.fstat(fd))
        if expected_identity is not None and before != expected_identity:
            raise Rejected('核心文件在校验期间发生变化', 'config_changed')
        sha = stream(source, None, MAX_BINARY)
        if expected_identity is not None and binary_identity(os.fstat(fd)) != expected_identity:
            raise Rejected('核心文件在校验期间发生变化', 'config_changed')
        return sha


def command(*args, check=True):
    result = subprocess.run(args, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, timeout=30, check=False,
                            env={'PATH': '/usr/sbin:/usr/bin:/sbin:/bin', 'LC_ALL': 'C'})
    if check and result.returncode:
        raise Rejected('系统服务操作失败，请检查对应 node-manager 服务日志')
    return result


def atomic(path, content, mode=0o600):
    path = pathlib.Path(path)
    if path.is_symlink():
        raise Rejected('发现符号链接，拒绝覆盖')
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix='.node-manager-')
    try:
        with os.fdopen(fd, 'wb') as f:
            f.write(content)
            f.flush()
            os.fchmod(f.fileno(), mode)
            os.fsync(f.fileno())
        os.replace(tmp, path)
        sync(path.parent)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


class Driver:
    binary = pathlib.Path('/opt/node-manager/snell-server')
    configs = pathlib.Path('/var/lib/node-manager/configs')
    units = pathlib.Path('/etc/systemd/system')

    module = 'snell'
    _snapshot_core = None

    @staticmethod
    def version_key(version):
        return tuple(map(int, version.split('.')))

    def supported(self):
        return platform.machine() in ASSETS[self.module][LEGACY_VERSION[self.module]]

    def asset(self, version):
        try:
            return ASSETS[self.module][version][platform.machine()]
        except (KeyError, TypeError):
            raise Rejected('当前架构没有所选核心版本的可信资产', 'core_version') from None

    def expected_hash(self):
        return self.asset(LEGACY_VERSION[self.module])['sha256']

    def versions(self):
        return sorted((v for v, assets in ASSETS[self.module].items() if platform.machine() in assets),
                      key=self.version_key)

    def identity(self, state):
        if not state['installed']:
            return None
        if 'core' not in state:
            return {'version': LEGACY_VERSION[self.module], 'sha256': self.expected_hash(),
                    'previous_version': None}
        current = state['core']
        if (not isinstance(current, dict) or set(current) != {'version', 'sha256', 'previous_version'}
                or current['sha256'] != self.asset(current['version'])['sha256']):
            raise Rejected('本机记录的核心版本或摘要无法确认', 'core_version')
        previous = current['previous_version']
        if previous is not None:
            self.asset(previous)
            if self.version_key(previous) >= self.version_key(current['version']):
                raise Rejected('回退版本记录无效', 'core_version')
        return current

    def require_core(self, state):
        current = self.identity(state)
        actual = binary_hash(self.binary)
        if actual != (current['sha256'] if current else None):
            raise Rejected('本机核心文件与可信安装状态不一致', 'config_changed')
        return current

    def core_status(self, state):
        empty = {'version': None, 'sha256': None, 'previous_version': None, 'candidates': []}
        try:
            current = self.identity(state)
            if current is None:
                self._snapshot_core = None
                try:
                    self.binary.lstat()
                except FileNotFoundError:
                    return empty, True
                return empty, False
            meta = self.binary.lstat()
            fingerprint = binary_identity(meta)
            key = (fingerprint, current['sha256'])
            # Same-tick writes can leave ctime unchanged. Only cache a hash
            # first read after the timestamp has aged out of that window.
            settled = time.time_ns() - meta.st_ctime_ns >= SNAPSHOT_CACHE_MIN_CTIME_AGE_NS
            now = time.monotonic()
            cached = self._snapshot_core
            if not settled or cached is None or cached[0] != key or now >= cached[1]:
                if (binary_hash(self.binary, expected_identity=fingerprint) != current['sha256']
                        or binary_identity(self.binary.lstat()) != fingerprint):
                    raise Rejected('本机核心文件与可信安装状态不一致', 'config_changed')
                self._snapshot_core = (key, now + SNAPSHOT_CACHE_TTL_SECONDS) if settled else None
        except (Rejected, OSError):
            self._snapshot_core = None
            return empty, False
        return {**current, 'candidates': [v for v in self.versions()
                                         if self.version_key(v) > self.version_key(current['version'])]}, True

    def core_directory(self, backup):
        name, device, inode = backup['stage']
        if not re.fullmatch(rf'\.node-manager-{re.escape(self.binary.name)}-[a-z0-9_]{{8}}', name):
            raise Rejected('核心暂存目录记录无效', 'config_changed')
        directory = self.binary.parent / name
        try:
            meta = directory.lstat()
        except FileNotFoundError:
            return None
        if (not stat.S_ISDIR(meta.st_mode) or meta.st_uid != os.geteuid()
                or meta.st_mode & 0o077 or (meta.st_dev, meta.st_ino) != (device, inode)):
            raise Rejected('核心暂存目录归属发生变化', 'config_changed')
        return directory

    def unit_pattern(self):
        return f'node-manager-{self.module}-*.service'

    def inspect_inventory(self, state):
        ports = {r['port'] for r in state['resources']}
        configs = {self.files(port)[0] for port in ports}
        unit_names = {self.unit(port) for port in ports}
        try:
            self._inspect_inventory(state, configs, unit_names)
        except (OSError, subprocess.TimeoutExpired, UnicodeError):
            raise Rejected('无法核实实例文件或系统服务归属', 'config_changed') from None

    def _inspect_inventory(self, state, configs, unit_names):
        config_dir = self.configs.lstat()
        code_dir = self.binary.parent.lstat()
        if (not stat.S_ISDIR(config_dir.st_mode) or config_dir.st_uid != os.geteuid()
                or config_dir.st_mode & 0o077 or set(self.configs.iterdir()) != configs
                or not stat.S_ISDIR(code_dir.st_mode) or code_dir.st_uid != os.geteuid()
                or code_dir.st_mode & 0o022):
            raise Rejected('检测到未登记的实例配置，拒绝变更核心', 'config_changed')
        if ({p.name for p in self.units.glob(self.unit_pattern())} - unit_names
                or any(self.units.glob(self.unit_pattern() + '.d'))):
            raise Rejected('检测到未登记的实例服务文件，拒绝变更核心', 'config_changed')
        for operation in ('list-unit-files', 'list-units'):
            result = command('systemctl', operation, '--all', '--no-legend', '--plain', '--type=service')
            names = (row.split()[0] for row in result.stdout.decode().splitlines() if row.strip())
            if any(fnmatch.fnmatchcase(name, self.unit_pattern()) and name not in unit_names for name in names):
                raise Rejected('检测到未登记的实例单元，拒绝变更核心', 'config_changed')
        if not self.configs_match(state):
            raise Rejected('实例配置或系统服务归属发生变化', 'config_changed')

    def runtime_ports(self, state):
        states = self.runtime(state)
        if set(states) != {str(r['port']) for r in state['resources']} or any(
                value not in ('active', 'inactive', 'failed') for value in states.values()):
            raise Rejected('实例运行状态正在变化或无法核实', 'config_changed')
        return sorted(int(port) for port, value in states.items() if value == 'active')

    def prepare_core(self, action, before, payload):
        self.require_platform()
        current = self.require_core(before)
        if action == 'install':
            if current is not None:
                raise Rejected('核心已安装，请选择升级操作', 'core_version')
            version, previous = self.versions()[-1], None
        else:
            if current is None:
                raise Rejected('请先安装所选模块核心', 'core_version')
            if action == 'core_uninstall':
                if before['resources']:
                    raise Rejected('请先删除所有受管实例', 'core_in_use')
                version, previous = None, None
            else:
                version = payload['version']
                self.asset(version)
                if action == 'core_rollback':
                    if version != current['previous_version']:
                        raise Rejected('请选择最近一次升级前的可信版本', 'core_version')
                    previous = None
                else:
                    if self.version_key(version) <= self.version_key(current['version']):
                        raise Rejected('请选择较新的可信版本', 'core_version')
                    previous = current['version']
        self.inspect_inventory(before)
        active = self.runtime_ports(before)
        target = {'version': version, 'sha256': self.asset(version)['sha256'],
                  'previous_version': previous} if version is not None else None
        after = {**before, 'installed': target is not None}
        if target is None:
            after.pop('core', None)
        else:
            after['core'] = target
        return after, {'kind': 'core', 'before_hash': current['sha256'] if current else None,
                       'after_hash': target['sha256'] if target else None, 'active': active,
                       'mutation_started': False}

    @contextlib.contextmanager
    def stage_core(self, after, backup):
        with contextlib.ExitStack() as context:
            candidate = None
            if after['installed']:
                asset = self.asset(after['core']['version'])
                try:
                    candidate = context.enter_context(tempfile.TemporaryFile())
                    self.download(asset, candidate)
                except (OSError, EOFError, ValueError, KeyError, zipfile.BadZipFile,
                        tarfile.TarError, http.client.HTTPException) as error:
                    if isinstance(error, Rejected):
                        raise
                    raise Rejected(f'核心下载或解包失败（{type(error).__name__}）', 'core_download') from None
            # Exclusive allocation occurs after the download. Record the exact
            # directory identity in the journal before writing staged files.
            directory = pathlib.Path(tempfile.mkdtemp(dir=self.binary.parent,
                                                      prefix=f'.node-manager-{self.binary.name}-'))
            meta = directory.lstat()
            backup['stage'] = [directory.name, meta.st_dev, meta.st_ino]
            try:
                sync(directory.parent)
                yield candidate
            finally:
                if self.core_directory(backup) is not None:
                    try:
                        directory.rmdir()
                    except OSError as error:
                        if error.errno != errno.ENOTEMPTY:
                            raise
                    else:
                        sync(directory.parent)

    def download(self, asset, candidate):
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), ReleaseRedirect())
        request = urllib.request.Request(asset['url'], headers={'User-Agent': 'NodeManager/1'})
        with tempfile.TemporaryFile() as package, opener.open(request, timeout=30) as response:
            if self.module == 'snell' and response.url != asset['url']:
                raise Rejected('Snell 核心下载发生非预期跳转', 'core_download')
            if stream(response, package, MAX_ARCHIVE, deadline=time.monotonic() + 120) != asset['archive_sha256']:
                raise Rejected('核心安装包摘要校验失败', 'core_download')
            package.seek(0)
            if self.module == 'snell':
                with zipfile.ZipFile(package) as archive:
                    item = archive.getinfo(asset['member'])
                    if stat.S_IFMT(item.external_attr >> 16) not in (0, stat.S_IFREG):
                        raise Rejected('核心安装包内容无效', 'core_download')
                    with archive.open(item) as member:
                        sha = stream(member, candidate, MAX_BINARY)
            else:
                with tarfile.open(fileobj=package, mode='r:gz') as archive:
                    item = archive.getmember(asset['member'])
                    if not item.isfile() or item.size > MAX_BINARY:
                        raise Rejected('核心安装包内容无效', 'core_download')
                    with archive.extractfile(item) as member:
                        sha = stream(member, candidate, MAX_BINARY)
        if sha != asset['sha256']:
            raise Rejected('核心二进制摘要校验失败', 'core_download')
        candidate.seek(0)

    def apply_core(self, before, after, backup, candidate, begin_mutation):
        self.require_core(before)
        self.inspect_inventory(before)
        directory = self.core_directory(backup)
        if directory is None:
            raise Rejected('核心暂存目录缺失', 'config_changed')
        staged, saved = directory / 'candidate', directory / 'backup'
        if candidate is not None:
            self.write_core_file(staged, candidate, backup['after_hash'])
            self.check_core_candidate(after, staged)
        if before['installed']:
            with self.binary.open('rb') as original:
                self.write_core_file(saved, original, backup['before_hash'])
        self.inspect_inventory(before)
        if self.runtime_ports(before) != backup['active']:
            raise Rejected('下载期间实例运行状态发生变化，请刷新后重试', 'config_changed')
        self.require_core(before)
        begin_mutation()
        for port in backup['active']:
            command('systemctl', 'stop', self.unit(port))
        if self.runtime_ports(before):
            raise Rejected('仍有实例运行，无法切换核心', 'core_runtime')
        if candidate is None:
            self.binary.unlink()
        else:
            os.replace(staged, self.binary)
            sync(directory)
        sync(self.binary.parent)
        for port in backup['active']:
            self.start_core_instance(port)

    def start_core_instance(self, port):
        unit = self.unit(port)
        try:
            command('systemctl', 'start', unit)
        except Rejected:
            result = command('systemctl', 'show', '--property=Result', '--value', unit, check=False)
            if (result.returncode or result.stdout.strip() != b'start-limit-hit'
                    or not self.unit_matches(port, self.unit_metadata([port])[port], True)):
                raise
            command('systemctl', 'reset-failed', unit)
            command('systemctl', 'start', unit)

    def check_core_candidate(self, state, executable):
        version_arg = 'version' if self.module == 'sb' else '--version'
        version = re.escape(state['core']['version']).encode()
        try:
            result = command(str(executable), version_arg, check=False)
        except (OSError, subprocess.TimeoutExpired):
            raise Rejected('候选核心无法在当前系统运行', 'core_runtime') from None
        if result.returncode or not re.search(rb'(?<![0-9.])' + version + rb'(?![0-9.])',
                                              result.stdout + result.stderr):
            raise Rejected('候选核心无法运行或报告版本不符', 'core_runtime')

    def verify_core(self, state, backup):
        self.require_core(state)
        self.inspect_inventory(state)
        if backup['active']:
            time.sleep(0.1)
        if self.runtime_ports(state) != backup['active']:
            raise Rejected('核心切换后实例运行状态不一致', 'core_runtime')
        try:
            for port in backup['active']:
                with socket.create_connection(('127.0.0.1', port), timeout=2):
                    pass
        except OSError:
            raise Rejected('核心切换后实例未正常监听', 'core_runtime') from None

    def restore_core(self, backup, before):
        self.inspect_inventory(before)
        started = backup.get('mutation_started')
        if started is False:
            self.require_core(before)
            return
        current = binary_hash(self.binary)
        running = self.runtime_ports(before)
        if started is None and current == backup['before_hash'] and running != backup['active']:
            raise Rejected('旧恢复记录无法确认服务启停归属，请先核实运行状态', 'config_changed')
        if set(running) - set(backup['active']):
            raise Rejected('存在操作前未运行的活动实例，需人工核实', 'config_changed')
        if current != backup['before_hash']:
            if current != backup['after_hash']:
                raise Rejected('核心文件不属于本次切换，拒绝覆盖', 'config_changed')
            directory = self.core_directory(backup)
            if backup['before_hash'] is not None:
                if directory is None or binary_hash(directory / 'backup') != backup['before_hash']:
                    raise Rejected('原核心备份未通过校验，拒绝恢复', 'config_changed')
            for port in running:
                command('systemctl', 'stop', self.unit(port))
            if self.runtime_ports(before):
                raise Rejected('仍有实例运行，无法恢复原核心', 'core_runtime')
            if backup['before_hash'] is None:
                self.binary.unlink()
            else:
                os.replace(directory / 'backup', self.binary)
                sync(directory)
            sync(self.binary.parent)
            running = []
        for port in sorted(set(backup['active']) - set(running)):
            self.start_core_instance(port)
        self.verify_core(before, backup)

    @staticmethod
    def write_core_file(path, source, digest):
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
        with os.fdopen(os.open(path, flags, 0o600), 'wb') as output:
            if stream(source, output, MAX_BINARY) != digest:
                raise Rejected('核心内容发生变化，拒绝切换', 'config_changed')
            output.flush()
            os.fchmod(output.fileno(), 0o755)
            os.fsync(output.fileno())
        sync(path.parent)

    def clean_core(self, backup):
        directory = self.core_directory(backup)
        if directory is None:
            return
        paths = set(directory.iterdir())
        if paths - {directory / 'candidate', directory / 'backup'}:
            raise Rejected('核心暂存目录含未知文件，拒绝清理', 'config_changed')
        for path in paths:
            binary_hash(path)
            path.unlink()
        directory.rmdir()
        sync(directory.parent)

    def unit(self, port):
        return f'node-manager-snell-{port}.service'

    def files(self, port):
        return (self.configs / f'{port}.conf', self.units / self.unit(port))

    def running(self, port):
        return command('systemctl', 'is-active', '--quiet', self.unit(port), check=False).returncode == 0

    def enabled(self, port):
        value = command('systemctl', 'is-enabled', self.unit(port), check=False)
        text = value.stdout.decode().strip()
        if text not in ('enabled', 'disabled', 'not-found', ''):
            raise Rejected('服务启用状态不受支持，拒绝修改')
        if not text and value.returncode not in (0, 1, 4):
            raise Rejected('无法读取服务状态')
        return text == 'enabled'

    def runtime(self, state):
        ports = [r['port'] for r in state['resources']]
        if not ports:
            return {}
        states = {str(port): 'unknown' for port in ports}
        try:
            result = command('systemctl', 'show', '--property=Id,ActiveState', *[self.unit(port) for port in ports], check=False)
            if result.returncode:
                return states
            units = {self.unit(port): str(port) for port in ports}
            for block in result.stdout.decode().split('\n\n'):
                fields = dict(line.split('=', 1) for line in block.splitlines() if '=' in line)
                port = units.get(fields.get('Id'))
                active = fields.get('ActiveState')
                if port and active in ('active', 'inactive', 'failed', 'activating', 'deactivating'):
                    states[port] = active
        except (OSError, subprocess.TimeoutExpired, ValueError):
            pass  # Unknown remains explicit; never pretend a failed query means stopped.
        return states

    def unit_metadata(self, ports):
        units = {self.unit(port): port for port in ports}
        if not units:
            return {}
        result = command('systemctl', 'show', '--all', '--property=Id,LoadState,FragmentPath,DropInPaths',
                         *units, check=False)
        info = {}
        for block in result.stdout.decode().split('\n\n'):
            fields = dict(line.split('=', 1) for line in block.splitlines() if '=' in line)
            unit = fields.get('Id')
            if unit in units and {'LoadState', 'FragmentPath', 'DropInPaths'} <= fields.keys():
                info[units[unit]] = fields
        if len(info) != len(units):
            raise Rejected('无法核实系统服务归属', 'config_changed')
        return info

    def unit_matches(self, port, info, owned):
        if info['DropInPaths']:
            return False
        if owned:
            return info['LoadState'] == 'loaded' and info['FragmentPath'] == str(self.files(port)[1])
        return info['LoadState'] == 'not-found' and not info['FragmentPath']

    def configs_match(self, state):
        for r in state['resources']:
            config, unit = self.files(r['port'])
            if config.is_symlink() or unit.is_symlink() or config.read_text() != self.config(r) or unit.read_text() != self.service(r['port']):
                return False
        units = self.unit_metadata(r['port'] for r in state['resources'])
        return all(self.unit_matches(port, info, True) for port, info in units.items())

    def consistent(self, state, core_valid=None):
        try:
            if core_valid is None:
                self.require_core(state)
            elif not core_valid:
                return False
            return self.configs_match(state)
        # KeyError: confirmed state predates a field the templates now read.
        except (KeyError, OSError, Rejected, subprocess.TimeoutExpired, UnicodeError):
            return False

    def check_config(self, item):
        pass

    def refresh(self, state):
        stale = []
        for item in state['resources']:
            port = item['port']
            config, unit = self.files(port)
            if config.read_text() == self.config(item) and unit.read_text() == self.service(port):
                continue
            self.check_config(item)
            stale.append((port, self.running(port)))
            atomic(config, self.config(item).encode())
            atomic(unit, self.service(port).encode(), 0o644)
        if not stale:
            return
        command('systemctl', 'daemon-reload')
        # Only what changed restarts; an instance stopped on purpose stays stopped.
        for port, running in stale:
            if running:
                command('systemctl', 'restart', self.unit(port))
                self.verify(state, {'port': port})

    def require_platform(self):
        if platform.system() != 'Linux' or not pathlib.Path('/run/systemd/system').is_dir():
            raise Rejected('管理执行仅支持 Linux systemd')
        if not self.supported():
            raise Rejected('当前架构不支持所选核心安装')
    def prepare(self, action, before, after, payload):
        self.require_platform()
        port = payload['port']
        owned = any(r['port'] == port for r in before['resources'])
        paths = self.files(port)
        if not self.unit_matches(port, self.unit_metadata([port])[port], owned):
            raise Rejected('检测到同名服务或额外配置，拒绝修改', 'config_changed')
        if not owned:
            if any(p.exists() or p.is_symlink() for p in paths):
                raise Rejected('发现非本后台管理的配置或服务，拒绝接管')
            for kind in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
                with socket.socket(socket.AF_INET, kind) as sock:
                    try:
                        sock.bind(('0.0.0.0', port))
                    except OSError as exc:
                        raise Rejected('监听端口被占用', 'port_in_use') from exc
        files = []
        for p in paths:
            if p.is_symlink():
                raise Rejected('配置不能是符号链接')
            files.append(p.read_text() if p.exists() else None)
        # Detect terminal edits, rather than replacing an unknown file.
        if owned:
            current = next(r for r in before['resources'] if r['port'] == port)
            if files != [self.config(current), self.service(port)]:
                raise Rejected('配置已被外部修改，拒绝覆盖；请先核实本机配置', 'config_changed')
        return {'kind': 'resource', 'port': port, 'files': files,
                'running': self.running(port), 'enabled': self.enabled(port)}

    def config(self, item):
        return ('[snell-server]\n'
                f"listen = 0.0.0.0:{item['port']}\npsk = {item['psk']}\n"
                f"ipv6 = true\ntfo = true\nmode = {item['mode']}\ndns-ip-preference = default\n")

    def service(self, port):
        capabilities = 'AmbientCapabilities=CAP_NET_BIND_SERVICE\nCapabilityBoundingSet=CAP_NET_BIND_SERVICE\n' if port < 1024 else ''
        return f'''[Unit]
Description=Node Manager Snell {port}
After=network-online.target
Wants=network-online.target
[Service]
DynamicUser=yes
{capabilities}LoadCredential=config:{self.configs}/{port}.conf
ExecStart={self.binary} -c %d/config
Restart=on-failure
RestartSec=3
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
[Install]
WantedBy=multi-user.target
'''

    def apply(self, action, before, after, payload):
        port = payload['port']
        config, unit = self.files(port)
        if action == 'delete':
            command('systemctl', 'stop', self.unit(port))
            command('systemctl', 'disable', self.unit(port))
            for p in (config, unit):
                p.unlink()
                sync(p.parent)
            command('systemctl', 'daemon-reload')
        else:
            item = next(r for r in after['resources'] if r['port'] == port)
            atomic(config, self.config(item).encode())
            atomic(unit, self.service(port).encode(), 0o644)
            command('systemctl', 'daemon-reload')
            if not self.unit_matches(port, self.unit_metadata([port])[port], True):
                raise Rejected('系统加载了同名服务或额外配置，拒绝启动', 'config_changed')
            command('systemctl', 'enable', self.unit(port))
            command('systemctl', 'restart', self.unit(port))

    def verify(self, state, payload):
        if not self.consistent(state):
            raise Rejected('实际配置与已确认配置不一致', 'config_changed')
        port = payload.get('port')
        if port is None:
            return
        if not any(r['port'] == port for r in state['resources']):
            if (any(path.exists() or path.is_symlink() for path in self.files(port))
                    or not self.unit_matches(port, self.unit_metadata([port])[port], False)
                    or self.running(port)):
                raise Rejected('删除后仍存在配置或服务')
            return
        time.sleep(0.1)
        if not self.running(port):
            raise Rejected('实例未正常运行')
        with socket.create_connection(('127.0.0.1', port), timeout=2):
            pass

    def restore(self, backup):
        if backup['kind'] == 'install':
            self.binary.unlink(missing_ok=True)
            sync(self.binary.parent)
            return
        port = backup['port']
        info = self.unit_metadata([port])[port]
        if not (self.unit_matches(port, info, True) or self.unit_matches(port, info, False)):
            raise Rejected('检测到同名服务或额外配置，无法安全恢复', 'config_changed')
        command('systemctl', 'stop', self.unit(port), check=False)
        if self.running(port):
            raise Rejected('服务无法停止，不能恢复配置')
        command('systemctl', 'disable', self.unit(port), check=False)
        for path, content in zip(self.files(port), backup['files']):
            if content is None:
                path.unlink(missing_ok=True)
                sync(path.parent)
            else:
                atomic(path, content.encode(), 0o644 if path.suffix == '.service' else 0o600)
        command('systemctl', 'daemon-reload')
        if not self.unit_matches(port, self.unit_metadata([port])[port], backup['files'][1] is not None):
            raise Rejected('系统加载的服务与恢复记录不一致', 'config_changed')
        if backup['enabled']:
            command('systemctl', 'enable', self.unit(port))
        if backup['running']:
            command('systemctl', 'start', self.unit(port))
        if self.running(port) != backup['running'] or self.enabled(port) != backup['enabled']:
            raise Rejected('原服务状态未恢复')
