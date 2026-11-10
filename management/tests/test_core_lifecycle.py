from contextlib import ExitStack
import hashlib
import io
import os
import pathlib
import sys
import tempfile
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch
import zipfile

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from agent import execute_module_task
from controller import Controller
from modules import SPECS
import install
from modules import request
from protocol import Rejected
from core_driver import SNAPSHOT_CACHE_MIN_CTIME_AGE_NS, SNAPSHOT_CACHE_TTL_SECONDS, binary_hash, binary_identity, stream
from snell import Driver
from core_driver import atomic
from storage import read, write
from test_controller import Driver as FakeDriver


def sha(value):
    return hashlib.sha256(value).hexdigest()


class Interrupted(BaseException):
    pass


class Systemd:
    def __init__(self, driver):
        self.driver = driver
        self.active = set()
        self.calls = []
        self.reject_hash = None

    def command(self, tool, action, *args, **_kwargs):
        self.calls.append((tool, action, *args))
        if action == '--version':
            version = self.driver.test_versions[pathlib.Path(tool).read_bytes()]
            return SimpleNamespace(stdout=version.encode(), stderr=b'', returncode=0)
        if action == 'stop':
            self.active.remove(args[0])
        elif action == 'start':
            if sha(self.driver.binary.read_bytes()) == self.reject_hash:
                raise Rejected('实例启动失败', 'core_runtime')
            self.active.add(args[0])
        return SimpleNamespace(stdout=b'', returncode=0)

    def runtime(self, state):
        return {str(r['port']): 'active' if self.driver.unit(r['port']) in self.active else 'inactive'
                for r in state['resources']}

    def metadata(self, ports):
        return {p: {'LoadState': 'loaded', 'FragmentPath': str(self.driver.files(p)[1]), 'DropInPaths': ''}
                for p in ports}

    def mutations(self):
        return [call[1:] for call in self.calls if call[1] in ('start', 'stop')]


class CoreLifecycleTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = pathlib.Path(tmp.name)
        self.driver = Driver()
        self.driver.binary = self.root / 'snell-server'
        self.driver.configs = self.root / 'configs'
        self.driver.units = self.root / 'units'
        self.driver.configs.mkdir(mode=0o700)
        self.driver.units.mkdir()
        self.systemd = Systemd(self.driver)
        self.driver.runtime = self.systemd.runtime
        self.driver.unit_metadata = self.systemd.metadata
        self.driver.require_platform = lambda: None
        self.c = Controller(self.root, self.driver, SPECS['snell'])
        self.seq = 0

        self.binary = {'6.0.0': b'trusted original binary', '6.1.0': b'trusted newer binary'}
        self.driver.test_versions = {binary: version for version, binary in self.binary.items()}
        self.packages, versions = {}, {}
        for version, binary in self.binary.items():
            buffer = io.BytesIO()
            with zipfile.ZipFile(buffer, 'w') as archive:
                archive.writestr('snell-server', binary)
            url = f'https://dl.nssurge.com/snell/{version}.zip'
            self.packages[url] = buffer.getvalue()
            versions[version] = {'fixture-arch': {
                'url': url, 'archive_sha256': sha(buffer.getvalue()),
                'sha256': sha(binary), 'member': 'snell-server',
            }}
        contexts = ExitStack()
        self.addCleanup(contexts.close)
        contexts.enter_context(patch('core_driver.ASSETS', {'snell': versions}))
        contexts.enter_context(patch('core_driver.platform.machine', return_value='fixture-arch'))
        contexts.enter_context(patch('core_driver.command', side_effect=self.systemd.command))
        contexts.enter_context(patch('core_driver.time.sleep'))
        contexts.enter_context(patch('core_driver.socket.create_connection'))
        self.opener = contexts.enter_context(patch('core_driver.urllib.request.build_opener'))
        self.opener.return_value.open.side_effect = self.response

    def response(self, req, timeout):
        response = io.BytesIO(self.packages[req.full_url])
        response.url = req.full_url
        return response

    def req(self, action, **payload):
        self.seq += 1
        return {'id': f'{self.seq:032x}', 'action': action, 'payload': payload,
                'revision': self.c.snapshot()['revision']}

    def installed(self, with_instances=False):
        atomic(self.driver.binary, self.binary['6.0.0'], 0o755)
        resources = []
        if with_instances:
            for port in (12345, 12346):
                item = {'port': port, 'name': str(port), 'host': 'test.example', 'public_port': port,
                        'mode': 'default', 'psk': 'p' * 32}
                resources.append(item)
                config, unit = self.driver.files(port)
                atomic(config, self.driver.config(item).encode())
                atomic(unit, self.driver.service(port).encode(), 0o644)
            self.systemd.active.add(self.driver.unit(12345))
        core = {'version': '6.0.0', 'sha256': self.driver.asset('6.0.0')['sha256'], 'previous_version': None}
        state = {'installed': True, 'resources': resources, 'core': core}
        write(self.c.state, state)
        return state

    def test_snapshot_reuses_verified_binary_but_still_checks_instance_config(self):
        self.installed(with_instances=True)
        ctime = self.driver.binary.stat().st_ctime_ns
        with patch('core_driver.time.time_ns', return_value=ctime) as wall_clock, \
                patch('core_driver.time.monotonic', return_value=0) as clock, \
                patch('core_driver.binary_hash', wraps=binary_hash) as digest:
            self.assertFalse(self.c.snapshot()['blocked'])
            wall_clock.return_value = ctime + SNAPSHOT_CACHE_MIN_CTIME_AGE_NS
            self.assertFalse(self.c.snapshot()['blocked'])
            self.assertFalse(self.c.snapshot()['blocked'])
            config, _ = self.driver.files(12345)
            config.write_text('edited outside the controller')
            self.assertTrue(self.c.snapshot()['blocked'])
            self.assertEqual(digest.call_count, 2)
            clock.return_value = SNAPSHOT_CACHE_TTL_SECONDS
            self.assertTrue(self.c.snapshot()['blocked'])
            self.assertEqual(digest.call_count, 3)

    def test_snapshot_detects_same_size_edit_and_uninstall_rechecks_digest(self):
        self.installed()
        binary = self.driver.binary
        before = binary.stat()
        settled = before.st_ctime_ns + SNAPSHOT_CACHE_MIN_CTIME_AGE_NS
        with patch('core_driver.time.time_ns', return_value=settled):
            req = self.req('core_uninstall')
            with patch('core_driver.binary_hash', return_value='incorrect digest') as digest:
                with self.assertRaisesRegex(Rejected, '可信安装状态不一致'):
                    self.c.execute(req)
            digest.assert_called_once_with(binary)

        with patch('core_driver.time.time_ns', return_value=before.st_ctime_ns) as clock:
            self.assertFalse(self.c.snapshot()['blocked'])
            binary.write_bytes(b'x' * before.st_size)
            os.utime(binary, ns=(before.st_atime_ns, before.st_mtime_ns))
            # ext4 can expose the very same attributes for both writes.
            with patch('core_driver.binary_identity', return_value=binary_identity(before)), \
                    patch('core_driver.binary_hash', wraps=binary_hash) as digest:
                self.assertTrue(self.c.snapshot()['blocked'])
                clock.return_value = binary.stat().st_ctime_ns + SNAPSHOT_CACHE_MIN_CTIME_AGE_NS
                self.assertTrue(self.c.snapshot()['blocked'])
            self.assertEqual(digest.call_count, 2)

    def test_snapshot_rejects_unsafe_or_unstable_binary_metadata(self):
        self.installed()
        binary = self.driver.binary
        self.assertFalse(self.c.snapshot()['blocked'])
        binary.chmod(0o775)
        self.assertTrue(self.c.snapshot()['blocked'])
        binary.chmod(0o755)

        def hash_with_mode_change(*args):
            result = stream(*args)
            binary.chmod(0o700)
            return result

        with patch('core_driver.stream', side_effect=hash_with_mode_change):
            self.assertTrue(self.c.snapshot()['blocked'])
        self.assertFalse(self.c.snapshot()['blocked'])
        target = binary.with_suffix('.trusted')
        binary.rename(target)
        binary.symlink_to(target)
        self.assertTrue(self.c.snapshot()['blocked'])
        self.assertIsNone(self.c.snapshot()['core']['version'])

    def test_upgrade_and_rollback_preserve_running_set_and_secrets(self):
        before = self.installed(with_instances=True)
        snapshot = self.c.snapshot()
        self.assertEqual(snapshot['core']['version'], '6.0.0')
        self.assertEqual(snapshot['core']['candidates'], ['6.1.0'])
        paths = [path for r in before['resources'] for path in self.driver.files(r['port'])]
        files = {path: (path.read_bytes(), path.stat().st_ino) for path in paths}
        first = self.req('core_upgrade', version='6.1.0')
        self.assertEqual(self.c.execute(first)['status'], 'succeeded')
        self.assertNotEqual(self.c.snapshot()['revision'], snapshot['revision'])
        self.assertEqual(self.c.snapshot()['core']['previous_version'], '6.0.0')
        self.assertEqual(self.c.execute(first)['status'], 'succeeded')
        self.assertEqual(self.opener.return_value.open.call_count, 1)
        self.assertEqual(self.c.execute(self.req('core_rollback', version='6.0.0'))['status'], 'succeeded')
        self.assertEqual(read(self.c.state)['resources'], before['resources'])
        self.assertEqual(self.c.snapshot()['core']['version'], '6.0.0')
        self.assertIsNone(self.c.snapshot()['core']['previous_version'])
        self.assertEqual({path: (path.read_bytes(), path.stat().st_ino) for path in paths}, files)
        active = self.driver.unit(12345)
        self.assertEqual(self.systemd.mutations(), [('stop', active), ('start', active)] * 2)
        self.assertEqual(self.systemd.active, {active})
        self.assertFalse(list(self.root.glob('.node-manager-snell-server-*')))

    def test_failed_archive_and_candidate_configuration_leave_services_alone(self):
        before = self.installed(with_instances=True)
        url = self.driver.asset('6.1.0')['url']
        good = self.packages[url]
        self.packages[url] = b'corrupt archive'
        with self.assertRaises(Rejected) as failure:
            self.c.execute(self.req('core_upgrade', version='6.1.0'))
        self.assertEqual(failure.exception.code, 'core_download')
        self.assertFalse(self.c.journal.exists())
        self.packages[url] = good
        with patch.object(self.driver, 'check_core_candidate', side_effect=Rejected('不兼容', 'invalid_config')):
            self.assertEqual(self.c.execute(self.req('core_upgrade', version='6.1.0'))['code'], 'invalid_config')
        self.assertEqual(self.systemd.mutations(), [])
        self.assertEqual(read(self.c.state), before)
        self.assertEqual(self.driver.binary.read_bytes(), self.binary['6.0.0'])
        self.assertFalse(list(self.root.glob('.node-manager-snell-server-*')))

    def test_exit_during_download_preserves_observed_inactive_or_failed_state(self):
        for stopped_state in ('inactive', 'failed'):
            with self.subTest(stopped_state=stopped_state):
                before = self.installed(with_instances=True)
                self.systemd.calls.clear()
                req = self.req('core_upgrade', version='6.1.0')

                def exits_during_download(request, timeout):
                    self.systemd.active.clear()
                    return self.response(request, timeout)

                def runtime(state):
                    if self.systemd.active:
                        return self.systemd.runtime(state)
                    return {str(r['port']): stopped_state if r['port'] == 12345 else 'inactive'
                            for r in state['resources']}

                self.opener.return_value.open.side_effect = exits_during_download
                with patch.object(self.driver, 'runtime', side_effect=runtime):
                    result = self.c.execute(req)
                    self.assertEqual((result['status'], result['code']), ('rolled_back', 'config_changed'))
                    self.assertEqual(result['message'], '核心切换前已中止，保留当前配置和运行状态')
                    self.assertEqual(self.c.execute(req), result)
                    self.assertEqual(self.c.snapshot()['runtime']['12345'], stopped_state)
                self.assertEqual(self.systemd.mutations(), [])
                self.assertEqual(self.driver.binary.read_bytes(), self.binary['6.0.0'])
                self.assertEqual(read(self.c.state), before)
                self.assertFalse(self.c.journal.exists())

    def test_mutation_marker_write_failure_does_not_restart_exited_service(self):
        import controller

        write_journal = controller.write
        for published in (False, True):
            with self.subTest(published=published):
                before = self.installed(with_instances=True)
                self.systemd.calls.clear()
                req = self.req('core_upgrade', version='6.1.0')

                def fail_marker(path, value):
                    if path == self.c.journal and value.get('backup', {}).get('mutation_started'):
                        self.systemd.active.clear()
                        if published:
                            write_journal(path, value)
                        raise OSError('journal write interrupted')
                    write_journal(path, value)

                with patch('controller.write', side_effect=fail_marker):
                    result = self.c.execute(req)
                self.assertEqual(result['status'], 'rolled_back')
                self.assertEqual(self.systemd.mutations(), [])
                self.assertEqual(read(self.c.state), before)
                self.assertEqual(self.driver.binary.read_bytes(), self.binary['6.0.0'])
                self.assertFalse(self.c.journal.exists())

    def test_pre_switch_interruption_recovery_preserves_exited_service(self):
        self.installed(with_instances=True)

        def interrupt_candidate(*_args):
            self.systemd.active.clear()
            raise Interrupted

        with patch.object(self.driver, 'check_core_candidate', side_effect=interrupt_candidate):
            with self.assertRaises(Interrupted):
                self.c.execute(self.req('core_upgrade', version='6.1.0'))
        self.assertIs(read(self.c.journal)['backup']['mutation_started'], False)
        self.assertEqual(Controller(self.root, self.driver, SPECS['snell']).recover()['status'], 'rolled_back')
        self.assertEqual(self.systemd.mutations(), [])
        self.assertEqual(self.systemd.active, set())
        self.assertFalse(self.c.journal.exists())

    def test_stop_interruption_restores_services(self):
        before = self.installed(with_instances=True)
        self.systemd.calls.clear()

        def interrupt_stop(tool, action, *args, **kwargs):
            if action == 'stop':
                self.assertIs(read(self.c.journal)['backup']['mutation_started'], True)
                self.systemd.command(tool, action, *args, **kwargs)
                raise Interrupted
            return self.systemd.command(tool, action, *args, **kwargs)

        with patch('core_driver.command', side_effect=interrupt_stop):
            with self.assertRaises(Interrupted):
                self.c.execute(self.req('core_upgrade', version='6.1.0'))
        result = Controller(self.root, self.driver, SPECS['snell']).recover()
        active = self.driver.unit(12345)
        self.assertEqual(result['status'], 'rolled_back')
        self.assertEqual(self.systemd.mutations(), [('stop', active), ('start', active)])
        self.assertEqual(self.systemd.active, {active})
        self.assertEqual(read(self.c.state), before)
        self.assertEqual(self.driver.binary.read_bytes(), self.binary['6.0.0'])

    def test_start_failure_restores_original_bytes_and_running_set(self):
        before = self.installed(with_instances=True)
        self.systemd.reject_hash = sha(self.binary['6.1.0'])
        outcome = self.c.execute(self.req('core_upgrade', version='6.1.0'))
        self.assertEqual((outcome['status'], outcome['code']), ('rolled_back', 'core_runtime'))
        self.assertEqual(self.driver.binary.read_bytes(), self.binary['6.0.0'])
        self.assertEqual(read(self.c.state), before)
        self.assertEqual(self.systemd.active, {self.driver.unit(12345)})
        self.assertFalse(self.c.journal.exists())
        self.assertFalse(list(self.root.glob('.node-manager-snell-server-*')))

    def test_prepared_recovery_restores_offline_and_committed_recovery_keeps_candidate(self):
        self.installed(with_instances=True)
        apply = self.driver.apply_core

        def crash(*args):
            apply(*args)
            raise Interrupted

        with patch.object(self.driver, 'apply_core', side_effect=crash):
            with self.assertRaises(Interrupted):
                self.c.execute(self.req('core_upgrade', version='6.1.0'))
        self.assertEqual(self.driver.binary.read_bytes(), self.binary['6.1.0'])
        restarted = Controller(self.root, self.driver, SPECS['snell'])
        self.opener.side_effect = AssertionError('recovery must not download')
        self.assertEqual(restarted.recover()['status'], 'rolled_back')
        self.assertEqual(self.driver.binary.read_bytes(), self.binary['6.0.0'])
        self.opener.side_effect = None
        with patch.object(self.c, '_commit', side_effect=Interrupted):
            with self.assertRaises(Interrupted):
                self.c.execute(self.req('core_upgrade', version='6.1.0'))
        self.assertEqual(read(self.c.journal)['phase'], 'committed')
        self.opener.side_effect = AssertionError('recovery must not download')
        self.assertEqual(restarted.recover()['status'], 'succeeded')
        self.assertEqual(self.driver.binary.read_bytes(), self.binary['6.1.0'])
        self.assertEqual(restarted.snapshot()['core']['previous_version'], '6.0.0')

    def test_install_latest_and_uninstall_only_after_ownership_checks(self):
        self.assertEqual(self.c.execute(self.req('install'))['status'], 'succeeded')
        self.assertEqual(self.c.snapshot()['core']['version'], '6.1.0')
        unknown = self.driver.units / self.driver.unit(4321)
        unknown.write_text('unmanaged')
        with self.assertRaisesRegex(Rejected, '未登记'):
            self.c.execute(self.req('core_uninstall'))
        self.assertTrue(unknown.exists())
        unknown.unlink()
        pm = Controller(self.root / 'pm', FakeDriver(), SPECS['pm'])
        write(pm.state, {'installed': True, 'resources': [{'port': 12345}]})
        result = execute_module_task({'snell': self.c, 'pm': pm},
                                     {'request': self.req('core_uninstall'), 'expires': time.time() + 30})
        self.assertEqual(result['code'], 'pm_dependency')
        self.assertEqual(self.c.execute(self.req('core_uninstall'))['status'], 'succeeded')
        self.assertFalse(self.driver.binary.exists())
        self.assertFalse(self.c.snapshot()['installed'])

    def test_refuses_untrusted_binary_and_versions(self):
        self.installed(with_instances=True)
        with self.assertRaises(Rejected) as failure:
            self.c.execute(self.req('core_uninstall'))
        self.assertEqual(failure.exception.code, 'core_in_use')
        for value in ('6.0.0', '7.0.0', 'https://example.com/6.1.0'):
            with self.assertRaises(Rejected) as failure:
                self.c.execute(self.req('core_upgrade', version=value))
            self.assertEqual(failure.exception.code, 'core_version')
        with self.assertRaises(Rejected):
            request(self.req('core_upgrade', version='6.1.0', url='https://example.com/'))
        atomic(self.driver.binary, b'unrecognized binary', 0o755)
        snapshot = self.c.snapshot()
        self.assertTrue(snapshot['blocked'])
        self.assertIsNone(snapshot['core']['version'])
        self.assertEqual(self.systemd.mutations(), [])

    def test_fixed_temp_name_collision_during_download_preserves_external_file(self):
        self.installed()
        unknown = self.root / '.node-manager-snell-server.candidate'

        def collide(req, timeout):
            unknown.write_bytes(b'external file')
            return self.response(req, timeout)

        self.opener.return_value.open.side_effect = collide
        self.assertEqual(self.c.execute(self.req('core_upgrade', version='6.1.0'))['status'], 'succeeded')
        self.assertEqual(unknown.read_bytes(), b'external file')
        self.assertEqual(list(self.root.glob('.node-manager-snell-server-*')), [])

    def test_start_rate_limit_is_reset_only_for_the_original_active_unit(self):
        self.installed(with_instances=True)
        active = self.driver.unit(12345)
        attempts = 0

        def systemd(tool, action, *args, **kwargs):
            nonlocal attempts
            if action == 'start':
                attempts += 1
                if attempts == 1:
                    raise Rejected('启动超出限速')
            if action == 'show':
                return SimpleNamespace(returncode=0, stdout=b'start-limit-hit\n')
            return self.systemd.command(tool, action, *args, **kwargs)

        with patch('core_driver.command', side_effect=systemd):
            self.assertEqual(self.c.execute(self.req('core_upgrade', version='6.1.0'))['status'], 'succeeded')
        self.assertEqual(attempts, 2)
        self.assertEqual([call for call in self.systemd.calls if call[1] == 'reset-failed'],
                         [('systemctl', 'reset-failed', active)])
        self.assertEqual(self.systemd.active, {active})

    def test_candidate_loader_failure_is_rejected_before_stopping_services(self):
        self.installed(with_instances=True)

        def fail_probe(tool, action, *args, **kwargs):
            if action == '--version':
                return SimpleNamespace(returncode=1, stdout=b'', stderr=b'loader unavailable')
            return self.systemd.command(tool, action, *args, **kwargs)

        with patch('core_driver.command', side_effect=fail_probe):
            result = self.c.execute(self.req('core_upgrade', version='6.1.0'))
        self.assertEqual((result['status'], result['code']), ('rolled_back', 'core_runtime'))
        self.assertEqual(self.systemd.mutations(), [])
        self.assertEqual(self.c.snapshot()['core']['version'], '6.0.0')

    def test_component_switch_checks_older_core_and_rejects_losing_rollback_target(self):
        self.installed()
        manifest = {'core_assets.json': b'{"snell":{"6.0.0":{"fixture-arch":{"sha256":"'
                    + sha(self.binary['6.0.0']).encode() + b'"}}}}'}
        with patch.object(install, 'ROOT', self.root), patch.object(install, 'CODE', self.root):
            install.check_core_versions(manifest)
            with self.assertRaisesRegex(ValueError, '无法确认'):
                install.check_core_versions({'core_assets.json': b'{"snell":{}}'})
            self.c.execute(self.req('core_upgrade', version='6.1.0'))
            legacy = {'core_assets.json': b'{}', 'snell.py': sha(self.binary['6.1.0']).encode()}
            with self.assertRaisesRegex(ValueError, '回退版本'):
                install.check_core_versions(legacy)


if __name__ == '__main__':
    unittest.main()
