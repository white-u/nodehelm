import contextlib
import io
import json
import pathlib
import sys
import tempfile
import unittest
from unittest.mock import MagicMock, patch
from urllib.error import HTTPError

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import agent
import install
import lifecycle
import menu
from controller import Controller
from modules import SPECS
from protocol import Rejected
from storage import read, write
from test_node_lifecycle import Driver

OLD = 'https://old.example.com'
NEW = 'https://new.example.com'


class Fixture(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.base = pathlib.Path(temp.name)
        self.root = self.base / 'state'
        self.root.mkdir(mode=0o700)
        self.calls = []
        self.controllers = {name: Controller(self.root if name == 'snell' else self.root / name,
                                            Driver(name, self.calls), SPECS[name]) for name in lifecycle.MODULES}
        self.binding = {'server': OLD, 'node': 7, 'credential': 'c' * 64}
        install.write_json(self.root / 'binding.json', self.binding)
        stack = contextlib.ExitStack()
        self.addCleanup(stack.close)
        for target, name, value in ((agent, 'ROOT', self.root), (install, 'ROOT', self.root),
                                    (install, 'LIFECYCLE_LOCK', self.base / 'lifecycle-run.lock'),
                                    (install, 'PURGE_JOURNAL', self.base / 'purge.json'),
                                    (lifecycle, 'RECOVERY', self.base / 'helper'),
                                    (agent, 'MONITOR_ENV', self.base / 'agent.env')):
            stack.enter_context(patch.object(target, name, value))
        self.stop = stack.enter_context(patch.object(agent, 'stop_owned_agent'))
        self.started = stack.enter_context(patch.object(install, 'start_confirmed'))
        stack.enter_context(patch('builtins.print'))

    def snapshot(self):
        return {'blocked': False}

    def rebind(self, server, stdin=''):
        with patch.object(agent.sys, 'stdin', io.StringIO(stdin)):
            agent.rebind(self.snapshot, server)


class RebindTests(Fixture):
    def test_proof_matches_the_hub_and_never_carries_the_credential(self):
        # The same vector is asserted in src/management/tests.rs.
        proof = json.dumps({'proof': 'eb298f2f63e81ebabb8c44b7578eb15d887078101cf615dac538a30b71fda61c'}).encode()
        with patch('secrets.token_hex', return_value='a' * 64), patch.object(install, 'request', return_value=proof) as request:
            self.assertTrue(agent.same_hub(self.binding, 'https://panel.example.com'))
            self.assertFalse(agent.same_hub(self.binding, 'https://relay.example.com'), 'a relay answers for another address')
        self.assertEqual(request.call_args.args[1:], ('/api/management-agent/v1/identify', {'node': 7, 'nonce': 'a' * 64}))
        self.assertNotIn('c' * 64, repr(request.call_args_list))

    def test_unbound_or_unknown_address_is_not_the_same_hub(self):
        refused = HTTPError(NEW, 401, 'revoked', {'X-Node-Manager-Binding': 'revoked'}, io.BytesIO())
        with patch.object(install, 'request', side_effect=refused):
            self.assertFalse(agent.same_hub(self.binding, NEW))
        with patch.object(install, 'request', return_value=b'{"proof":"00"}'):
            self.assertFalse(agent.same_hub(self.binding, NEW))

    def test_new_address_of_the_same_hub_keeps_identity_and_moves_the_monitor_agent(self):
        (self.base / 'agent.env').write_text(f'MONITOR_SERVER={OLD}/\nMONITOR_TOKEN=t\n')
        (self.base / 'agent.env').chmod(0o600)
        write(self.root / 'pending-result.json', {'id': 'kept'})
        with patch.object(agent, 'same_hub', return_value=True), patch.object(install, 'register') as register, \
                patch.object(agent, 'command') as command:
            self.rebind('https://NEW.example.com/')
        register.assert_not_called()
        self.assertEqual(install.read_json(self.root / 'binding.json'), {**self.binding, 'server': NEW})
        self.assertEqual((self.base / 'agent.env').read_text(), f'MONITOR_SERVER={NEW}\nMONITOR_TOKEN=t\n')
        command.assert_called_once_with('systemctl', 'try-restart', 'monitor-agent.service')
        self.assertTrue((self.root / 'pending-result.json').exists(), 'the same hub still waits for it')
        self.started.assert_called_once()

    def test_monitor_agent_of_another_hub_is_left_alone(self):
        (self.base / 'agent.env').write_text('MONITOR_SERVER=https://elsewhere.example.com\n')
        (self.base / 'agent.env').chmod(0o600)
        with patch.object(agent, 'same_hub', return_value=True), patch.object(agent, 'command') as command:
            self.rebind(NEW)
        self.assertEqual((self.base / 'agent.env').read_text(), 'MONITOR_SERVER=https://elsewhere.example.com\n')
        command.assert_not_called()

    def test_monitor_env_that_is_not_ours_to_rewrite_does_not_undo_the_move(self):
        (self.base / 'agent.env').write_text(f'MONITOR_SERVER={OLD}\n')
        (self.base / 'agent.env').chmod(0o644)
        with patch.object(agent, 'same_hub', return_value=True), patch.object(agent, 'command') as command:
            self.rebind(NEW)
        self.assertEqual(install.read_json(self.root / 'binding.json')['server'], NEW)
        self.started.assert_called_once()
        command.assert_not_called()

    def test_another_hub_needs_a_code_and_changes_nothing_without_one(self):
        before = (self.root / 'binding.json').read_bytes()
        with patch.object(agent, 'same_hub', return_value=False), patch.object(install, 'register') as register:
            with self.assertRaises(agent.NeedsCode):
                self.rebind(NEW)
            with self.assertRaisesRegex(Rejected, '接入码无效'):
                self.rebind(NEW, '{"enrollment":"short"}')
        register.assert_not_called()
        self.stop.assert_not_called()
        self.assertEqual((self.root / 'binding.json').read_bytes(), before)

    def test_code_is_spent_before_the_old_binding_goes(self):
        code = 'd' * 64
        new = {'server': NEW, 'node': 3, 'credential': 'e' * 64}
        write(self.root / 'pending-result.json', {'id': 'old hub'})
        def register(server, enrollment):
            self.assertEqual((server, enrollment), (NEW, code))
            self.assertEqual(install.read_json(self.root / 'binding.json'), self.binding)
            self.stop.assert_not_called()
            return new
        with patch.object(agent, 'same_hub', return_value=False), patch.object(install, 'register', side_effect=register):
            self.rebind(NEW, json.dumps({'enrollment': code}))
        self.assertEqual(install.read_json(self.root / 'binding.json'), new)
        self.assertFalse((self.root / 'pending-result.json').exists())
        self.started.assert_called_once()

    def test_refused_code_keeps_the_old_binding(self):
        before = (self.root / 'binding.json').read_bytes()
        with patch.object(agent, 'same_hub', return_value=False), \
                patch.object(install, 'register', side_effect=ValueError('后台拒绝登记')):
            with self.assertRaisesRegex(ValueError, '拒绝登记'):
                self.rebind(NEW, json.dumps({'enrollment': 'd' * 64}))
        self.stop.assert_not_called()
        self.assertEqual((self.root / 'binding.json').read_bytes(), before)

    def test_an_address_that_is_no_hub_leaves_nothing_pending(self):
        before = (self.root / 'binding.json').read_bytes()
        refused = HTTPError(NEW, 405, 'Method Not Allowed', {}, io.BytesIO())
        with patch.object(agent, 'same_hub', return_value=False), patch.object(install, 'request', side_effect=refused):
            with self.assertRaisesRegex(ValueError, 'HTTP 405'):
                self.rebind(NEW, json.dumps({'enrollment': 'd' * 64}))
        self.assertFalse((self.root / 'enrollment.json').exists())
        self.assertEqual((self.root / 'binding.json').read_bytes(), before)

    def test_unfinished_uninstall_blocks_both_ways(self):
        write(self.root / lifecycle.JOB, {'id': 'b' * 32, 'scope': 'all', 'attempt': 1, 'status': 'paused'})
        with patch.object(agent, 'same_hub', return_value=True):
            with self.assertRaisesRegex(Rejected, '卸载尚未完成'):
                self.rebind(NEW)
        with patch.object(agent, 'same_hub', return_value=False), patch.object(agent, 'revocation', return_value='后台仍接受原绑定'), \
                patch.object(install, 'register') as register:
            with self.assertRaisesRegex(Rejected, '卸载尚未完成'):
                self.rebind(NEW, json.dumps({'enrollment': 'd' * 64}))
        register.assert_not_called()
        self.assertEqual(install.read_json(self.root / 'binding.json'), self.binding)


class LocalUninstallTests(Fixture):
    def setUp(self):
        super().setUp()
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        self.stack.enter_context(patch.object(install, 'purge_plan', return_value={}))
        self.prepared = self.stack.enter_context(patch.object(lifecycle, 'prepare_worker'))

    def state(self, module, ports, installed=True):
        write(self.controllers[module].state, {'installed': installed, 'resources': [{'port': port, 'name': str(port)} for port in ports]})

    def uninstall(self, revocation=lambda: '后台仍接受原绑定'):
        lifecycle.local_uninstall(self.controllers, agent.execute_module_task, self.snapshot, self.binding, revocation, self.stop)

    def test_runs_the_panel_plan_and_hands_cleanup_to_a_local_executor(self):
        self.state('snell', [10086])
        self.state('pm', [10086])
        write(self.root / 'pending-result.json', {'id': 'owed to a hub'})
        self.uninstall()
        self.assertEqual(self.calls, [('pm', 'delete', 10086), ('snell', 'delete', 10086),
                                      ('snell', 'core_uninstall', None), ('pm', 'core_uninstall', None)])
        self.stop.assert_called_once()
        self.assertFalse((self.root / 'pending-result.json').exists())
        job = read(self.root / lifecycle.JOB)
        self.assertEqual((job['status'], job['local']), ('cleaning', True))
        self.assertTrue(self.prepared.call_args.args[4], 'the executor must not wait on a hub')
        engine = lifecycle.Lifecycle(self.controllers, agent.execute_module_task, self.snapshot, self.binding)
        self.assertIsNone(engine.receipt(), 'a local uninstall is never reported to a hub')

    def test_pause_names_the_cause_and_a_rerun_continues(self):
        self.state('snell', [10086])
        self.controllers['snell'].driver.unknown = True
        with self.assertRaisesRegex(ValueError, '重新运行 nodehelm uninstall'):
            self.uninstall()
        self.assertEqual(self.calls, [])
        self.controllers['snell'].driver.unknown = False
        self.uninstall()
        self.assertEqual(read(self.root / lifecycle.JOB)['status'], 'cleaning')
        self.assertIn(('snell', 'delete', 10086), self.calls)

    def test_panel_uninstall_is_finished_in_the_panel_unless_it_was_given_up(self):
        panel = {'id': 'b' * 32, 'scope': 'all', 'attempt': 1, 'status': 'paused', 'stage': 'instances',
                 'index': 0, 'steps': [], 'current': None}
        write(self.root / lifecycle.JOB, panel)
        with self.assertRaisesRegex(ValueError, '请在后台继续'):
            self.uninstall()
        self.stop.assert_not_called()
        self.uninstall(revocation=lambda: None)
        job = read(self.root / lifecycle.JOB)
        self.assertNotEqual(job['id'], panel['id'])
        self.assertTrue(job['local'])

    def test_panel_lifecycle_cannot_continue_a_local_one(self):
        self.state('snell', [10086])
        self.controllers['snell'].driver.unknown = True
        with self.assertRaises(ValueError):
            self.uninstall()
        local = read(self.root / lifecycle.JOB)
        engine = lifecycle.Lifecycle(self.controllers, agent.execute_module_task, self.snapshot, self.binding)
        with self.assertRaisesRegex(ValueError, '不能互相接续'):
            engine.advance({'id': local['id'], 'scope': 'all', 'attempt': 9})
        self.assertEqual(read(self.root / lifecycle.JOB)['attempt'], local['attempt'])
        with self.assertRaisesRegex(ValueError, '未完成的本机卸载'):
            engine.advance({'id': 'f' * 32, 'scope': 'all', 'attempt': 1})

    def test_local_executor_never_calls_a_hub(self):
        helper = self.base / 'helper'
        helper.mkdir(mode=0o700)
        state = {'id': 'b' * 32, 'attempt': 1, 'phase': 'paused', 'resume_phase': 'purging', 'local': True}
        with patch.object(install, 'request', side_effect=AssertionError('hub called')), \
                patch('urllib.request.build_opener', side_effect=AssertionError('hub called')):
            self.assertTrue(lifecycle.report_state(state, 'paused'))
            lifecycle.finish_request(state, 'finished')
        self.assertEqual((state['phase'], state['cleaning_attempt']), ('purging', 1))


class MenuTests(unittest.TestCase):
    def test_rebind_asks_for_a_code_only_when_the_address_is_another_hub(self):
        runs = []
        def run(command, input, **kwargs):
            runs.append((command[-1], input))
            return MagicMock(returncode=3 if len(runs) == 1 else 0, stdout='', stderr='需要接入码')
        with patch.object(menu.subprocess, 'run', side_effect=run), patch('builtins.input', side_effect=[NEW, 'd' * 64]):
            with self.assertRaises(SystemExit) as done:
                menu.rebind()
        self.assertEqual(done.exception.code, 0)
        self.assertEqual(runs, [(NEW, ''), (NEW, json.dumps({'enrollment': 'd' * 64}))])

    def test_uninstall_needs_an_explicit_yes(self):
        with patch('builtins.input', return_value='y'), patch.object(menu, 'tool') as tool:
            with self.assertRaises(SystemExit):
                menu.uninstall()
        tool.assert_not_called()
        with patch('builtins.input', return_value='yes'), patch.object(menu, 'tool') as tool:
            menu.uninstall()
        tool.assert_called_once_with('agent.py', 'uninstall')


if __name__ == '__main__':
    unittest.main()
