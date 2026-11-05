from contextlib import nullcontext
import datetime as dt
import pathlib
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from agent import execute_module_task
from controller import Controller
from model import Rejected, revision
from pm import Driver, GIB, batch, period_start, plan
from storage import read, write
from test_controller import Driver as FakeDriver


def policy(**change):
    return dict(port=23456, name='Example', quota_gb=1, quota_mode='in_out', reset_day=31,
                expiry_date='', egress_mbps=0) | change


class PmRefreshTests(unittest.TestCase):
    def test_changed_units_are_rewritten_and_the_table_rebuilt(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            driver = Driver(root, root)
            keep, stale = root / 'keep.service', root / 'stale.service'
            keep.write_text('keep\n');stale.write_text('old\n')
            driver.unit_files = lambda: [(keep, 'keep\n'), (stale, 'new\n')]
            state = {'installed': True, 'resources': []}
            with patch('pm.command') as command, patch.object(driver, 'reconcile') as reconcile:
                driver.refresh(state)
                self.assertEqual(stale.read_text(), 'new\n')
                command.assert_called_once_with('systemctl', 'daemon-reload')
                reconcile.assert_called_once_with(state)
                command.reset_mock();reconcile.reset_mock()
                driver.refresh({'installed': False, 'resources': []})
                command.assert_not_called();reconcile.assert_not_called()


class PmTests(unittest.TestCase):
    def test_month_end_and_expiry_use_utc_calendar_days(self):
        self.assertEqual(period_start(dt.date(2028, 2, 29), 31), '2028-02-29')
        self.assertEqual(period_start(dt.date(2026, 3, 30), 31), '2026-02-28')
        self.assertEqual(period_start(dt.date(2026, 3, 31), 31), '2026-03-31')
        self.assertEqual(period_start(dt.date(2026, 3, 31), 0), '')
        state = {'resources': [policy(expiry_date='2026-03-31')]}
        self.assertFalse(plan(state, dt.date(2026, 3, 31))['23456']['expired'])
        self.assertTrue(plan(state, dt.date(2026, 4, 1))['23456']['expired'])

    def test_existing_counters_preserved_and_renewal_reseeds_quota(self):
        today = dt.date(2026, 4, 1)
        active = plan({'resources': [policy()]}, today)
        expired = plan({'resources': [policy(expiry_date='2026-03-31')]}, today)
        values = {'23456': {'rx': GIB, 'tx': 120}}
        name, epoch = 'node-manager/pm/' + 'a' * 32, 'b' * 16
        unchanged = batch(name, epoch, active, active, values, (), 'c' * 16)
        self.assertNotIn('add counter', unchanged)
        self.assertNotIn('delete quota', unchanged)
        renewed = batch(name, epoch, expired, active, values, (), 'c' * 16)
        self.assertNotIn('add counter', renewed)
        self.assertIn(f'over {GIB} bytes used {GIB} bytes', renewed)

    def test_retry_does_not_add_already_restored_usage_again(self):
        with tempfile.TemporaryDirectory() as directory:
            driver = Driver(pathlib.Path(directory))
            before = {'epoch': 'b' * 16, 'plan': {}}
            restored = {'epoch': 'c' * 16, 'rollback_of': before['epoch']}
            backup = {'installed': True, 'state': {'installed': True, 'resources': []},
                      'checkpoint': before, 'reset_port': '23456'}
            with patch.object(driver, 'inspect', return_value={'epoch': restored['epoch']}), \
                    patch.object(driver, 'settle', return_value=restored), \
                    patch.object(driver, 'verify') as verify, patch.object(driver, 'replace') as replace:
                driver.restore(backup)
            verify.assert_called_once_with(backup['state'], {})
            replace.assert_not_called()

    def test_maintenance_does_not_create_empty_usage_when_checkpoint_is_missing(self):
        with tempfile.TemporaryDirectory() as directory:
            driver = Driver(directory)
            boot = pathlib.Path(directory) / 'boot'
            boot.write_text('test boot')
            driver.boot_id = boot
            with patch.object(driver, 'inspect', return_value=None), patch.object(driver, 'replace') as replace:
                with self.assertRaises(Rejected) as error:
                    driver.reconcile({'installed': True, 'resources': [policy()]})
            self.assertEqual(error.exception.code, 'pm_drift')
            replace.assert_not_called()

    def test_local_disable_empty_uses_recoverable_controller_journal(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            driver = Driver(root, root)
            state = {'installed': True, 'resources': []}
            write(root / 'state.json', state)
            prior = {'epoch': 'b' * 16, 'plan': {}}

            def interrupt(_before):
                raise KeyboardInterrupt

            with patch.object(driver, 'local_lock', return_value=nullcontext()) as lock, \
                    patch.object(driver, 'verify', return_value=prior), \
                    patch.object(driver, 'restore', side_effect=interrupt):
                with self.assertRaises(KeyboardInterrupt):
                    driver.disable_empty()
            lock.assert_called_once_with(lock_writer=False)
            controller = Controller(root, driver, 'pm')
            journal = read(controller.journal)
            self.assertTrue(journal['local'])
            self.assertEqual(journal['phase'], 'prepared')
            self.assertEqual(read(controller.state), state)
            with patch.object(driver, 'inspect', return_value=None), \
                    patch.object(driver, 'settle', return_value=prior), \
                    patch.object(driver, 'restore_empty_runtime') as restore, \
                    patch.object(driver, 'verify', return_value=prior):
                self.assertEqual(controller.recover()['status'], 'rolled_back')
            restore.assert_called_once_with(None, prior, prior)
            self.assertEqual(read(controller.state), state)
            self.assertFalse(controller.journal.exists())

    def test_install_refuses_a_unit_from_another_systemd_search_path(self):
        with tempfile.TemporaryDirectory() as directory:
            driver = Driver(directory, pathlib.Path(directory))
            info = {'LoadState': 'loaded', 'FragmentPath': '/run/systemd/system/node-manager-pm.service',
                    'DropInPaths': '', 'ActiveState': 'inactive'}
            with patch.object(driver, 'unit_info', return_value=info), patch.object(driver, 'inspect') as inspect:
                with self.assertRaises(Rejected) as error:
                    driver.prepare('install', {}, {}, {})
            self.assertEqual(error.exception.code, 'pm_conflict')
            inspect.assert_not_called()

    def test_agent_checks_listener_ownership_and_legacy_conflicts(self):
        class Probe(FakeDriver):
            def runtime(self, state):
                return {str(r['port']): 'active' for r in state['resources']}

        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            snell = Controller(root / 'snell', Probe())
            pm = Controller(root / 'pm', FakeDriver(), 'pm')
            snell_state = {'installed': True, 'resources': []}
            pm_state = {'installed': True, 'resources': []}
            write(snell.state, snell_state)
            write(pm.state, pm_state)
            controllers = {'snell': snell, 'pm': pm}
            apply = {'expires': time.time() + 60, 'request': {
                'id': 'a' * 32, 'module': 'pm', 'action': 'apply',
                'payload': policy(), 'revision': revision(pm_state),
            }}
            self.assertEqual(execute_module_task(controllers, apply)['code'], 'pm_port')
            snell_state['resources'] = [{'port': 23456}]
            write(snell.state, snell_state)
            legacy = {'status': 'ready', 'rows': [{'port': 23456}]}
            with patch('agent.port_monitor.snapshot', return_value=legacy):
                self.assertEqual(execute_module_task(controllers, apply)['code'], 'pm_legacy')
            pm_state['resources'] = [policy()]
            write(pm.state, pm_state)
            delete = {'expires': time.time() + 60, 'request': {
                'id': 'b' * 32, 'action': 'delete', 'payload': {'port': 23456}, 'revision': revision(snell_state),
            }}
            self.assertEqual(execute_module_task(controllers, delete)['code'], 'pm_dependency')
            self.assertEqual(read(snell.state), snell_state)
