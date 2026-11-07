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
import pm
from pm import Driver, GIB, batch, block_script, peers, period_start, plan, reality_probes, watch
from storage import read, write
from test_controller import Driver as FakeDriver


# No test reads the host's nftables: those for the block table ask for the real one.
_blocked = Driver.blocked


def setUpModule():
    patcher = patch.object(Driver, 'blocked', return_value=None)
    patcher.start()
    unittest.addModuleCleanup(patcher.stop)


def policy(**change):
    return dict(port=23456, name='Example', quota_gb=1, quota_mode='in_out', reset_day=31,
                expiry_date='', egress_mbps=0) | change


SS = '''0 0 192.0.2.10:23456 203.0.113.1:50000
0 0 192.0.2.10:23456 203.0.113.1:50001
0 0 [::ffff:192.0.2.10]:23456 [::ffff:203.0.113.2]:50002
0 0 [2001:db8::10]:23456 [2001:db8::2]:50003
0 0 [fe80::10%eth0]:23456 [fe80::3%eth0]:50004
0 0 192.0.2.10:22 198.51.100.9:50005
0 0 192.0.2.10:123456 198.51.100.8:50006
garbage
'''


def limited(max_ips=1, cooldown_min=30, whitelist=()):
    return policy(ip_limit={'max_ips': max_ips, 'cooldown_min': cooldown_min, 'whitelist': list(whitelist)})


class PmIpTests(unittest.TestCase):
    def test_peers_are_distinct_normalized_addresses_on_watched_ports(self):
        self.assertEqual(peers(SS, {'23456'}), {'23456': {'203.0.113.1', '203.0.113.2', '2001:db8::2', 'fe80::3'}})
        self.assertEqual(peers(SS, {'80'}), {})

    def test_addresses_count_from_their_second_minute_and_whitelist_is_skipped(self):
        rules = [limited(whitelist=['2001:db8::2'])]
        first = watch(rules, SS, {}, 1000)['23456']
        self.assertEqual((first['count'], first['level'], first['seq']), (0, 0, 0))
        second = watch(rules, SS, {'23456': first}, 1060)['23456']
        self.assertEqual(second['ips'], ['203.0.113.1', '203.0.113.2', 'fe80::3'])
        self.assertNotIn('2001:db8::2', second['seen'])
        # 3 over a limit of 1 is level 1; over limit + 2 would be level 2.
        self.assertEqual((second['count'], second['level'], second['seq'], second['last']), (3, 1, 1, 1060))
        # An address seen once, as by a scan, is not counted.
        scan = SS + '0 0 192.0.2.10:23456 198.51.100.77:40000\n'
        third = watch(rules, scan, {'23456': second}, 1120)['23456']
        self.assertEqual(third['count'], 3)

    def test_alerts_repeat_after_cooldown_or_on_a_rise_and_reset_silently(self):
        rules = [limited(max_ips=1, cooldown_min=2)]
        state = {'23456': {'seen': ['203.0.113.1', '203.0.113.2'], 'level': 1, 'seq': 4, 'last': 1000}}
        two = '0 0 192.0.2.10:23456 203.0.113.1:1\n0 0 192.0.2.10:23456 203.0.113.2:1\n'
        same = watch(rules, two, state, 1060)['23456']
        self.assertEqual((same['level'], same['seq'], same['last']), (1, 4, 1000))
        again = watch(rules, two, {'23456': same}, 1120)['23456']
        self.assertEqual((again['seq'], again['last']), (5, 1120))
        many = ''.join(f'0 0 192.0.2.10:23456 203.0.113.{n}:1\n' for n in range(1, 6))
        primed = {'23456': {**again, 'seen': [f'203.0.113.{n}' for n in range(1, 6)]}}
        rise = watch(rules, many, primed, 1130)['23456']
        self.assertEqual((rise['count'], rise['level'], rise['seq']), (5, 2, 6))
        calm = watch(rules, '0 0 192.0.2.10:23456 203.0.113.1:1\n', {'23456': rise}, 1190)['23456']
        self.assertEqual((calm['level'], calm['seq'], calm['last'], calm['ips']), (0, 6, 0, ['203.0.113.1']))

    def test_rules_without_a_limit_are_counted_but_never_alert(self):
        many = ''.join(f'0 0 192.0.2.10:23456 203.0.113.{n}:1\n' for n in range(1, 21))
        first = watch([policy()], many, {}, 1000)
        second = watch([policy()], many, first, 1060)['23456']
        self.assertEqual((second['count'], second['level'], second['seq']), (20, 0, 0))
        self.assertEqual(len(second['ips']), 15)

    def test_maintenance_reads_connections_only_while_rules_exist(self):
        with tempfile.TemporaryDirectory() as directory:
            driver = Driver(directory)
            with patch('pm.connections', return_value=SS) as connections:
                driver.watch_ips({'installed': True, 'resources': []})
                connections.assert_not_called()
                self.assertFalse(driver.ip_watch.exists())
                driver.watch_ips({'installed': True, 'resources': [policy()]})
                self.assertEqual(read(driver.ip_watch)['ports']['23456']['count'], 0)
                driver.ip_watch.write_text('{broken')
                driver.watch_ips({'installed': True, 'resources': [limited()]})
                driver.watch_ips({'installed': True, 'resources': []})
                self.assertFalse(driver.ip_watch.exists())
            with patch('pm.connections', return_value=None):
                driver.watch_ips({'installed': True, 'resources': [limited()]})
            self.assertFalse(driver.ip_watch.exists())

    def test_status_reports_a_recent_check_and_alert_state_for_limited_rules_only(self):
        with tempfile.TemporaryDirectory() as directory:
            driver = Driver(directory)
            now = int(time.time())
            rule = plan({'resources': [policy()]}, dt.date.today())['23456']
            data = {'plan': {'23456': rule}, 'values': {'23456': {'rx': 1, 'tx': 2}}, 'quotas': {}, 'sampled_at': now}
            write(driver.checkpoint, data)
            entry = {'seen': [], 'count': 4, 'level': 1, 'ips': ['203.0.113.1'], 'seq': 2, 'last': now}
            states = {'node-manager-pm.service': {'ActiveState': 'inactive'}}
            with patch.object(driver, 'units_match', return_value=True), \
                    patch.object(driver, 'timer_active', return_value=True), \
                    patch.object(driver, 'inspect'), patch.object(driver, 'sample', return_value=data), \
                    patch.object(driver, 'unit_states', return_value=states), \
                    patch.object(driver, 'expected_plan', return_value={'23456': rule}):
                for sampled, rules, counted, alerting in ((now, [limited()], True, True),
                                                          (now - 300, [limited()], False, False),
                                                          (now, [policy()], True, False)):
                    write(driver.ip_watch, {'sampled_at': sampled, 'ports': {'23456': entry}})
                    usage = driver.status({'installed': True, 'resources': rules})[1]['23456']
                    self.assertEqual(('ip_count' in usage, 'ip_alert' in usage, 'ip_level' in usage),
                                     (counted, alerting, alerting))
                write(driver.ip_watch, {'sampled_at': now, 'ports': {'23456': entry}})
                usage = driver.status({'installed': True, 'resources': [limited()]})[1]['23456']
                self.assertEqual({key: usage[key] for key in ('ip_count', 'ip_level', 'ip_alert', 'ip_list')},
                                 {'ip_count': 4, 'ip_level': 1, 'ip_alert': 2, 'ip_list': ['203.0.113.1']})

    def test_rule_format_without_ip_limit_is_unchanged(self):
        from model import resource
        self.assertNotIn('ip_limit', resource(policy(), 'pm'))
        rule = resource(limited(whitelist=['::ffff:203.0.113.7', '2001:DB8::1', '203.0.113.7']), 'pm')
        self.assertEqual(rule['ip_limit']['whitelist'], ['203.0.113.7', '2001:db8::1'])
        self.assertTrue(resource(limited() | {'ip_limit': limited()['ip_limit'] | {'block': True}}, 'pm')['ip_limit']['block'])
        self.assertNotIn('block', resource(limited(), 'pm')['ip_limit'], 'off is left out, keeping old revisions')
        for bad in ({'max_ips': 0}, {'action': 'block'}, {'block': False}, {'whitelist': ['fe80::1%eth0']}):
            with self.assertRaises(Rejected):
                resource(policy(ip_limit={'max_ips': 1, 'cooldown_min': 30, 'whitelist': []} | bad), 'pm')


def blocking(max_ips=2, whitelist=()):
    return policy(ip_limit={'max_ips': max_ips, 'cooldown_min': 30, 'whitelist': list(whitelist), 'block': True})


def online(*hosts):
    return ''.join(f'0 0 192.0.2.10:23456 203.0.113.{n}:1\n' for n in hosts)


class PmBlockTests(unittest.TestCase):
    def test_the_addresses_past_limit_plus_three_that_came_last_are_blocked(self):
        # Limit 2: 3 warns, 5 alarms, the 6th and later are blocked.
        state = watch([blocking()], online(1, 2, 3, 4, 5), {}, 1000)
        state = watch([blocking()], online(1, 2, 3, 4, 5), state, 1060)
        self.assertNotIn('block', state['23456'])
        self.assertEqual(state['23456']['level'], 2)
        state = watch([blocking()], online(1, 2, 3, 4, 5, 6, 7), state, 1120)
        self.assertNotIn('block', state['23456'], 'new addresses wait for their second minute')
        state = watch([blocking()], online(1, 2, 3, 4, 5, 6, 7), state, 1180)
        self.assertEqual(state['23456']['block'], ['203.0.113.6', '203.0.113.7'])
        self.assertEqual(state['23456']['since']['203.0.113.1'], 1000)
        # Blocked addresses stay established for a while but are no longer counted.
        state.pop('block', None)
        after = watch([blocking()], online(1, 2, 3, 4, 5, 6, 7), state, 1240,
                      {'23456': {'203.0.113.6', '203.0.113.7'}})['23456']
        self.assertEqual((after['count'], 'block' in after), (5, False))

    def test_no_block_without_the_switch_or_for_whitelisted_addresses(self):
        hosts = online(*range(1, 9))
        state = watch([limited(max_ips=2)], hosts, {}, 1000)
        self.assertNotIn('block', watch([limited(max_ips=2)], hosts, state, 1060)['23456'])
        # Whitelisted addresses are not counted: 5 of 8 is an alarm, not yet a block.
        rule = blocking(whitelist=[f'203.0.113.{n}' for n in range(1, 4)])
        state = watch([rule], hosts, {}, 1000)
        self.assertNotIn('block', watch([rule], hosts, state, 1060)['23456'])

    def test_block_script_creates_once_and_changes_elements(self):
        owner = 'node-manager/pm/' + '0' * 32
        created = block_script(owner, False, {('203.0.113.6', '23456'), ('2001:db8::6', '23456')}, set(), True)
        self.assertIn(f'add table {pm.BLOCK_TABLE} {{ comment "{owner}"; }}', created)
        self.assertEqual(created.count('add rule'), 2)
        self.assertIn('add element inet node_manager_pm_block v4 { 203.0.113.6 . 23456 timeout 30m }', created)
        self.assertIn('add element inet node_manager_pm_block v6 { 2001:db8::6 . 23456 timeout 30m }', created)
        changed = block_script(owner, True, set(), {('203.0.113.6', '23456')}, True)
        self.assertEqual(changed, 'delete element inet node_manager_pm_block v4 { 203.0.113.6 . 23456 }\n')
        self.assertEqual(block_script(owner, True, set(), set(), True), '')
        self.assertEqual(block_script(owner, False, set(), set(), True), '', 'no table before the first block')
        self.assertEqual(block_script(owner, True, set(), set(), False), 'delete table inet node_manager_pm_block\n')
        self.assertEqual(block_script(owner, False, set(), set(), False), '')

    def test_only_our_table_is_read(self):
        driver = Driver.__new__(Driver)
        driver.owner = 'node-manager/pm/' + '0' * 32
        tables = [{'table': {'family': 'inet', 'name': 'node_manager_pm_block'}}]
        table = [{'table': {'family': 'inet', 'name': 'node_manager_pm_block', 'comment': driver.owner}},
                 {'set': {'name': 'v4', 'elem': [{'elem': {'val': {'concat': ['203.0.113.6', 23456]}, 'timeout': 1800}}]}},
                 {'set': {'name': 'v6', 'elem': [{'concat': ['2001:db8::6', 23456]}]}}]
        with patch('pm.entries', side_effect=[tables, table]):
            self.assertEqual(_blocked(driver), {('203.0.113.6', '23456'), ('2001:db8::6', '23456')})
        with patch('pm.entries', return_value=[]):
            self.assertIsNone(_blocked(driver))
        foreign = [{'table': {'family': 'inet', 'name': 'node_manager_pm_block', 'comment': 'someone else'}}]
        with patch('pm.entries', side_effect=[tables, foreign]), self.assertRaises(Rejected):
            _blocked(driver)

    def test_failed_reality_handshakes_are_read_from_managed_sb_units_only(self):
        log = ('process connection from 203.0.113.9:40000: TLS handshake: REALITY: processed invalid connection\n'
               'process connection from [2001:db8::9]:40001: TLS handshake: REALITY: processed invalid connection\n'
               'process connection from 203.0.113.1:40002: mux connection closed: read frame header: EOF\n')
        with tempfile.TemporaryDirectory() as directory:
            units = pathlib.Path(directory)
            (units / 'node-manager-sb-23456.service').write_text('')
            done = type('Result', (), {'returncode': 0, 'stdout': log})()
            with patch('pm.subprocess.run', return_value=done) as run:
                self.assertEqual(reality_probes({'23456', '34567'}, units), {'23456': {'203.0.113.9', '2001:db8::9'}})
            self.assertEqual(run.call_count, 1, 'no SB unit on 34567, no journal read')

    def test_maintenance_blocks_drops_and_never_touches_a_foreign_table(self):
        hosts = online(*range(1, 8))
        with tempfile.TemporaryDirectory() as directory:
            driver = Driver(directory)
            driver.owner = 'node-manager/pm/' + '0' * 32
            rule = blocking(whitelist=['203.0.113.9'])
            with patch('pm.connections', return_value=hosts), patch('pm.reality_probes', return_value={}), \
                    patch('pm.nft') as nft:
                with patch.object(Driver, 'blocked', return_value=None):
                    driver.watch_ips({'installed': True, 'resources': [rule]})
                    nft.assert_not_called()
                    driver.watch_ips({'installed': True, 'resources': [rule]})
                script = nft.call_args.kwargs['script']
                self.assertIn('add table', script)
                self.assertIn('{ 203.0.113.6 . 23456 timeout 30m }', script)
                self.assertNotIn('block', read(driver.ip_watch)['ports']['23456'])
                nft.reset_mock()
                # A whitelisted address is let go, and so is every address once blocking is off.
                with patch.object(Driver, 'blocked', return_value={('203.0.113.9', '23456'), ('203.0.113.6', '23456')}):
                    driver.watch_ips({'installed': True, 'resources': [rule]})
                    self.assertIn('delete element inet node_manager_pm_block v4 { 203.0.113.9 . 23456 }', nft.call_args.kwargs['script'])
                    driver.watch_ips({'installed': True, 'resources': [limited()]})
                    self.assertEqual(nft.call_args.kwargs['script'], 'delete table inet node_manager_pm_block\n')
                nft.reset_mock()
                with patch.object(Driver, 'blocked', side_effect=Rejected('foreign', 'pm_conflict')):
                    driver.watch_ips({'installed': True, 'resources': [rule]})
                nft.assert_not_called()
                self.assertIn('23456', read(driver.ip_watch)['ports'], 'counting goes on')


class PmUnitStateTests(unittest.TestCase):
    SHOW = (b'Id=node-manager-pm.service\nLoadState=loaded\nActiveState=inactive\nFragmentPath=/etc/systemd/system/node-manager-pm.service\n'
            b'UnitFileState=static\nDropInPaths=\n\nId=node-manager-pm.timer\nLoadState=loaded\nActiveState=active\n'
            b'FragmentPath=/etc/systemd/system/node-manager-pm.timer\nUnitFileState=enabled\nDropInPaths=\n')

    def test_status_reads_both_units_in_one_call(self):
        with patch('pm.command') as command:
            command.return_value.stdout = self.SHOW
            states = Driver.unit_states()
        command.assert_called_once()
        self.assertEqual(states['node-manager-pm.timer']['UnitFileState'], 'enabled')
        self.assertEqual(states['node-manager-pm.service']['FragmentPath'], '/etc/systemd/system/node-manager-pm.service')
        self.assertTrue(Driver.timer_active(states))
        for state, enabled in (('disabled', False), ('masked', False), ('linked', False), ('enabled-runtime', True)):
            changed = {**states, 'node-manager-pm.timer': {**states['node-manager-pm.timer'], 'UnitFileState': state}}
            self.assertEqual(Driver.timer_active(changed), enabled, state)
        stopped = {**states, 'node-manager-pm.timer': {**states['node-manager-pm.timer'], 'ActiveState': 'inactive'}}
        self.assertFalse(Driver.timer_active(stopped))

    def test_units_match_uses_the_given_states(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            driver = Driver(root, root)
            for path, content in driver.unit_files():
                path.write_text(content)
            states = {path.name: {'LoadState': 'loaded', 'FragmentPath': str(path), 'DropInPaths': ''} for path, _ in driver.unit_files()}
            with patch.object(driver, 'unit_info') as unit_info:
                self.assertTrue(driver.units_match(states))
                states['node-manager-pm.timer']['DropInPaths'] = '/etc/systemd/system/node-manager-pm.timer.d/x.conf'
                self.assertFalse(driver.units_match(states))
            unit_info.assert_not_called()

    def test_a_missing_or_renamed_unit_is_refused(self):
        with patch('pm.command') as command:
            command.return_value.stdout = self.SHOW.split(b'\n\n')[0]
            with self.assertRaises(Rejected):
                Driver.unit_states()


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
