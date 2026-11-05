#!/usr/bin/env python3
"""Run as root: unshare -n python3 integration_core_lifecycle.py /path/to/release-archives.

Place the five upstream archives for the host architecture in the given directory,
using their published names. Run from a checkout readable by a systemd service
with ProtectHome enabled, for example a staging copy under /opt."""
import hashlib
import os
import pathlib
import shutil
import signal
import socket
import sys
import tempfile
import time
import urllib.request
import uuid
from unittest.mock import patch

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import pm
from agent import PMController, execute_module_task
from controller import Controller
from model import Rejected
from services import Driver
from snell import Driver as SnellDriver, atomic, command
from storage import private_directory, read, write


SYSTEMD = pathlib.Path('/etc/systemd/system')
VERSIONS = {'snell': ('6.0.0', None), 'sb': ('1.14.4', '1.14.3'), 'fw': ('2.9.6', '2.9.5')}


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def service_namespace(unit, pid):
    return unit.replace('[Service]\n', f'[Service]\nNetworkNamespacePath=/proc/{pid}/ns/net\n', 1)


def pm_units(root, pid, netns):
    service = f'{root.name}-pm.service'
    timer = f'{root.name}-pm.timer'
    service_text = service_namespace(pm.SERVICE_UNIT, pid)
    runner = f'"{pathlib.Path(__file__).resolve()}" --maintain-pm {root} {pid} {netns}'
    service_text = service_text.replace('/opt/node-manager/pm.py', runner)
    service_text = service_text.replace('/var/lib/node-manager/pm', str(root / 'state' / 'pm'))
    timer_text = pm.TIMER_UNIT.replace('Unit=node-manager-pm.service', f'Unit={service}')
    return patch.multiple(pm, SERVICE_NAME=service, TIMER_NAME=timer,
                          SERVICE_UNIT=service_text, TIMER_UNIT=timer_text)


def maintain_pm(root, pid, netns):
    if root.parent != pathlib.Path('/opt') or not root.name.startswith('node-manager-acceptance-'):
        raise RuntimeError('Unexpected PM test path')
    if os.stat('/proc/self/ns/net').st_ino != netns:
        raise RuntimeError('Refusing to maintain PM outside the test network namespace')
    with pm_units(root, pid, netns):
        pm.Driver(root / 'state' / 'pm', SYSTEMD).maintain()


def build(root, module):
    state = private_directory(root / 'state' / module)
    configs = private_directory(state / 'configs')
    driver = SnellDriver() if module == 'snell' else Driver(module, state.parent, root)
    if module == 'snell':
        driver.binary = root / 'snell-server'
        driver.configs = configs
    driver.unit = lambda port: f'{root.name}-{module}-{port}.service'
    driver.unit_pattern = lambda: f'{root.name}-{module}-*.service'
    service, pid = driver.service, os.getpid()
    driver.service = lambda port: service_namespace(service(port), pid)
    return Controller(state, driver, module), driver


def task(controller, action, payload=None):
    return dict(id=uuid.uuid4().hex, module=controller.module, action=action,
                payload=payload if payload is not None else {},
                revision=controller.snapshot()['revision'])


def execute(controller, action, payload=None, status='succeeded'):
    result = controller.execute(task(controller, action, payload))
    if result['status'] != status:
        diagnostic = read(controller.root / 'last-error.json')
        raise AssertionError((controller.module, action, result, diagnostic))
    return result


def rejected(controller, action, payload=None, *, code):
    request = task(controller, action, payload)
    try:
        controller.execute(request)
    except Rejected as error:
        assert error.code == code, (controller.module, action, error.code, str(error))
        after = controller.snapshot()
        assert after['revision'] == request['revision'] and not controller.journal.exists()
    else:
        raise AssertionError(f'{controller.module} unexpectedly allowed {action}')


def archive_path(assets, url):
    return assets / url.rsplit('/', 1)[-1]


def local_downloads(assets):
    class LocalReleaseOpener:
        def open(self, request, timeout):
            url = request.full_url if isinstance(request, urllib.request.Request) else request
            response = archive_path(assets, url).open('rb')
            response.url = url
            return response
    return patch('urllib.request.build_opener', return_value=LocalReleaseOpener())


def seed_previous(controller, driver, version):
    """Reproduce a node already running the verified historical upstream release."""
    asset = driver.asset(version)
    with tempfile.TemporaryFile() as binary:
        driver.download(asset, binary)
        atomic(driver.binary, binary.read(), 0o755)
    write(controller.state, {'installed': True, 'resources': [],
                             'core': {'version': version, 'sha256': asset['sha256'], 'previous_version': None}})

def free_port():
    with socket.socket() as tcp, socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
        tcp.bind(('127.0.0.1', 0))
        port = tcp.getsockname()[1]
        udp.bind(('127.0.0.1', port))
        return port


def resource(module, port, target):
    result = dict(name=f'Lifecycle {port}', port=port)
    if module == 'fw':
        return dict(result, target_host='127.0.0.1', target_port=target)
    result.update(host='127.0.0.1', public_port=port)
    if module == 'snell':
        return dict(result, mode='default')
    return dict(result, protocol='ss2022')


def assert_core(controller, driver, version, previous=None):
    snapshot = controller.snapshot()
    assert snapshot['installed'] and not snapshot['blocked']
    core = snapshot['core']
    assert core['version'] == version and core.get('previous_version') == previous, core
    assert core['sha256'] == driver.asset(version)['sha256'] == digest(driver.binary)
    assert not any(driver.binary.parent.glob(f'.node-manager-{driver.binary.name}-*'))
    return snapshot


def assert_instance(driver, port, sha256):
    with socket.create_connection(('127.0.0.1', port), timeout=2):
        pass
    pid = int(command('systemctl', 'show', '-p', 'MainPID', '--value', driver.unit(port)).stdout)
    assert pid > 0 and digest(pathlib.Path(f'/proc/{pid}/exe')) == sha256
    assert os.stat(f'/proc/{pid}/ns/net').st_ino == os.stat('/proc/self/ns/net').st_ino
    return pid


def assert_tcp_relay(port, target):
    data = b'lifecycle TCP'
    with socket.create_connection(('127.0.0.1', port), timeout=3) as client:
        client.sendall(data)
        while True:
            peer, _ = target.accept()
            with peer:
                peer.settimeout(3)
                received = peer.recv(len(data), socket.MSG_WAITALL)
                # The service health check may have already connected without a payload.
                if not received:
                    continue
                assert received == data
                peer.sendall(b'reply')
                break
        assert client.recv(5, socket.MSG_WAITALL) == b'reply'


def assert_udp_relay(port, target):
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client:
        client.settimeout(3)
        client.sendto(b'lifecycle UDP', ('127.0.0.1', port))
        received, relay = target.recvfrom(1024)
        assert received == b'lifecycle UDP'
        target.sendto(b'reply', relay)
        assert client.recv(1024) == b'reply'


def uninstall_and_reinstall(controller, driver, current):
    execute(controller, 'core_uninstall')
    assert not controller.snapshot()['installed'] and not driver.binary.exists()
    execute(controller, 'install')
    assert_core(controller, driver, current)
    execute(controller, 'core_uninstall')
    assert not controller.snapshot()['installed'] and not driver.binary.exists()


def run_snell(root):
    controller, driver = build(root, 'snell')
    current, _ = VERSIONS['snell']
    execute(controller, 'install')
    installed = assert_core(controller, driver, current)
    # The deployed v6 installation predates persisted core metadata.
    state = read(controller.state)
    del state['core']
    write(controller.state, state)
    assert_core(controller, driver, current)
    rejected(controller, 'core_upgrade', {'version': '999.0.0'}, code='core_version')
    rejected(controller, 'core_upgrade', {'version': current}, code='core_version')
    rejected(controller, 'core_rollback', {'version': current}, code='core_version')
    port = free_port()
    execute(controller, 'apply', resource('snell', port, None))
    assert_instance(driver, port, installed['core']['sha256'])
    rejected(controller, 'core_uninstall', code='core_in_use')
    execute(controller, 'delete', {'port': port})
    uninstall_and_reinstall(controller, driver, current)
    print('PASS Snell 6.0.0: existing state, running instance, unknown upgrade rejection, uninstall and reinstall', flush=True)
    print('UNVERIFIED Snell cross-version upgrade/rollback: catalogue contains one trusted version', flush=True)


def interrupt_upgrade(controller, driver, version):
    request = task(controller, 'core_upgrade', {'version': version})
    apply = driver.apply_core
    pid = os.fork()
    if pid == 0:
        def stop_after_apply(*args, **kwargs):
            apply(*args, **kwargs)
            os.kill(os.getpid(), signal.SIGKILL)
        try:
            with patch.object(driver, 'apply_core', side_effect=stop_after_apply):
                controller.execute(request)
        finally:
            os._exit(1)
    _, status = os.waitpid(pid, 0)
    assert os.WIFSIGNALED(status) and os.WTERMSIG(status) == signal.SIGKILL
    assert controller.journal.exists() and controller.snapshot()['blocked']
    assert digest(driver.binary) == driver.asset(version)['sha256']
    recovered = controller.recover()
    assert recovered['status'] == 'rolled_back', (recovered, read(controller.root / 'last-error.json'))
    assert controller.execute(request) == recovered and not controller.journal.exists()


def run_version_switch(root, module):
    current, previous = VERSIONS[module]
    controller, driver = build(root, module)
    seed_previous(controller, driver, previous)
    assert assert_core(controller, driver, previous)['core']['candidates'] == [current]
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as target, socket.socket() as tcp_target:
        target.bind(('127.0.0.1', 0))
        target.settimeout(3)
        tcp_target.bind(target.getsockname())
        tcp_target.listen()
        tcp_target.settimeout(3)
        active, stopped = free_port(), free_port()
        while active == stopped:
            stopped = free_port()
        for port in (active, stopped):
            execute(controller, 'apply', resource(module, port, target.getsockname()[1]))
        command('systemctl', 'stop', driver.unit(stopped))
        assert not driver.running(stopped) and driver.enabled(stopped)
        original = controller.snapshot()
        contents = {path: path.read_bytes() for port in (active, stopped) for path in driver.files(port)}

        def verify(version, fallback=None):
            snapshot = assert_core(controller, driver, version, fallback)
            assert snapshot['resources'] == original['resources']
            for path, content in contents.items():
                assert path.read_bytes() == content, path
            assert driver.enabled(active) and driver.enabled(stopped) and not driver.running(stopped)
            pid = assert_instance(driver, active, snapshot['core']['sha256'])
            if module == 'fw':
                assert_tcp_relay(active, tcp_target)
                assert_udp_relay(active, target)
            return pid

        original_pid = verify(previous)
        rejected(controller, 'core_rollback', {'version': current}, code='core_version')
        rejected(controller, 'core_uninstall', code='core_in_use')
        assert verify(previous) == original_pid

        original_verify = driver.verify_core

        def fail_candidate_verification(state, backup):
            original_verify(state, backup)
            if state['core']['version'] == current:
                assert_instance(driver, active, driver.asset(current)['sha256'])
                raise Rejected('Injected failure after candidate startup')

        with patch.object(driver, 'verify_core', side_effect=fail_candidate_verification):
            execute(controller, 'core_upgrade', {'version': current}, status='rolled_back')
        verify(previous)

        interrupt_upgrade(controller, driver, current)
        previous_pid = verify(previous)

        execute(controller, 'core_upgrade', {'version': current})
        current_pid = verify(current, previous)
        assert current_pid != previous_pid
        rejected(controller, 'core_uninstall', code='core_in_use')
        execute(controller, 'core_rollback', {'version': previous})
        assert verify(previous) != current_pid

        for port in (active, stopped):
            execute(controller, 'delete', {'port': port})
        uninstall_and_reinstall(controller, driver, current)
    print(f'PASS {module.upper()} {previous} -> {current} -> {previous}: real executables, '
          'configs/credentials and stopped sibling preserved, verification failure and SIGKILL recovery, '
          'uninstall gate and reinstall', flush=True)


def run_pm(root):
    with pm_units(root, os.getpid(), os.stat('/proc/self/ns/net').st_ino):
        state = root / 'state' / 'pm'
        driver = pm.Driver(state, SYSTEMD)
        controller = PMController(state, driver, 'pm')
        pm.nft('add', 'table', 'inet', 'port_monitor')
        pm.nft('add', 'counter', 'inet', 'port_monitor', 'original')
        legacy = pm.entries('list', 'table', 'inet', 'port_monitor')
        pm.nft('add', 'table', *pm.TABLE.split())
        orphan = pm.entries('list', 'table', *pm.TABLE.split())
        rejected(controller, 'install', code='pm_conflict')
        assert pm.entries('list', 'table', *pm.TABLE.split()) == orphan
        pm.nft('delete', 'table', *pm.TABLE.split())
        execute(controller, 'install')
        assert driver.inspect() and driver.timer_active()
        command('systemctl', 'start', pm.SERVICE_NAME)
        port = free_port()
        rule = dict(port=port, name='Lifecycle policy', quota_gb=0, quota_mode='in_out',
                    reset_day=0, expiry_date='', egress_mbps=0)
        execute(controller, 'apply', rule)
        rejected(controller, 'core_uninstall', code='core_in_use')
        assert driver.inspect() and driver.timer_active()
        fw, fw_driver = build(root, 'fw')
        execute(fw, 'install')

        def uninstall_fw():
            return execute_module_task({'fw': fw, 'pm': controller}, {
                'request': task(fw, 'core_uninstall'), 'expires': time.time() + 60})

        result = uninstall_fw()
        assert result['status'] == 'rejected' and result['code'] == 'pm_dependency', result
        assert fw_driver.binary.exists()
        execute(controller, 'delete', {'port': port})
        result = uninstall_fw()
        assert result['status'] == 'succeeded' and not fw_driver.binary.exists(), result
        execute(controller, 'core_uninstall')
        assert not controller.snapshot()['installed'] and driver.inspect() is None
        assert not driver.timer_active()
        for path, _ in driver.unit_files():
            assert not path.exists()
        execute(controller, 'install')
        assert driver.inspect() and driver.timer_active()
        command('systemctl', 'start', pm.SERVICE_NAME)
        execute(controller, 'core_uninstall')
        assert pm.entries('list', 'table', 'inet', 'port_monitor') == legacy
    print('PASS PM: reject orphan table and nonempty rules, empty disable and restart with real nftables/timer, '
          'legacy table preserved', flush=True)


def cleanup(root):
    paths = sorted(SYSTEMD.glob(f'{root.name}-*'), key=lambda path: path.suffix != '.timer')
    if paths:
        for path in paths:
            command('systemctl', 'stop', path.name)
            if path.suffix != '.service' or not path.name.endswith('-pm.service'):
                command('systemctl', 'disable', path.name)
            path.unlink()
        command('systemctl', 'daemon-reload')
    shutil.rmtree(root)


def main(assets):
    if not __debug__:
        raise RuntimeError('Acceptance checks require Python assertions')
    if (os.geteuid() != 0 or not pathlib.Path('/run/systemd/system').is_dir()
            or os.stat('/proc/self/ns/net').st_ino == os.stat('/proc/1/ns/net').st_ino
            or any(row.get('table') for row in pm.entries('list', 'tables'))):
        raise RuntimeError('Linux systemd/root and a fresh network namespace are required')
    command('ip', 'link', 'set', 'lo', 'up')
    root = pathlib.Path('/opt') / f'node-manager-acceptance-{uuid.uuid4().hex[:12]}'
    root.mkdir(mode=0o755)
    try:
        with local_downloads(assets):
            run_snell(root)
            for module in ('sb', 'fw'):
                run_version_switch(root, module)
            run_pm(root)
    finally:
        cleanup(root)


if __name__ == '__main__':
    if len(sys.argv) == 5 and sys.argv[1] == '--maintain-pm':
        maintain_pm(pathlib.Path(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]))
    elif len(sys.argv) == 2:
        main(pathlib.Path(sys.argv[1]).resolve())
    else:
        raise SystemExit(__doc__)
