#!/usr/bin/env python3
"""Local backup, restore, upgrade and removal for a systemd Node Manager hub."""
import argparse
import contextlib
import fcntl
import hashlib
import json
import os
import pathlib
import re
import shlex
import shutil
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time
import urllib.parse

from hub_health import wait_for_hub

UNITS = pathlib.Path('/etc/systemd/system')
STATE = pathlib.Path('/var/lib/node-manager-hub-lifecycle')
UNIT = 'node-manager-hub.service'
BINARY = pathlib.Path('/opt/node-manager-hub/monitor-hub')
DATABASE = pathlib.Path('/var/lib/node-manager-hub/hub.db')
MARKER = '.node-manager-install'
# nodehelm.sh keeps a copy beside the program and links it into the path.
LOCAL_COMMAND = 'nodehelm-hub'
COMMAND_LINK = pathlib.Path('/usr/local/sbin') / LOCAL_COMMAND
GATE = 'hub_lifecycle_maintenance'
HEX = re.compile(r'[0-9a-f]{64}')


def sync(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def exists(path):
    return path.exists() or path.is_symlink()


def regular(path, private=False):
    meta = path.lstat()
    if (not stat.S_ISREG(meta.st_mode) or meta.st_nlink != 1
            or meta.st_mode & (0o077 if private else 0o022)):
        raise ValueError(f'{path.name} 必须是权限受限的独立普通文件')
    return meta


def private_dir(path):
    meta = path.lstat()
    if not stat.S_ISDIR(meta.st_mode) or meta.st_mode & 0o077 or meta.st_uid != os.geteuid():
        raise ValueError(f'{path.name} 必须是当前用户拥有的 0700 目录')
    return path


def trusted_parents(path):
    for directory in path.parents:
        meta = directory.lstat()
        if not stat.S_ISDIR(meta.st_mode) or meta.st_uid != 0 or meta.st_mode & 0o022:
            raise ValueError(f'备份父目录 {directory} 须归 root 所有且不能允许其他用户写入')


def sha256(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest() if hasattr(hashlib, 'file_digest') else digest_stream(source)


def digest_stream(source):
    digest = hashlib.sha256()
    for chunk in iter(lambda: source.read(1024 * 1024), b''):
        digest.update(chunk)
    return digest.hexdigest()


def replace_file(destination, source=None, data=None, mode=0o600, owner=None, staging=None, temporary=None):
    if exists(destination):
        regular(destination)
    staging = destination.parent if staging is None else staging
    if temporary is None:
        fd, name = tempfile.mkstemp(dir=staging, prefix='.hub-lifecycle-')
        tmp = pathlib.Path(name)
    else:
        tmp = temporary
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(fd, 'wb') as target:
            if source is not None:
                with source.open('rb') as payload:
                    shutil.copyfileobj(payload, target)
            else:
                target.write(data)
            if owner is not None:
                os.fchown(target.fileno(), *owner)
            os.fchmod(target.fileno(), mode)
            target.flush()
            os.fsync(target.fileno())
        os.replace(tmp, destination)
        sync(destination.parent)
        if staging != destination.parent:
            sync(staging)
    finally:
        tmp.unlink(missing_ok=True)


def switch_temporary(path):
    return path.with_name(f'.node-manager-switch-{path.name}.tmp')


def remove(path):
    if exists(path):
        regular(path)
        path.unlink()
        sync(path.parent)


def write_json(path, value):
    replace_file(path, data=json.dumps(value, sort_keys=True).encode())


def load_json(path):
    regular(path, private=True)
    return json.loads(path.read_bytes())


def ctl(*args):
    result = subprocess.run(('/usr/bin/systemctl', *args), capture_output=True, text=True,
                            stdin=subprocess.DEVNULL, timeout=90, check=False,
                            env={'PATH': '/usr/sbin:/usr/bin:/sbin:/bin', 'LC_ALL': 'C'})
    if result.returncode:
        raise ValueError(f'系统服务操作失败（{args[0]}），请检查后台服务日志')
    return result.stdout


def read_only(path):
    return sqlite3.connect(path.as_uri() + '?mode=ro', uri=True, timeout=5)


def copy_db(source, destination):
    regular(source)
    # SQLite reads committed WAL pages and never copies a torn live DB.
    destination.touch(mode=0o600, exist_ok=False)
    with contextlib.closing(read_only(source)) as original, contextlib.closing(sqlite3.connect(destination)) as copy:
        original.backup(copy)
        # SQLite can copy a WAL-mode header. Backups must remain single files,
        # including when the backup itself is later opened read-only.
        if copy.execute('PRAGMA journal_mode=DELETE').fetchone() != ('delete',):
            raise ValueError('无法将备份数据库转换为单文件模式')
        if copy.execute('PRAGMA integrity_check').fetchall() != [('ok',)]:
            raise ValueError('SQLite 数据库完整性校验未通过')
    sync(destination)
    sync(destination.parent)


def check_key(database, key):
    if exists(key):
        regular(key, private=True)
        if key.stat().st_size != 32:
            raise ValueError('管理密钥长度不正确')
        return
    with contextlib.closing(read_only(database)) as db:
        tables = {name for name, in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        if ('managed_node' in tables and db.execute("SELECT 1 FROM managed_node WHERE snapshot!='' LIMIT 1").fetchone()
                or 'managed_operation' in tables and db.execute('SELECT 1 FROM managed_operation LIMIT 1').fetchone()
                or 'managed_lifecycle' in tables and db.execute("SELECT 1 FROM managed_lifecycle WHERE cleanup_cipher!='' LIMIT 1").fetchone()):
            raise ValueError('数据库包含加密管理数据，缺少配套密钥')


def ensure_idle(database):
    with contextlib.closing(read_only(database)) as db:
        tables = {name for name, in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        if ('managed_maintenance' in tables and db.execute('SELECT 1 FROM managed_maintenance WHERE expires>? LIMIT 1',
                                                          (int(time.time()),)).fetchone()):
            raise ValueError('有管理组件正在维护，请等待该操作完成')
        if 'managed_lifecycle' in tables and db.execute("SELECT 1 FROM managed_lifecycle WHERE status IN ('running','paused','cleaning','finalizing') LIMIT 1").fetchone():
            raise ValueError('节点卸载尚未确认完成，请先恢复并完成清理')
        if 'managed_operation' in tables:
            if db.execute("SELECT 1 FROM managed_operation WHERE status='delivered' OR (status='queued' AND expires>?) LIMIT 1",
                          (int(time.time()),)).fetchone():
                raise ValueError('后台仍有未完成管理任务，请先核实任务结果')


def gate(database, enabled):
    with contextlib.closing(sqlite3.connect(database, timeout=10)) as db, db:
        if enabled:
            db.execute('INSERT OR REPLACE INTO setting(key,value) VALUES(?,?)', (GATE, 'on'))
        else:
            db.execute('DELETE FROM setting WHERE key=?', (GATE,))


def snapshot(workspace, binary, database, unit, automatic=False):
    directory = pathlib.Path(tempfile.mkdtemp(prefix='backup-', dir=workspace))
    copy_db(database, directory / 'hub.db')
    key = database.with_name(database.name + '.management.key')
    check_key(directory / 'hub.db', key)
    if exists(key):
        replace_file(directory / 'hub.db.management.key', source=key)
    replace_file(directory / 'monitor-hub', source=binary, mode=0o700)
    names = {file.name: sha256(file) for file in directory.iterdir()}
    # Marks the copy an update or restore takes for itself, the only kind ever pruned.
    write_json(directory / 'manifest.json', {'unit': unit, 'files': names, **({'automatic': True} if automatic else {})})
    sync(workspace)
    return directory


def verify_backup(directory):
    private_dir(directory)
    info = load_json(directory / 'manifest.json')
    files = info['files']
    mandatory = {'hub.db', 'monitor-hub'}
    if set(files) not in (mandatory, mandatory | {'hub.db.management.key'}):
        raise ValueError('后台备份文件清单不符合要求')
    if {path.name for path in directory.iterdir()} != set(files) | {'manifest.json'}:
        raise ValueError('备份目录含未声明文件')
    for name, checksum in files.items():
        file = directory / name
        regular(file, private=True)
        if not isinstance(checksum, str) or not HEX.fullmatch(checksum) or sha256(file) != checksum:
            raise ValueError(f'备份文件 {name} 校验失败')
    check_key(directory / 'hub.db', directory / 'hub.db.management.key')
    return info


class Hub:
    def __init__(self, args):
        self.unit = args.unit
        self.binary = args.binary
        self.database = args.db
        self.health = args.health
        self.workspace = args.backups
        self.key = self.database.with_name(self.database.name + '.management.key')
        if not re.fullmatch(r'node-manager-[a-z0-9-]+\.service', self.unit):
            raise ValueError('仅支持独立的 node-manager-*.service 后台')
        self.state = STATE / self.unit
        self.journal = self.state / 'recovery.json'
        self.blocked = self.state / 'switching'
        self.drop_in = UNITS / (self.unit + '.d') / '90-node-manager-lifecycle.conf'
        self.guard = f'[Unit]\nConditionPathExists=!{self.blocked}\n'.encode()
        for path in (self.binary, self.workspace):
            if not path.is_absolute() or path.resolve() != path or re.search(r'\s', str(path)):
                raise ValueError('请使用绝对、无符号链接且不含空白的路径')
        self.check_database_path()
        health = urllib.parse.urlsplit(self.health)
        if (health.scheme != 'http' or health.hostname not in ('127.0.0.1', '::1')
                or not health.port or health.path != '/api/me' or health.query or health.fragment
                or health.username or health.password):
            raise ValueError('健康检查须使用后台回环监听的 HTTP /api/me 地址')
        self.listen = health.netloc
        self.health_ip, self.health_port = health.hostname, health.port

    def check_database_path(self):
        path = self.database
        if not path.is_absolute() or re.search(r'\s', str(path)) or path.is_symlink():
            raise ValueError('数据库须使用绝对、不含空白且不是符号链接的文件路径')
        if path.resolve() == path:
            return
        parent = path.parent
        private = pathlib.Path('/var/lib/private')
        if (not parent.is_symlink() or parent.parent != private.parent
                or parent.resolve() != private / parent.name or parent.lstat().st_uid != 0
                or path.resolve() != private / parent.name / path.name):
            raise ValueError('数据库目录只能使用 systemd 的固定 StateDirectory 链接')
        meta = private.lstat()
        target = parent.resolve().lstat()
        if (not stat.S_ISDIR(meta.st_mode) or meta.st_uid != 0 or meta.st_mode & 0o077
                or not stat.S_ISDIR(target.st_mode) or target.st_mode & 0o077):
            raise ValueError('systemd 私有数据目录权限不符合要求')

    def prepare_guard(self):
        directory = self.drop_in.parent
        directory.mkdir(mode=0o755, exist_ok=True)
        meta = directory.lstat()
        if (not stat.S_ISDIR(meta.st_mode) or meta.st_uid != 0 or meta.st_mode & 0o022
                or any(path != self.drop_in for path in directory.iterdir())):
            raise ValueError('后台服务存在未知覆盖配置')
        if exists(self.drop_in):
            regular(self.drop_in)
            if self.drop_in.read_bytes() != self.guard:
                raise ValueError('后台启动保护配置已被修改')
        else:
            # An interrupted write may leave a temporary file. Keep it
            # outside the strictly validated systemd override directory.
            replace_file(self.drop_in, data=self.guard, mode=0o644, staging=UNITS)
        ctl('daemon-reload')
        self.info()
        replace_file(self.blocked, data=b'local transition\n')

    def identity(self):
        return {'unit': self.unit, 'binary': str(self.binary), 'db': str(self.database),
                'health': self.health, 'workspace': str(self.workspace)}

    def info(self):
        self.check_database_path()
        fields = 'FragmentPath,DropInPaths,ActiveState,MainPID,ExecStart,StateDirectory'
        info = dict(line.split('=', 1) for line in ctl('show', '--all', f'--property={fields}', self.unit).splitlines() if '=' in line)
        unit_path = UNITS / self.unit
        expected_drop_in = ''
        if exists(self.drop_in):
            if regular(self.drop_in).st_uid != 0 or self.drop_in.read_bytes() != self.guard:
                raise ValueError('后台启动保护配置已被修改')
            expected_drop_in = str(self.drop_in)
        if (info.get('FragmentPath') != str(unit_path) or info.get('DropInPaths') != expected_drop_in
                or regular(unit_path).st_uid != 0
                or self.database.parent.is_symlink() and info.get('StateDirectory') != self.database.parent.name):
            raise ValueError('后台服务实际加载路径或覆盖配置不符合要求')
        match = re.fullmatch(r'\{ path=(\S+) ; argv\[\]=(.*?) ;.*\}', info.get('ExecStart', ''))
        argv = shlex.split(match[2]) if match else []
        if (not argv or match[1] != str(self.binary) or argv[0] != str(self.binary)
                or argv.count('--db') != 1 or argv[argv.index('--db')+1:argv.index('--db')+2] != [str(self.database)]
                or argv.count('--listen') != 1 or argv[argv.index('--listen')+1:argv.index('--listen')+2] != [self.listen]):
            raise ValueError('指定后台二进制、数据库或监听地址与实际服务不一致')
        return info

    def stop(self):
        self.info()
        ctl('stop', self.unit)
        self.check_stopped()

    def check_stopped(self):
        info = self.info()
        if info.get('MainPID') != '0' or info.get('ActiveState') not in ('inactive', 'failed'):
            raise ValueError('后台未停止，拒绝修改文件')

    def start(self):
        self.info()
        # Explicit maintenance recovery may follow several failed candidate starts.
        ctl('reset-failed', self.unit)
        regular(self.database)
        ctl('start', self.unit)
        wait_for_hub(self.info, self.binary, self.health, self.health_ip, self.health_port)

    def install(self, directory, replace_binary, record):
        for path in (self.database, self.key, self.binary):
            remove(switch_temporary(path))
        for suffix in ('-wal', '-shm', '-journal'):
            remove(self.database.with_name(self.database.name + suffix))
        replace_file(self.database, source=directory / 'hub.db', owner=record['db_owner'],
                     temporary=switch_temporary(self.database))
        key = directory / 'hub.db.management.key'
        if exists(key):
            replace_file(self.key, source=key, owner=record['db_owner'], temporary=switch_temporary(self.key))
        else:
            remove(self.key)
        if replace_binary:
            replace_file(self.binary, source=directory / 'monitor-hub', mode=record['binary_mode'],
                         owner=record['binary_owner'], temporary=switch_temporary(self.binary))

    def rollback(self, record):
        if not re.fullmatch(r'backup-[a-z0-9_]+', record['backup']):
            raise ValueError('切换恢复记录中的备份路径无效')
        old = self.workspace / record['backup']
        verify_backup(old)
        regular(self.binary)
        if sha256(self.binary) not in (record['before_binary'], record['after_binary']):
            raise ValueError('后台程序已在外部替换，拒绝覆盖')
        self.prepare_guard()
        self.stop()
        self.install(old, True, record)
        # Once the old hub can accept writes, recovery must never recopy the
        # stopped snapshot over it, even if this process dies before cleanup.
        record['phase'] = 'rolled_back'
        write_json(self.journal, record)
        self.resume_paused(True)

    def pause(self, active):
        write_json(self.journal, {'identity': self.identity(), 'phase': 'paused', 'active': active})
        self.prepare_guard()
        self.stop()

    def resume_paused(self, active):
        if exists(self.drop_in):
            ctl('daemon-reload')
        remove(self.blocked)
        if active:
            self.start()
        remove(self.journal)

    def recover(self, record):
        if record['identity'] != self.identity():
            raise ValueError('恢复参数与原切换记录不同')
        phase = record['phase']
        if phase == 'ready':
            self.start()
            gate(self.database, False)
            remove(self.journal)
        elif phase == 'paused':
            self.resume_paused(record['active'])
        elif phase == 'switching':
            self.rollback(record)
        elif phase == 'rolled_back':
            self.resume_paused(True)
        elif phase == 'uninstalling':
            self.finish_uninstall(record)
        else:
            raise ValueError('发现未知后台恢复阶段，请保留现场')

    def backup(self):
        active = self.info().get('ActiveState') == 'active'
        self.pause(active)
        try:
            directory = snapshot(self.workspace, self.binary, self.database, self.unit)
        finally:
            self.resume_paused(active)
        print(f'后台一致性备份已保存至 {directory}')

    def transition(self, args):
        db_meta = regular(self.database)
        bin_meta = regular(self.binary)
        if bin_meta.st_uid != 0:
            raise ValueError('后台程序须归 root 所有')
        if args.action == 'upgrade':
            regular(args.candidate)
            if not HEX.fullmatch(args.sha256) or sha256(args.candidate) != args.sha256:
                raise ValueError('候选后台程序摘要校验失败')
        else:
            verify_backup(args.backup)
        if self.info().get('ActiveState') != 'active':
            raise ValueError('切换前后台须运行；中断的切换请先 recover')
        ensure_idle(self.database)
        self.pause(True)
        try:
            ensure_idle(self.database)
            old = snapshot(self.workspace, self.binary, self.database, self.unit, automatic=True)
            record = {'identity': self.identity(), 'backup': old.name, 'phase': 'switching',
                      'db_owner': [db_meta.st_uid, db_meta.st_gid],
                      'binary_owner': [bin_meta.st_uid, bin_meta.st_gid],
                      'binary_mode': stat.S_IMODE(bin_meta.st_mode),
                      'before_binary': sha256(old / 'monitor-hub'),
                      'after_binary': args.sha256 if args.action == 'upgrade' else sha256(old / 'monitor-hub')}
            with tempfile.TemporaryDirectory(prefix='candidate-', dir=self.workspace) as name:
                stage = pathlib.Path(name)
                source = old if args.action == 'upgrade' else args.backup
                copy_db(source / 'hub.db', stage / 'hub.db')
                if exists(source / 'hub.db.management.key'):
                    replace_file(stage / 'hub.db.management.key', source=source / 'hub.db.management.key')
                if args.action == 'upgrade':
                    replace_file(stage / 'monitor-hub', source=args.candidate, mode=0o700)
                    if sha256(stage / 'monitor-hub') != args.sha256:
                        raise ValueError('候选后台程序在准备期间发生变化')
                    protocol = subprocess.run((str(stage / 'monitor-hub'), '--lifecycle-protocol'),
                                              capture_output=True, stdin=subprocess.DEVNULL, timeout=15)
                    if protocol.returncode or protocol.stdout.strip() != b'node-manager-hub-lifecycle-v1':
                        raise ValueError('候选后台程序不支持维护切换协议')
                else:
                    outcome = subprocess.run((str(self.binary), '--db', str(stage / 'hub.db'), '--prepare-restore'),
                                             capture_output=True, stdin=subprocess.DEVNULL, timeout=300)
                    if outcome.returncode:
                        raise ValueError('候选数据库未通过后台恢复校验')
                gate(stage / 'hub.db', True)
                sync(stage / 'hub.db')
                write_json(self.journal, record)
                self.check_stopped()
                self.install(stage, args.action == 'upgrade', record)
                remove(self.blocked)
                self.start()
        except Exception as original:
            if load_json(self.journal)['phase'] == 'switching':
                try:
                    self.rollback(record)
                except Exception:
                    raise ValueError(f'后台切换失败且回退未完成，原备份位于 {old}，请执行 recover') from None
                raise ValueError(f'后台切换失败，已恢复停服时的版本和数据库（{diagnostic(original)}）；备份 {old}') from None
            self.resume_paused(True)
            raise
        record['phase'] = 'ready'
        write_json(self.journal, record)
        gate(self.database, False)
        remove(self.journal)
        print(f'后台已启动并通过回环 HTTP 检查。切换前备份 {old}')
        # A restore source is the user's choice to keep, even an automatic one.
        self.prune_automatic(keep={old, getattr(args, 'backup', None)})

    def prune_automatic(self, keep):
        # The switch has succeeded; a failure here only leaves an extra copy behind.
        removed = 0
        for entry in sorted(self.workspace.iterdir()):
            try:
                if entry in keep or not entry.name.startswith('backup-'):
                    continue
                info = verify_backup(entry)
                if info['unit'] != self.unit or info.get('automatic') is not True:
                    continue
                shutil.rmtree(entry)
                removed += 1
            except (ValueError, OSError, KeyError, TypeError):
                continue
        if removed:
            sync(self.workspace)
            print(f'已清理 {removed} 份较早的自动备份，手动备份不受影响')

    def data_files(self):
        return [self.database, self.key, *(self.database.with_name(self.database.name + suffix)
                                          for suffix in ('-wal', '-shm', '-journal'))]

    def uninstall(self, args):
        marker = self.binary.parent / MARKER
        if ((self.unit, self.binary, self.database) != (UNIT, BINARY, DATABASE)
                or regular(marker).st_uid != 0 or marker.read_bytes() != b'node-manager-hub-v1\n'):
            raise ValueError('仅可卸载独立安装器登记的后台')
        active = self.info().get('ActiveState') == 'active'
        # Either way is stated, never assumed: one keeps the data, the other leaves
        # nothing, and a copy to keep is the panel's backup download, taken first.
        purge = args.purge
        ensure_idle(self.database)
        self.pause(active)
        try:
            ensure_idle(self.database)
            if purge:
                for path in self.data_files():
                    if exists(path):
                        regular(path)
            record = {'identity': self.identity(), 'phase': 'uninstalling', 'purge': purge,
                      'unit_sha': sha256(UNITS / self.unit), 'binary_sha': sha256(self.binary)}
            write_json(self.journal, record)
        except Exception:
            self.resume_paused(active)
            raise
        self.finish_uninstall(record)

    def remove_data_directories(self):
        data = self.database.parent
        if not exists(data):
            return
        real = data.resolve()
        themes = real / 'themes'
        if themes.is_dir() and not themes.is_symlink():
            shutil.rmtree(themes)
        # Never a blanket delete: --db may point into a directory this project does not own.
        if any(real.iterdir()):
            print(f'数据目录仍有非本项目文件，已保留：{real}')
            return
        real.rmdir()
        if data.is_symlink():
            data.unlink()

    def remove_local_command(self):
        command = self.binary.parent / LOCAL_COMMAND
        if COMMAND_LINK.is_symlink() and os.readlink(COMMAND_LINK) == str(command):
            COMMAND_LINK.unlink()
            sync(COMMAND_LINK.parent)
        for path in (command, command.with_name('nodehelm-hub.new')):
            remove(path)
        tools = self.binary.parent / 'tools'
        if exists(tools):
            for name in ('hub-lifecycle.py', 'hub_health.py'):
                remove(tools / name)
                remove(tools / (name + '.new'))
            if any(tools.iterdir()):
                print(f'工具目录有非本项目文件，已保留：{tools}')
            else:
                tools.rmdir()

    def remove_backups(self):
        # Only what this tool wrote and can still verify; anything else stays and is named.
        if not exists(self.workspace):
            return
        kept = []
        for entry in sorted(self.workspace.iterdir()):
            try:
                if not entry.name.startswith('backup-') or verify_backup(entry)['unit'] != self.unit:
                    raise ValueError
            except (ValueError, OSError, KeyError, TypeError):
                kept.append(entry)
                continue
            shutil.rmtree(entry)
        sync(self.workspace)
        if kept:
            print('备份目录中有无法核实的内容，已保留：' + '，'.join(str(path) for path in kept))
        else:
            self.workspace.rmdir()

    def remove_run_state(self):
        # The lock this process holds stays valid until exit; nothing else is left to protect.
        remove(self.state / 'lock')
        self.state.rmdir()
        if not any(STATE.iterdir()):
            STATE.rmdir()
        if self.unit == UNIT:
            remove(pathlib.Path('/run/node-manager-hub-install.lock'))

    def finish_uninstall(self, record):
        # An older purge record named a last backup it promised to keep.
        purge = record.get('purge', bool(record.get('backup')))
        legacy = 'purge' not in record
        unit_file = UNITS / self.unit
        if exists(unit_file):
            regular(unit_file)
            if sha256(unit_file) != record['unit_sha']:
                raise ValueError('后台服务文件已被外部替换，拒绝删除')
            self.prepare_guard()
            self.stop()
            ctl('disable', self.unit)
            remove(unit_file)
        if exists(self.drop_in):
            regular(self.drop_in)
            if self.drop_in.read_bytes() != self.guard:
                raise ValueError('后台启动保护配置已被修改')
            remove(self.drop_in)
        if self.drop_in.parent.exists() and not any(self.drop_in.parent.iterdir()):
            self.drop_in.parent.rmdir()
            sync(UNITS)
        ctl('daemon-reload')
        if exists(self.binary):
            regular(self.binary)
            if sha256(self.binary) != record['binary_sha']:
                raise ValueError('后台程序已被外部替换，拒绝删除')
            remove(self.binary)
        remove(switch_temporary(self.binary))
        self.remove_local_command()
        if purge:
            for path in (*self.data_files(), switch_temporary(self.database), switch_temporary(self.key)):
                remove(path)
        marker = self.binary.parent / MARKER
        # A purge interrupted after removing the marker resumes here without it.
        if not (purge and not exists(marker)):
            if regular(marker).st_uid != 0 or marker.read_bytes() != b'node-manager-hub-v1\n':
                raise ValueError('后台安装标识已被外部替换，请核实卸载状态')
        if purge:
            self.remove_data_directories()
            if not legacy:
                self.remove_backups()
        remove(self.blocked)
        if purge:
            remove(marker)
            if self.binary.parent.exists() and not any(self.binary.parent.iterdir()):
                self.binary.parent.rmdir()
        remove(self.journal)
        if purge:
            self.remove_run_state()
        print('后台服务及程序已卸载。' + ('数据库、密钥、主题和备份已全部删除。' if purge else '后台数据库和密钥已保留，可用 --reuse-data 重装。'))


def diagnostic(error):
    return str(error) if type(error) is ValueError else type(error).__name__


@contextlib.contextmanager
def exclusive_lock(path):
    if exists(path):
        regular(path, private=True)
    fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    with os.fdopen(fd, 'w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        yield


def service_listen(unit):
    # An installation with a custom --port needs no extra argument for maintenance.
    match = re.search(r'--listen\s+(\S+)', ctl('show', '--property=ExecStart', unit))
    return match[1] if match else '127.0.0.1:9911'


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--unit', default=UNIT)
    parser.add_argument('--binary', type=pathlib.Path, default=BINARY)
    parser.add_argument('--db', type=pathlib.Path, default=DATABASE)
    parser.add_argument('--health', help='默认按服务实际的 --listen 地址')
    parser.add_argument('--backups', type=pathlib.Path, default=pathlib.Path('/var/backups/node-manager-hub'))
    actions = parser.add_subparsers(dest='action', required=True)
    actions.add_parser('backup')
    upgrade = actions.add_parser('upgrade')
    upgrade.add_argument('--candidate', type=pathlib.Path, required=True)
    upgrade.add_argument('--sha256', required=True)
    restore = actions.add_parser('restore')
    restore.add_argument('--backup', type=pathlib.Path, required=True)
    actions.add_parser('recover')
    uninstall = actions.add_parser('uninstall')
    scope = uninstall.add_mutually_exclusive_group(required=True)
    scope.add_argument('--purge', action='store_true', help='删除数据库、密钥、主题、维护记录和本工具的备份')
    scope.add_argument('--keep-data', action='store_true', help='保留数据库、密钥和主题以便重装')
    args = parser.parse_args(argv)
    if getattr(args, 'backup', None) is not None and (not args.backup.is_absolute() or args.backup.resolve() != args.backup):
        parser.error('备份必须是绝对路径且不能包含符号链接')
    if os.geteuid() != 0 or not pathlib.Path('/run/systemd/system').is_dir():
        raise ValueError('请在 Linux systemd 后台主机上使用 root 执行')
    os.umask(0o077)
    if args.health is None:
        args.health = f'http://{service_listen(args.unit)}/api/me'
    hub = Hub(args)
    if hub.workspace == hub.database.parent.resolve() or hub.database.parent.resolve() in hub.workspace.parents:
        raise ValueError('后台备份工作目录须位于活动数据目录外')
    trusted_parents(hub.workspace)
    if getattr(args, 'backup', None) is not None:
        trusted_parents(args.backup)
    hub.workspace.mkdir(mode=0o700, exist_ok=True)
    private_dir(hub.workspace)
    STATE.mkdir(mode=0o700, exist_ok=True)
    private_dir(STATE)
    hub.state.mkdir(mode=0o700, exist_ok=True)
    private_dir(hub.state)
    with contextlib.ExitStack() as stack:
        if hub.unit == UNIT:
            stack.enter_context(exclusive_lock(pathlib.Path('/run/node-manager-hub-install.lock')))
            if exists(pathlib.Path('/opt/.node-manager-hub-install.json')):
                raise ValueError('安装仍有待恢复记录，请先使用安装器 --resume')
        stack.enter_context(exclusive_lock(hub.state / 'lock'))
        if exists(hub.journal):
            if args.action != 'recover':
                raise ValueError('存在未完成的后台切换，请先执行 recover')
            hub.recover(load_json(hub.journal))
            print('后台恢复已完成')
        elif args.action == 'recover':
            print('没有待恢复的后台切换')
        elif args.action == 'backup':
            hub.backup()
        elif args.action == 'uninstall':
            hub.uninstall(args)
        else:
            hub.transition(args)


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(f'后台维护未完成：{diagnostic(error)}', file=sys.stderr)
        sys.exit(1)
