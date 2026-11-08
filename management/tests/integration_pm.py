"""Run only as root in a fresh network namespace: unshare -n python3 integration_pm.py."""
import datetime as dt
import os
import pathlib
import socket
import subprocess
import sys
import tempfile
from unittest.mock import Mock, patch

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from controller import Controller
from protocol import Rejected, revision
import pm
from pm import Driver, TABLE, entries, nft, plan
from storage import read, write


def ip(*args, pid=None):
    prefix = ['nsenter', '-t', str(pid), '-n'] if pid else []
    subprocess.run([*prefix, 'ip', *args], check=True)


def task(controller, action, payload, number):
    state = read(controller.state, {'installed': False, 'resources': []})
    return controller.execute(dict(id=f'{number:032x}', module='pm', action=action,
                                   payload=payload, revision=revision(state)))


def main():
    if os.stat('/proc/self/ns/net').st_ino == os.stat('/proc/1/ns/net').st_ino:
        raise SystemExit('Refusing to run outside a separate network namespace')
    if any(row.get('table') for row in entries('list', 'tables')):
        raise SystemExit('Refusing to run outside a clean network namespace')
    port = 23456
    peer = subprocess.Popen(['unshare', '-n', sys.executable, '-u', '-c', """
import socket, sys
print('ready')
for line in sys.stdin:
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client:
        client.settimeout(1)
        client.sendto(b'x' * int(line), ('192.0.2.1', 23456))
        try: print(client.recv(1024).decode())
        except socket.timeout: print('timeout')
"""], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        assert peer.stdout.readline().strip() == 'ready'
        ip('link', 'add', 'pm-host', 'type', 'veth', 'peer', 'name', 'pm-peer')
        ip('link', 'set', 'pm-peer', 'netns', str(peer.pid))
        ip('addr', 'add', '192.0.2.1/24', 'dev', 'pm-host')
        ip('link', 'set', 'pm-host', 'up')
        ip('addr', 'add', '192.0.2.2/24', 'dev', 'pm-peer', pid=peer.pid)
        ip('link', 'set', 'pm-peer', 'up', pid=peer.pid)
        nft('add', 'table', 'inet', 'port_monitor')
        nft('add', 'counter', 'inet', 'port_monitor', 'original')
        legacy = entries('list', 'table', 'inet', 'port_monitor')[1:]
        with tempfile.TemporaryDirectory() as temporary, socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as server:
            root = pathlib.Path(temporary)
            units = root / 'units'
            units.mkdir()
            driver = Driver(root / 'pm', units)
            controller = Controller(root / 'pm', driver, 'pm')
            server.bind(('192.0.2.1', port))
            server.settimeout(0.3)

            def traffic(size, blocked=False, reply_blocked=False):
                peer.stdin.write(f'{size}\n')
                peer.stdin.flush()
                try:
                    data, address = server.recvfrom(2048)
                    assert not blocked and len(data) == size
                    try:
                        server.sendto(b'ok', address)
                    except PermissionError:
                        assert reply_blocked, 'unexpected egress policy rejection'
                except socket.timeout:
                    assert blocked
                assert peer.stdout.readline().strip() == ('timeout' if blocked or reply_blocked else 'ok')

            def unit_info(name):
                path = units / name
                return {'LoadState': 'loaded' if path.exists() else 'not-found',
                        'FragmentPath': str(path) if path.exists() else '', 'DropInPaths': '', 'ActiveState': 'inactive'}

            def unit_states():
                return {name: {**unit_info(name), 'UnitFileState': 'enabled'} for name in ('node-manager-pm.service', 'node-manager-pm.timer')}

            with patch('pm.command'), patch.object(driver, 'timer_active', return_value=True), patch.object(driver, 'unit_info', side_effect=unit_info), \
                    patch.object(driver, 'unit_states', side_effect=unit_states):
                assert task(controller, 'install', {}, 1)['status'] == 'succeeded'
                rule = dict(port=port, name='Test', quota_gb=0, quota_mode='in_out',
                            reset_day=0, expiry_date='', egress_mbps=0)
                assert task(controller, 'apply', rule, 2)['status'] == 'succeeded'
                traffic(100)
                initial = driver.inspect()['values'][str(port)]
                assert initial['rx'] >= 100 and initial['tx'] > 0, initial
                assert task(controller, 'apply', {**rule, 'name': 'Renamed'}, 3)['status'] == 'succeeded'
                assert driver.inspect()['values'][str(port)] == initial

                # Blocking with the kernel: limit 1 alarms at 4, so the 5th address to come
                # online (the peer) is dropped from its own table, before the PM counters.
                clock = Mock(time=Mock(side_effect=range(1000, 10 ** 6, 60)))

                def minutes(rule, *rounds):
                    write(controller.state, {**read(controller.state), 'resources': [rule]})
                    with patch.object(pm, 'time', clock), patch('pm.reality_probes', return_value={}):
                        for hosts in rounds:
                            ss = ''.join(f'0 0 192.0.2.1:{port} {host}:40000\n' for host in hosts)
                            with patch('pm.connections', return_value=ss):
                                driver.watch_ips(read(controller.state))

                others = [f'198.51.100.{n}' for n in range(1, 5)]
                limit = {'max_ips': 1, 'cooldown_min': 30, 'whitelist': [], 'block': True}
                blocking = {**rule, 'name': 'Renamed', 'ip_limit': limit}
                assert task(controller, 'apply', blocking, 30)['status'] == 'succeeded'
                minutes(read(controller.state)['resources'][0], others, others, others + ['192.0.2.2'], others + ['192.0.2.2'])
                assert driver.blocked() == {('192.0.2.2', str(port))}, driver.blocked()
                before = driver.inspect()['values'][str(port)]
                traffic(100, blocked=True)
                assert driver.inspect()['values'][str(port)] == before, 'blocked traffic is not metered'
                driver.sample(driver.inspect(), read(driver.checkpoint))  # the PM table is unchanged
                listed = {**blocking, 'ip_limit': {**limit, 'whitelist': ['192.0.2.2']}}
                assert task(controller, 'apply', listed, 31)['status'] == 'succeeded'
                minutes(read(controller.state)['resources'][0], others + ['192.0.2.2'])
                assert driver.blocked() == set(), 'a whitelisted address is let go at once'
                traffic(100)
                assert task(controller, 'apply', blocking, 32)['status'] == 'succeeded'
                minutes(read(controller.state)['resources'][0], others + ['192.0.2.2'], others + ['192.0.2.2'])
                assert driver.blocked() == {('192.0.2.2', str(port))}, driver.blocked()
                raised = {**blocking, 'ip_limit': {**limit, 'max_ips': 2}}
                assert task(controller, 'apply', raised, 33)['status'] == 'succeeded'
                minutes(read(controller.state)['resources'][0], others + ['192.0.2.2'])
                assert driver.blocked() == set(), 'a raised limit lets the port go at once'
                assert task(controller, 'apply', {**rule, 'name': 'Renamed'}, 34)['status'] == 'succeeded'
                minutes(read(controller.state)['resources'][0], others)
                assert driver.blocked() is None, 'no blocking rule, no table'
                initial = driver.inspect()['values'][str(port)]
                assert task(controller, 'reset', {'port': port}, 4)['status'] == 'succeeded'
                assert driver.inspect()['values'][str(port)] == {'rx': 0, 'tx': 0}

                # Use a small byte limit to test the actual kernel enforcement without 1 GiB of traffic.
                current = read(controller.state)
                live = driver.inspect()
                saved = driver.sample(live, read(driver.checkpoint))
                current['resources'][0]['quota_gb'] = 1
                with patch('pm.GIB', 80):
                    driver.replace(live, saved, plan(current, dt.datetime.now(dt.timezone.utc).date()))
                    write(controller.state, current)
                    traffic(100, blocked=True)
                    assert driver.status(current)[1][str(port)]['alert'] == 'quota_reached'
                assert task(controller, 'reset', {'port': port}, 5)['status'] == 'succeeded'
                traffic(100)

                # Force a verify failure after a real kernel mutation. The controller must restore.
                original_verify = driver.verify
                verify_calls = 0

                def fail_once(state, payload):
                    nonlocal verify_calls
                    verify_calls += 1
                    if verify_calls == 2:
                        raise Rejected('forced post-apply verification failure')
                    return original_verify(state, payload)

                for number, action in ((6, 'reset'), (7, 'delete')):
                    before = driver.inspect()['values'][str(port)]
                    verify_calls = 0
                    with patch.object(driver, 'verify', side_effect=fail_once):
                        assert task(controller, action, {'port': port}, number)['status'] == 'rolled_back'
                    assert not controller.journal.exists()
                    assert driver.inspect()['values'][str(port)] == before, action
                    traffic(100)
                # A rollback can commit its kernel change before final verification is interrupted.
                old_usage = driver.inspect()['values'][str(port)]
                verify_calls = 0

                def fail_before_receipt(state, payload):
                    nonlocal verify_calls
                    verify_calls += 1
                    if verify_calls in (2, 3):
                        raise Rejected('forced failure before durable rollback receipt')
                    return original_verify(state, payload)

                with patch.object(driver, 'verify', side_effect=fail_before_receipt):
                    assert task(controller, 'reset', {'port': port}, 8)['status'] == 'recovery_failed'
                assert controller.recover()['status'] == 'rolled_back'
                assert driver.inspect()['values'][str(port)] == old_usage

                # Reboot after persisting a successful operation, before acknowledging its journal.
                state = read(controller.state)
                driver.boot_id = root / 'fake-boot-id'
                driver.boot_id.write_text('boot after committed journal')
                write(controller.journal, {'id': 'f' * 32, 'digest': 'committed-test', 'after': state, 'phase': 'committed'})
                nft('delete', 'table', *TABLE.split())
                assert controller.recover()['status'] == 'succeeded'
                assert driver.inspect()['values'][str(port)] == old_usage
                traffic(100)

                with patch('pm.GIB', 80):
                    quota_rule = {**rule, 'quota_gb': 1}
                    assert task(controller, 'apply', {**quota_rule, 'expiry_date': '2000-01-01'}, 9)['status'] == 'succeeded'
                    assert task(controller, 'reset', {'port': port}, 10)['status'] == 'succeeded'
                    traffic(100, blocked=True)
                    assert driver.status(read(controller.state))[1][str(port)]['alert'] == 'expired'
                    assert task(controller, 'apply', quota_rule, 11)['status'] == 'succeeded'
                    assert driver.status(read(controller.state))[1][str(port)]['alert'] == 'quota_reached'
                    traffic(100, blocked=True)

                    # Detect an external quota reset even though rule structure and counters are unchanged.
                    driver.maintain()
                    nft('reset', 'quota', *TABLE.split(), f'quota_{port}')
                    assert driver.status(state) == (True, {})
                    # Explicit repair never overwrites an existing table, including a drifted owned table.
                    try:
                        driver.repair_missing()
                    except Rejected:
                        pass
                    else:
                        raise AssertionError('existing PM table must be left intact')
                    nft('delete', 'table', *TABLE.split())
                    driver.repair_missing()
                    traffic(100, blocked=True)
                with patch('pm.GIB', 20):
                    out_only = {**rule, 'quota_gb': 1, 'quota_mode': 'out_only'}
                    assert task(controller, 'apply', out_only, 12)['status'] == 'succeeded'
                    assert task(controller, 'reset', {'port': port}, 13)['status'] == 'succeeded'
                    traffic(100, reply_blocked=True)
                    assert driver.status(read(controller.state))[1][str(port)]['alert'] == 'quota_reached'
                with patch('pm.BYTES_PER_MEGABIT', 1):
                    assert task(controller, 'apply', {**rule, 'egress_mbps': 1}, 14)['status'] == 'succeeded'
                    traffic(100, reply_blocked=True)
                assert task(controller, 'delete', {'port': port}, 15)['status'] == 'succeeded'
                traffic(100)
                driver.disable_empty()
                assert driver.inspect() is None
                assert not read(controller.state)['installed']
        assert entries('list', 'table', 'inet', 'port_monitor')[1:] == legacy
        print('PASS isolated Linux PM: install, counters, counter continuity, blocking (drop before metering, whitelist release, table removal), bidirectional/outbound quota, outbound policing, reset, rollback retry, reboot, expiry/renewal, legacy coexistence and cleanup')
    finally:
        peer.kill()
        peer.wait()


if __name__ == '__main__':
    main()
