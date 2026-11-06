import contextlib
import io
import importlib.util
import json
import pathlib
import sqlite3
import sys
import tempfile
import time
import types
import unittest
from unittest.mock import patch

MANAGEMENT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(MANAGEMENT))
import install

sys.path.insert(0, str(MANAGEMENT.parent / 'scripts'))
spec = importlib.util.spec_from_file_location('hub_lifecycle', MANAGEMENT.parent / 'scripts/hub-lifecycle.py')
hub_lifecycle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hub_lifecycle)


COMPONENT_FILES = frozenset(path.name for path in MANAGEMENT.iterdir() if path.suffix in ('.py', '.json') and not path.name.startswith('.'))
OLDER_FILES = COMPONENT_FILES - {'core_assets.json'}


class ComponentLifecycleTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = pathlib.Path(temp.name) / 'state'
        self.code = pathlib.Path(temp.name) / 'code'
        install.prepare_directories(self.root, self.code, self.root.parent.stat().st_uid)
        self.purge_journal = self.root.parent / 'purge.json'
        self.units = self.root.parent / 'units'
        self.units.mkdir()
        context = patch.multiple(install, ROOT=self.root, CODE=self.code, UNITS=self.units,
                                 PURGE_JOURNAL=self.purge_journal)
        context.start()
        self.addCleanup(context.stop)
        self.before = {name: b'# previous\n' for name in OLDER_FILES}
        self.after = {name: b'# next\n' for name in COMPONENT_FILES}
        self.before_manifest = {name: install.digest(data) for name, data in self.before.items()}
        self.after_manifest = {name: install.digest(data) for name, data in self.after.items()}
        install.write_version(self.before_manifest, self.before)
        self.binding = {'server': 'https://panel.example.com', 'credential': 'test'}

    def test_unknown_manifest_entry_and_symlink_are_refused(self):
        with self.assertRaises(ValueError):
            install.manifest({**self.after_manifest, '../other': 'a' * 64})
        path = self.code / 'pm.py'
        path.unlink()
        path.symlink_to(self.code / 'agent.py')
        with self.assertRaises(ValueError):
            install.check_current(self.before_manifest)

    def test_unverified_receipt_refuses_before_service_changes(self):
        install.write_json(self.root / 'pending-result.json', {'id': 'still-pending'})
        with patch.object(install, 'check_agent_unit', return_value={'ActiveState': 'active'}), \
                patch.object(install, 'stop_runtimes') as stop:
            with self.assertRaisesRegex(ValueError, '回执'):
                install.change(self.binding, self.before_manifest, self.after_manifest, self.after)
        stop.assert_not_called()

    def test_failed_start_restores_legacy_release_and_retains_backup(self):
        phases = []

        def start(_record, _binding):
            phases.append(install.read_json(self.root / install.JOURNAL)['phase'])
            if len(phases) == 1:
                install.check_current(self.after_manifest)
                raise ValueError('new agent unhealthy')
            install.check_current(self.before_manifest)

        with patch.object(install, 'check_agent_unit', return_value={'ActiveState': 'active'}), \
                patch.object(install, 'pm_info', return_value=({}, False)), \
                patch.object(install, 'maintenance', return_value={'owned': True}), \
                patch.object(install, 'stop_runtimes'), \
                patch.object(install, 'start_runtimes', side_effect=start):
            with self.assertRaisesRegex(ValueError, '已恢复原版本'):
                install.change(self.binding, self.before_manifest, self.after_manifest, self.after)
        self.assertEqual(phases, ['starting', 'starting'])
        self.assertFalse((self.root / install.JOURNAL).exists())
        backup = next((self.root / 'component-backups').iterdir())
        self.assertEqual(install.read_version(backup / 'before', self.before_manifest), self.before)
        self.assertEqual(install.read_json(self.root / install.MANIFEST), self.before_manifest)

    def test_failed_instance_refresh_restores_previous_release(self):
        refreshed = []

        def refresh():
            refreshed.append(install.read_json(self.root / install.MANIFEST))
            if len(refreshed) == 1:
                raise ValueError('template rejected')

        with patch.object(install, 'check_agent_unit', return_value={'ActiveState': 'active'}), \
                patch.object(install, 'pm_info', return_value=({}, False)), \
                patch.object(install, 'maintenance', return_value={'owned': True}), \
                patch.object(install, 'stop_runtimes'), patch.object(install, 'start_runtimes') as start, \
                patch.object(install, 'refresh_instances', side_effect=refresh):
            with self.assertRaisesRegex(ValueError, '已恢复原版本'):
                install.change(self.binding, self.before_manifest, self.after_manifest, self.after)
        # The second pass rewrites instances for the restored code, then only that starts.
        self.assertEqual(refreshed, [self.after_manifest, self.before_manifest])
        self.assertEqual(start.call_count, 1)
        install.check_current(self.before_manifest)

    def test_manual_instance_edit_stops_the_upgrade_before_anything_changes(self):
        with patch.object(install, 'check_agent_unit', return_value={'ActiveState': 'active'}), \
                patch.object(install, 'pm_info', return_value=({}, False)), \
                patch.object(install, 'check_unmodified', side_effect=ValueError('实例配置被手工修改')), \
                patch.object(install, 'stop_runtimes') as stop, patch.object(install, 'maintenance') as lease:
            with self.assertRaisesRegex(ValueError, '手工修改'):
                install.change(self.binding, self.before_manifest, self.after_manifest, self.after)
        stop.assert_not_called()
        lease.assert_not_called()
        install.check_current(self.before_manifest)

    def test_manifest_follows_the_release_without_an_installer_change(self):
        added = {**self.after_manifest, 'new_module.py': 'a' * 64}
        self.assertEqual(install.manifest(added), added)
        for invalid in ({'new_module.py': 'a' * 64}, {'agent.py': 'short'}, {'agent.py': 'a' * 64, 'a/b.py': 'a' * 64}, {'agent.py': 'a' * 64, 'run.sh': 'a' * 64}, []):
            with self.assertRaises(ValueError):
                install.manifest(invalid)

    def test_switch_removes_files_the_new_release_dropped(self):
        added = {'agent.py': b'# a\n', 'new_module.py': b'# n\n'}
        added_manifest = {name: install.digest(data) for name, data in added.items()}
        install.write_version(added_manifest, added)
        self.assertEqual(sorted(path.name for path in self.code.iterdir()), ['agent.py', 'new_module.py'])
        install.write_version(self.before_manifest, self.before)
        self.assertFalse((self.code / 'new_module.py').exists())
        install.check_current(self.before_manifest)

    def test_lost_lease_response_retains_prepared_token(self):
        with patch.object(install, 'check_agent_unit', return_value={'ActiveState': 'active'}), \
                patch.object(install, 'pm_info', return_value=({}, False)), \
                patch.object(install, 'maintenance', side_effect=[TimeoutError(), {'owned': True}]), \
                patch.object(install, 'stop_runtimes') as stop:
            with self.assertRaises(TimeoutError):
                install.change(self.binding, self.before_manifest, self.after_manifest, self.after)
        stop.assert_not_called()
        record = install.read_json(self.root / install.JOURNAL)
        self.assertEqual(record['phase'], 'prepared')
        self.assertRegex(record['token'], r'^[0-9a-f]{64}$')

    def test_uninstall_default_preserves_business_files_and_state(self):
        units = self.root / 'units'
        units.mkdir()
        install.write_file(units / install.SERVICE, install.SERVICE_UNIT.encode(), 0o644)
        business = self.code / 'snell-server'
        business.write_bytes(b'preserve me')
        state = self.root / 'state.json'
        install.write_json(state, {'installed': True, 'resources': [{'port': 1234}]})
        with patch.object(install, 'UNITS', units), \
                patch.object(install, 'unit_info', return_value={}), patch.object(install, 'ctl') as ctl:
            install.uninstall(types.SimpleNamespace(purge=False))
        self.assertEqual(business.read_bytes(), b'preserve me')
        self.assertTrue(state.exists())
        self.assertFalse((units / install.SERVICE).exists())
        self.assertNotIn(install.PM_TIMER, [arg for call in ctl.call_args_list for arg in call.args])

    def test_purge_retries_partial_deletion_after_legacy_rollback_with_disabled_pm(self):
        legacy = {name: (MANAGEMENT / name).read_bytes() for name in OLDER_FILES}
        install.write_version({name: install.digest(data) for name, data in legacy.items()}, legacy)
        pm = self.root / 'pm'
        pm.mkdir(mode=0o700)
        install.write_json(pm / 'owner.json', 'previous pm owner')
        install.write_json(pm / 'state.json', {'installed': False, 'resources': []})
        absent_pm = {'nftables': [{'table': {'family': 'inet', 'name': 'unrelated'}}]}
        live_pm = {'nftables': [{'table': {'family': 'ip', 'name': 'node_manager_pm'}}]}
        nft = types.SimpleNamespace(returncode=0, stdout=json.dumps(live_pm))
        args = types.SimpleNamespace(purge=True)
        real_remove = install.remove

        def interrupt(path):
            real_remove(path)
            if path == self.code / 'agent.py':
                raise KeyboardInterrupt

        with patch.object(install, 'unit_info', return_value={}), \
                patch.object(install, 'ctl', return_value=types.SimpleNamespace(stdout='')), \
                patch.object(install.subprocess, 'run', return_value=nft) as command:
            with self.assertRaisesRegex(ValueError, '仍存在受管 PM 表'):
                install.uninstall(args)
            self.assertFalse(self.purge_journal.exists())
            self.assertTrue((self.code / 'agent.py').exists())
            nft.stdout = json.dumps(absent_pm)
            with patch.object(install, 'remove', side_effect=interrupt):
                with self.assertRaises(KeyboardInterrupt):
                    install.uninstall(args)
            self.assertFalse((self.code / 'agent.py').exists())
            self.assertTrue(self.purge_journal.is_file())
            # On retry the retained PM owner still has to be checked against nftables.
            nft.stdout = json.dumps(live_pm)
            with self.assertRaisesRegex(ValueError, '仍存在受管 PM 表'):
                install.uninstall(args)
            nft.stdout = json.dumps(absent_pm)
            install.uninstall(args)
        command.assert_called_with(('nft', '-j', '-n', 'list', 'tables'),
                                   capture_output=True, text=True, stdin=install.subprocess.DEVNULL,
                                   timeout=15, env={'PATH': '/usr/sbin:/usr/bin:/sbin:/bin', 'LC_ALL': 'C'})
        self.assertFalse(self.code.exists())
        self.assertFalse(self.root.exists())
        self.assertFalse(self.purge_journal.exists())

    def test_pm_transition_during_pause_restores_old_runtimes_without_switching(self):
        pm = self.root / 'pm'
        pm.mkdir(mode=0o700)
        pending = pm / 'nft-transition.json'

        def stop(_record):
            self.assertEqual(install.read_json(self.root / install.JOURNAL)['phase'], 'paused')
            install.write_json(pending, {'epoch': 'keep for module recovery'})

        with patch.object(install, 'check_agent_unit', return_value={'ActiveState': 'active'}), \
                patch.object(install, 'pm_info', return_value=({}, False)), \
                patch.object(install, 'check_pm_units'), \
                patch.object(install, 'maintenance', return_value={'owned': True}), \
                patch.object(install, 'stop_runtimes', side_effect=stop), \
                patch.object(install, 'start_runtimes') as start:
            with self.assertRaisesRegex(ValueError, '已恢复原版本'):
                install.change(self.binding, self.before_manifest, self.after_manifest, self.after)
        start.assert_called_once()
        self.assertTrue(pending.exists())
        self.assertFalse((self.root / install.JOURNAL).exists())
        install.check_current(self.before_manifest)

    def test_recover_discards_interrupted_unpublished_version_backup(self):
        write_file = install.write_file

        def interrupt(path, data, mode=0o600):
            write_file(path, data, mode)
            if path.parent.name == 'before':
                raise KeyboardInterrupt

        with patch.object(install, 'check_agent_unit', return_value={'ActiveState': 'active'}), \
                patch.object(install, 'pm_info', return_value=({}, False)), \
                patch.object(install, 'write_file', side_effect=interrupt), \
                patch.object(install, 'maintenance') as maintenance:
            with self.assertRaises(KeyboardInterrupt):
                install.change(self.binding, self.before_manifest, self.after_manifest, self.after)
        maintenance.assert_not_called()
        self.assertEqual(install.read_json(self.root / install.JOURNAL)['phase'], 'backing_up')
        install.component_action(types.SimpleNamespace(action='recover', offline=False))
        self.assertFalse((self.root / install.JOURNAL).exists())
        self.assertEqual(list((self.root / 'component-backups').iterdir()), [])
        install.check_current(self.before_manifest)


class HubListenTests(unittest.TestCase):
    def test_follows_the_port_the_service_was_installed_with(self):
        shown = 'ExecStart={ path=/opt/x/monitor-hub ; argv[]=/opt/x/monitor-hub --listen 127.0.0.1:39911 --db /d ; }\n'
        with patch.object(hub_lifecycle, 'ctl', return_value=shown):
            self.assertEqual(hub_lifecycle.service_listen('node-manager-hub.service'), '127.0.0.1:39911')

    def test_falls_back_to_the_default_port_before_installation(self):
        with patch.object(hub_lifecycle, 'ctl', return_value='ExecStart=\n'):
            self.assertEqual(hub_lifecycle.service_listen('node-manager-hub.service'), '127.0.0.1:9911')


class HubPurgeTests(unittest.TestCase):
    def hub(self, root):
        hub = object.__new__(hub_lifecycle.Hub)
        hub.database = root / 'data' / 'hub.db'
        hub.database.parent.mkdir()
        (hub.database.parent / 'themes').mkdir()
        (hub.database.parent / 'themes' / 'index.html').write_text('x')
        return hub

    def test_data_directory_and_themes_go_together(self):
        with tempfile.TemporaryDirectory() as directory:
            hub = self.hub(pathlib.Path(directory))
            hub.remove_data_directories()
            self.assertFalse(hub.database.parent.exists())
            hub.remove_data_directories()

    def test_real_directory_behind_a_systemd_symlink_also_goes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            hub = self.hub(root)
            real = hub.database.parent.rename(root / 'private')
            (root / 'data').symlink_to(real)
            hub.remove_data_directories()
            self.assertFalse(real.exists())
            self.assertFalse((root / 'data').is_symlink())

    def test_files_it_does_not_own_are_kept(self):
        with tempfile.TemporaryDirectory() as directory:
            hub = self.hub(pathlib.Path(directory))
            other = hub.database.parent / 'other.txt'
            other.write_text('mine')
            hub.remove_data_directories()
            self.assertEqual(other.read_text(), 'mine')
            self.assertFalse((hub.database.parent / 'themes').exists())


class HubLifecycleTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = pathlib.Path(temp.name).resolve()
        self.binary = self.root / 'monitor-hub'
        self.binary.write_bytes(b'backup binary')
        self.database = self.root / 'live.db'
        self.connection = sqlite3.connect(self.database)
        self.addCleanup(self.connection.close)
        self.connection.executescript("""
            PRAGMA journal_mode=WAL;
            CREATE TABLE setting (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO setting VALUES ('test', 'committed in WAL');
            CREATE TABLE managed_node (snapshot TEXT);
            INSERT INTO managed_node VALUES ('encrypted');
            CREATE TABLE managed_operation (status TEXT, expires INTEGER);
            CREATE TABLE managed_maintenance (expires INTEGER);
        """)
        self.key = self.database.with_name(self.database.name + '.management.key')

    def test_snapshot_keeps_committed_wal_and_rejects_key_tampering(self):
        hub_lifecycle.replace_file(self.key, data=b'k' * 32)
        backup = hub_lifecycle.snapshot(self.root, self.binary, self.database, 'node-manager-test.service')
        self.assertEqual(hub_lifecycle.verify_backup(backup)['unit'], 'node-manager-test.service')
        with contextlib.closing(hub_lifecycle.read_only(backup / 'hub.db')) as db:
            self.assertEqual(db.execute("SELECT value FROM setting WHERE key='test'").fetchone(), ('committed in WAL',))
        hub_lifecycle.replace_file(backup / 'hub.db.management.key', data=b'z' * 32)
        with self.assertRaisesRegex(ValueError, '校验失败'):
            hub_lifecycle.verify_backup(backup)

    def test_uninstall_states_what_happens_to_the_data(self):
        for argv in (['uninstall'], ['uninstall', '--purge', '--keep-data']):
            with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
                hub_lifecycle.main(argv)

    def test_full_removal_deletes_verified_backups_and_keeps_the_rest(self):
        hub_lifecycle.replace_file(self.key, data=b'k' * 32)
        workspace = self.root / 'backups'
        workspace.mkdir(mode=0o700)
        backups = [hub_lifecycle.snapshot(workspace, self.binary, self.database, 'node-manager-test.service')
                   for _ in range(2)]
        hub = object.__new__(hub_lifecycle.Hub)
        hub.workspace, hub.unit = workspace, 'node-manager-test.service'
        hub.remove_backups()
        self.assertFalse(workspace.exists())
        workspace.mkdir(mode=0o700)
        tampered = hub_lifecycle.snapshot(workspace, self.binary, self.database, 'node-manager-test.service')
        (tampered / 'hub.db').write_bytes(b'changed')
        other = workspace / 'notes.txt'
        other.write_text('mine')
        verified = hub_lifecycle.snapshot(workspace, self.binary, self.database, 'node-manager-test.service')
        foreign = hub_lifecycle.snapshot(workspace, self.binary, self.database, 'node-manager-other.service')
        with contextlib.redirect_stdout(io.StringIO()) as shown:
            hub.remove_backups()
        self.assertFalse(verified.exists())
        self.assertTrue(tampered.exists() and other.exists() and foreign.exists())
        self.assertIn('notes.txt', shown.getvalue())
        self.assertFalse(any(path.exists() for path in backups))

    def test_snapshot_rejects_missing_key_when_encrypted_data_exists(self):
        with self.assertRaisesRegex(ValueError, '配套密钥'):
            hub_lifecycle.snapshot(self.root, self.binary, self.database, 'node-manager-test.service')

    def test_idle_refuses_delivered_task_and_active_node_lease(self):
        with self.connection:
            self.connection.execute('INSERT INTO managed_operation VALUES (?,?)', ('delivered', 0))
        with self.assertRaisesRegex(ValueError, '未完成'):
            hub_lifecycle.ensure_idle(self.database)
        with self.connection:
            self.connection.execute('DELETE FROM managed_operation')
            self.connection.execute('INSERT INTO managed_maintenance VALUES (?)', (int(time.time()) + 60,))
        with self.assertRaisesRegex(ValueError, '正在维护'):
            hub_lifecycle.ensure_idle(self.database)

    def test_failed_upgrade_restores_binary_key_and_stopped_database(self):
        args = types.SimpleNamespace(unit='node-manager-test.service', binary=self.binary,
                                     db=self.database, health='http://127.0.0.1:9911/api/me', backups=self.root,
                                     action='upgrade', candidate=self.root / 'new-monitor-hub')
        with patch.object(hub_lifecycle, 'STATE', self.root):
            hub = hub_lifecycle.Hub(args)
        hub.state.mkdir()
        hub_lifecycle.replace_file(self.key, data=b'k' * 32)
        args.candidate.write_bytes(b'#!/bin/sh\nprintf "node-manager-hub-lifecycle-v1\\n"\n')
        args.sha256 = hub_lifecycle.sha256(args.candidate)
        # Simulate an inactive service while using the real files and SQLite backup path.
        self.connection.close()
        regular = hub_lifecycle.regular

        def binary_owner(path, private=False):
            meta = regular(path, private)
            return types.SimpleNamespace(st_uid=0, st_gid=meta.st_gid, st_mode=meta.st_mode) if path == self.binary else meta

        observations = []

        def start():
            with contextlib.closing(hub_lifecycle.read_only(self.database)) as db:
                value = db.execute('SELECT value FROM setting WHERE key=?', (hub_lifecycle.GATE,)).fetchone()
            observations.append((self.binary.read_bytes(), value, self.key.read_bytes()))
            if value:
                raise ValueError('candidate could not bind')

        with patch.object(hub_lifecycle, 'regular', side_effect=binary_owner), \
                patch.object(hub_lifecycle.os, 'fchown'), patch.object(hub, 'info', return_value={'ActiveState': 'active'}), \
                patch.object(hub, 'prepare_guard'), patch.object(hub, 'check_stopped'), \
                patch.object(hub, 'stop'), patch.object(hub, 'start', side_effect=start):
            with self.assertRaisesRegex(ValueError, '已恢复停服时的版本'):
                hub.transition(args)
        self.assertEqual(observations, [(args.candidate.read_bytes(), ('on',), b'k' * 32),
                                        (b'backup binary', None, b'k' * 32)])
        self.assertFalse(hub.journal.exists())

    def test_interrupted_rollback_recovery_preserves_new_live_writes(self):
        args = types.SimpleNamespace(unit='node-manager-test.service', binary=self.binary,
                                     db=self.database, health='http://127.0.0.1:9911/api/me', backups=self.root)
        with patch.object(hub_lifecycle, 'STATE', self.root):
            hub = hub_lifecycle.Hub(args)
        hub.state.mkdir()
        hub_lifecycle.replace_file(self.key, data=b'k' * 32)
        backup = hub_lifecycle.snapshot(self.root, self.binary, self.database, hub.unit)
        checksum = hub_lifecycle.sha256(self.binary)
        db_stat = self.database.stat()
        bin_stat = self.binary.stat()
        self.connection.close()
        record = {'identity': hub.identity(), 'phase': 'switching', 'backup': backup.name,
                  'before_binary': checksum, 'after_binary': checksum,
                  'db_owner': [db_stat.st_uid, db_stat.st_gid],
                  'binary_owner': [bin_stat.st_uid, bin_stat.st_gid], 'binary_mode': 0o755}
        hub_lifecycle.write_json(hub.journal, record)
        leftovers = [hub_lifecycle.switch_temporary(path) for path in (self.binary, self.database, self.key)]
        for path in leftovers:
            hub_lifecycle.replace_file(path, data=b'interrupted replacement')

        def start_then_interrupt():
            self.assertEqual(hub_lifecycle.load_json(hub.journal)['phase'], 'rolled_back')
            with contextlib.closing(sqlite3.connect(self.database)) as db, db:
                db.execute("INSERT INTO setting VALUES ('post-rollback', 'keep')")
            raise KeyboardInterrupt

        with patch.object(hub_lifecycle.os, 'fchown'), patch.object(hub, 'prepare_guard'), \
                patch.object(hub, 'stop'), patch.object(hub, 'start', side_effect=start_then_interrupt):
            with self.assertRaises(KeyboardInterrupt):
                hub.rollback(record)
        self.assertFalse(any(path.exists() for path in leftovers))

        with patch.object(hub, 'start') as start, patch.object(hub, 'install') as install:
            hub.recover(hub_lifecycle.load_json(hub.journal))
        start.assert_called_once()
        install.assert_not_called()
        self.assertFalse(hub.journal.exists())
        with contextlib.closing(hub_lifecycle.read_only(self.database)) as db:
            self.assertEqual(db.execute("SELECT value FROM setting WHERE key='post-rollback'").fetchone(), ('keep',))

    def test_pause_journal_precedes_stop_and_can_resume_old_service(self):
        args = types.SimpleNamespace(unit='node-manager-test.service', binary=self.binary,
                                     db=self.database, health='http://127.0.0.1:9911/api/me', backups=self.root)
        with patch.object(hub_lifecycle, 'STATE', self.root):
            hub = hub_lifecycle.Hub(args)
        hub.state.mkdir()
        with patch.object(hub, 'prepare_guard'), patch.object(hub, 'stop', side_effect=TimeoutError()):
            with self.assertRaises(TimeoutError):
                hub.pause(True)
        self.assertEqual(hub_lifecycle.load_json(hub.journal)['phase'], 'paused')
        with patch.object(hub, 'start') as start:
            hub.resume_paused(True)
        start.assert_called_once()
        self.assertFalse(hub.journal.exists())


if __name__ == '__main__':
    unittest.main()
