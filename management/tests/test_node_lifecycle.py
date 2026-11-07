import contextlib
import hashlib
import io
import json
import pathlib
import subprocess
import sys
import tempfile
import types
import unittest
from unittest.mock import patch, MagicMock
from urllib.error import URLError, HTTPError

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import agent
import install
import lifecycle
from controller import Controller
from model import Rejected
from storage import read, write


COMPONENT_FILES = frozenset(path.name for path in pathlib.Path(install.__file__).parent.iterdir() if path.suffix in ('.py', '.json') and not path.name.startswith('.'))


class Driver:
    def __init__(self, module, calls):
        self.module, self.calls = module, calls
        self.unknown = False
        self.crash = False
    def runtime(self, state):
        return {str(item['port']): 'active' for item in state['resources']}
    def consistent(self, _state):
        return not self.unknown
    def inspect_inventory(self, _state):
        if self.unknown:
            raise Rejected('unknown owned file')
    def prepare(self, action, before, after, payload):
        return {'kind': 'resource', 'before': before}
    def apply(self, action, before, after, payload):
        self.calls.append((self.module, action, payload.get('port')))
        if self.crash:
            raise KeyboardInterrupt
    def verify(self, *_args):
        pass
    def restore(self, _backup):
        pass
    def prepare_core(self, action, before, payload):
        return {**before, 'installed': False}, {'kind': 'core', 'before': before}
    @contextlib.contextmanager
    def stage_core(self, *_args):
        yield None
    def apply_core(self, before, after, backup, candidate, begin):
        begin()
        self.calls.append((self.module, 'core_uninstall', None))
    def verify_core(self, *_args):
        pass
    def clean_core(self, *_args):
        pass
    def restore_core(self, *_args):
        pass


class NodeLifecycleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = pathlib.Path(self.temp.name)
        self.root = self.base / 'state'
        self.root.mkdir(mode=0o700)
        self.calls = []
        self.controllers = {name: Controller(self.root if name == 'snell' else self.root / name,
                                            Driver(name, self.calls), name) for name in lifecycle.MODULES}
        self.binding = {'server': 'https://panel.example.com', 'node': 1, 'credential': 'a' * 64}
        self.job = {'id': 'b' * 32, 'scope': 'snell', 'attempt': 1}
        self.engine = lifecycle.Lifecycle(self.controllers, agent.execute_module_task, lambda: {}, self.binding, self.root)
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        self.stack.enter_context(patch.object(install, 'ROOT', self.root))
        self.stack.enter_context(patch.object(install, 'COMMAND', self.base / 'nodehelm'))
        self.stack.enter_context(patch.object(install, 'LIFECYCLE_LOCK', self.base / 'lifecycle-run.lock'))
        self.stack.enter_context(patch.object(install, 'PURGE_JOURNAL', self.base / 'purge.json'))
        self.stack.enter_context(patch.object(lifecycle, 'RECOVERY', self.base / 'helper'))
        self.stack.enter_context(patch.object(install, 'purge_plan', return_value={}))
    def state(self, module, ports, installed=True):
        write(self.controllers[module].state, {'installed': installed, 'resources': [{'port': port, 'name': str(port)} for port in ports]})
    def finish(self, job=None):
        for _ in range(30):
            result = self.engine.advance(job or self.job)
            if result['status'] != 'running':
                return result
        self.fail('lifecycle did not converge')
    def test_module_removes_only_dependent_pm_before_instances_and_core(self):
        self.state('snell', [10086])
        self.state('sb', [20086])
        self.state('pm', [10086, 20086])
        result = self.finish()
        self.assertEqual(result['status'], 'succeeded')
        self.assertEqual(self.calls, [('pm', 'delete', 10086), ('snell', 'delete', 10086), ('snell', 'core_uninstall', None)])
        self.assertEqual([item['port'] for item in self.controllers['pm'].snapshot()['resources']], [20086])
        self.assertTrue(self.controllers['sb'].snapshot()['installed'])
    def test_unknown_inventory_pauses_before_any_effect(self):
        self.state('snell', [10086])
        self.controllers['snell'].driver.unknown = True
        self.assertEqual(self.engine.advance({**self.job, 'attempt': 2})['status'], 'paused')
        self.assertEqual(self.calls, [])
    def test_committed_receipt_wins_after_progress_write_is_interrupted(self):
        self.state('snell', [10086])
        save = self.engine.save
        def interrupt(record):
            if record['index'] == 1:
                raise KeyboardInterrupt
            return save(record)
        with patch.object(self.engine, 'save', side_effect=interrupt):
            with self.assertRaises(KeyboardInterrupt):
                self.engine.advance(self.job)
        self.assertIsNotNone(read(self.engine.path)['current'])
        self.assertEqual(self.finish()['status'], 'succeeded')
        self.assertEqual(self.calls.count(('snell', 'delete', 10086)), 1)
    def test_interrupted_own_journal_recovers_then_pauses_until_new_attempt(self):
        self.state('snell', [10086])
        self.controllers['snell'].driver.crash = True
        with self.assertRaises(KeyboardInterrupt):
            self.engine.advance(self.job)
        self.controllers['snell'].driver.crash = False
        self.assertEqual(self.engine.advance(self.job)['status'], 'paused')
        self.assertFalse(self.controllers['snell'].journal.exists())
        count = len(self.calls)
        self.assertEqual(self.engine.advance(self.job)['status'], 'paused')
        self.assertEqual(len(self.calls), count)
        self.assertEqual(self.finish({**self.job, 'attempt': 2})['status'], 'succeeded')
        receipts = list(self.controllers['snell'].receipts.iterdir())
        self.assertEqual(len(receipts), 3)  # rolled-back delete, resumed delete, uninstall.
    def test_preflight_system_service_timeout_is_durable_pause(self):
        self.state('snell', [10086])
        with patch.object(self.controllers['snell'].driver, 'inspect_inventory', side_effect=subprocess.TimeoutExpired('systemctl', 45)):
            result = self.engine.advance(self.job)
        self.assertEqual(result['status'], 'paused')
        self.assertEqual(read(self.engine.path)['message'], 'lifecycle_preflight')
        self.assertEqual(self.calls, [])

    def test_foreign_journal_is_never_recovered_by_lifecycle(self):
        self.state('snell', [10086])
        write(self.controllers['snell'].journal, {'id': 'f' * 32})
        with patch.object(self.controllers['snell'], 'recover') as recover:
            self.assertEqual(self.engine.advance(self.job)['status'], 'paused')
        recover.assert_not_called()
        self.assertEqual(self.calls, [])
    def test_manual_recovery_keeps_internal_receipt_out_of_ordinary_result(self):
        self.state('snell', [10086])
        self.controllers['snell'].driver.crash = True
        with self.assertRaises(KeyboardInterrupt):
            self.engine.advance(self.job)
        self.controllers['snell'].driver.crash = False
        with patch.object(agent, 'ROOT', self.root), patch.object(agent, 'Controller', side_effect=lambda _root, _driver, module='snell': self.controllers[module]), patch.object(agent, 'PMController', return_value=self.controllers['pm']), patch.object(agent, 'ServiceDriver'), patch('snell.command'), patch('builtins.print'):
            agent.run_action('recover')
        self.assertFalse((self.root / 'pending-result.json').exists())
        self.assertEqual(read(self.engine.path)['status'], 'paused')
        self.assertFalse(self.controllers['snell'].journal.exists())
        self.assertEqual(self.finish({**self.job, 'attempt': 2})['status'], 'succeeded')

    @contextlib.contextmanager
    def agent_runtime(self):
        with patch.object(agent, 'ROOT', self.root), patch.object(agent, 'Controller', side_effect=lambda _root, _driver, module='snell': self.controllers[module]), patch.object(agent, 'PMController', return_value=self.controllers['pm']), patch.object(agent, 'ServiceDriver'), patch('builtins.print'):
            yield

    def test_unlink_completed_job_then_rebind_polls_without_old_receipt(self):
        for index, binding in enumerate((
                {**self.binding, 'node': 2, 'credential': 'c' * 64},
                {**self.binding, 'server': 'https://new-panel.example.com', 'credential': 'd' * 64})):
            with self.subTest(binding=binding):
                self.state('snell', [10086])
                self.state('sb', [20086])
                job = {**self.job, 'id': str(index + 1) * 32}
                self.assertEqual(self.finish(job)['status'], 'succeeded')
                self.assertEqual(self.engine.receipt()['id'], job['id'])
                self.engine.acknowledge({'id': job['id'], 'attempt': job['attempt']})
                restarted = lifecycle.Lifecycle(self.controllers, agent.execute_module_task, lambda: {}, self.binding, self.root)
                self.assertIs(restarted.saved()['confirmed'], True)
                write(self.root / 'binding.json', self.binding)
                write(self.root / 'enrollment.json', {'old': True})
                write(self.root / 'pending-result.json', {'id': 'e' * 32})
                history = {path: path.read_bytes() for controller in self.controllers.values()
                           for path in controller.receipts.iterdir()}
                business = self.controllers['sb'].snapshot()
                with self.agent_runtime(), patch.object(agent, 'stop_owned_agent') as stop:
                    agent.run_action('unlink')
                stop.assert_called_once_with()
                for name in ('binding.json', 'enrollment.json', 'pending-result.json', lifecycle.JOB):
                    self.assertFalse((self.root / name).exists())
                self.assertEqual({path: path.read_bytes() for path in history}, history)
                self.assertEqual(self.controllers['sb'].snapshot(), business)
                write(self.root / 'binding.json', binding)
                confirmation = {'server': binding['server'], 'node': binding['node'],
                                'credential_sha256': hashlib.sha256(binding['credential'].encode()).hexdigest(),
                                'nonce': 'f' * 32}
                write(self.root / 'connection-request.json', confirmation)
                def poll(current_binding, path, payload):
                    self.assertEqual(current_binding, binding)
                    self.assertEqual(path, 'poll')
                    self.assertIsNone(payload['result'])
                    self.assertNotIn('lifecycle_result', payload)
                    return {'task': None}
                with self.agent_runtime(), patch.object(agent, 'exchange', side_effect=poll) as exchange, patch.object(agent.time, 'sleep', side_effect=KeyboardInterrupt):
                    with self.assertRaises(KeyboardInterrupt):
                        agent.run_action('run')
                exchange.assert_called_once()
                self.assertEqual(read(self.root / 'connection-confirmed.json'), confirmation)

    def test_unlink_preserves_active_lifecycle_and_identity(self):
        for status in lifecycle.ACTIVE:
            with self.subTest(status=status):
                job = {**self.job, 'status': status}
                write(self.engine.path, job)
                write(self.root / 'binding.json', self.binding)
                # The Hub still accepts this binding, so the job can still finish.
                with self.agent_runtime(), patch.object(agent, 'exchange', return_value={}), patch.object(agent, 'stop_owned_agent') as stop:
                    with self.assertRaisesRegex(Rejected, '节点卸载尚未完成'):
                        agent.run_action('unlink')
                stop.assert_not_called()
                self.assertEqual(read(self.engine.path), job)
                self.assertEqual(read(self.root / 'binding.json'), self.binding)

    def test_unlink_active_job_needs_explicit_revocation(self):
        def revoked(binding, path, payload):
            self.assertEqual(path, 'maintenance')
            raise HTTPError(binding['server'], 401, 'revoked', {'X-Node-Manager-Binding': 'revoked'}, io.BytesIO())
        def unavailable(binding, path, payload):
            raise HTTPError(binding['server'], 503, 'down', {}, io.BytesIO())
        for status in lifecycle.ACTIVE:
            with self.subTest(status=status):
                write(self.engine.path, {**self.job, 'status': status})
                write(self.root / 'binding.json', self.binding)
                # An outage is not a decision to give up on this node.
                with self.agent_runtime(), patch.object(agent, 'exchange', side_effect=unavailable), patch.object(agent, 'stop_owned_agent') as stop:
                    with self.assertRaisesRegex(Rejected, '节点卸载尚未完成'):
                        agent.run_action('unlink')
                stop.assert_not_called()
                # Abandoned by the Hub: the node returns and can be enrolled again.
                with self.agent_runtime(), patch.object(agent, 'exchange', side_effect=revoked), patch.object(agent, 'stop_owned_agent'):
                    agent.run_action('unlink')
                self.assertFalse(self.engine.path.exists())
                self.assertFalse((self.root / 'binding.json').exists())

    def test_unlink_preserves_terminal_job_while_cleanup_helper_exists(self):
        self.assertEqual(self.finish()['status'], 'succeeded')
        write(self.root / 'binding.json', self.binding)
        lifecycle.RECOVERY.mkdir()
        before = read(self.engine.path)
        with self.agent_runtime(), patch.object(agent, 'stop_owned_agent') as stop:
            with self.assertRaisesRegex(Rejected, '节点卸载尚未完成'):
                agent.run_action('unlink')
        stop.assert_not_called()
        self.assertEqual(read(self.engine.path), before)
        self.assertEqual(read(self.root / 'binding.json'), self.binding)

    def test_unlink_rechecks_job_after_stopping_poller(self):
        self.assertEqual(self.finish()['status'], 'succeeded')
        self.engine.acknowledge({'id': self.job['id'], 'attempt': self.job['attempt']})
        write(self.root / 'binding.json', self.binding)
        active = {**read(self.engine.path), 'status': 'paused'}
        def stop():
            write(self.engine.path, active)
        with self.agent_runtime(), patch.object(agent, 'stop_owned_agent', side_effect=stop):
            with self.assertRaisesRegex(Rejected, '节点卸载尚未完成'):
                agent.run_action('unlink')
        self.assertEqual(read(self.engine.path), active)
        self.assertEqual(read(self.root / 'binding.json'), self.binding)

    def test_unlink_requires_matching_explicit_terminal_acknowledgement(self):
        self.assertEqual(self.finish()['status'], 'succeeded')
        write(self.root / 'binding.json', self.binding)
        before = read(self.engine.path)
        for ack in (None, {}, {'id': 'f' * 32, 'attempt': 1},
                    {'id': self.job['id'], 'attempt': 2}, {'id': self.job['id'], 'attempt': True}):
            with self.subTest(ack=ack):
                self.engine.acknowledge(ack)
                with self.agent_runtime(), patch.object(agent, 'exchange', return_value={'status': 'idle'}), patch.object(agent, 'stop_owned_agent') as stop:
                    with self.assertRaisesRegex(Rejected, '卸载回执尚未得到后台确认'):
                        agent.run_action('unlink')
                stop.assert_not_called()
                self.assertEqual(read(self.engine.path), before)
                self.assertEqual(read(self.root / 'binding.json'), self.binding)
        for status, scope in (('running', 'snell'), ('paused', 'snell'), ('cleaning', 'all'), ('succeeded', 'all')):
            with self.subTest(status=status, scope=scope):
                record = {**before, 'status': status, 'scope': scope}
                write(self.engine.path, record)
                self.engine.acknowledge({'id': self.job['id'], 'attempt': 1})
                self.assertEqual(read(self.engine.path), record)

    def test_unlink_lost_ack_after_explicit_revocation_uses_only_readonly_probe(self):
        self.assertEqual(self.finish()['status'], 'succeeded')
        write(self.root / 'binding.json', self.binding)
        before = read(self.engine.path)
        calls = []
        def revoked(binding, path, payload):
            self.assertEqual(binding, self.binding)
            self.assertEqual(path, 'maintenance')
            self.assertEqual(payload, {'action': 'status', 'token': '0' * 64})
            self.assertEqual(read(self.engine.path), before)
            calls.append(path)
            raise HTTPError(binding['server'], 401, 'revoked', {'X-Node-Manager-Binding': 'revoked'}, io.BytesIO())
        with self.agent_runtime(), patch.object(agent, 'exchange', side_effect=revoked), patch.object(agent, 'stop_owned_agent') as stop, patch.object(agent, 'write') as write_state:
            agent.run_action('unlink')
        self.assertEqual(calls, ['maintenance', 'maintenance'])
        stop.assert_called_once_with()
        write_state.assert_not_called()
        self.assertFalse((self.root / 'binding.json').exists())
        self.assertFalse(self.engine.path.exists())

    def test_unlink_unconfirmed_receipt_preserves_identity_unless_revocation_is_explicit(self):
        self.assertEqual(self.finish()['status'], 'succeeded')
        write(self.root / 'binding.json', self.binding)
        before = read(self.engine.path)
        responses = [{'status': 'idle'}, {'status': 'busy'}, URLError('TLS failure'), TimeoutError('timeout')]
        responses.extend(HTTPError(self.binding['server'], code, 'failed', headers, io.BytesIO())
                         for code, headers in ((401, {}), (401, {'X-Node-Manager-Binding': 'unknown'}),
                                               (403, {'X-Node-Manager-Binding': 'revoked'}),
                                               (409, {}), (500, {'X-Node-Manager-Binding': 'revoked'})))
        for response in responses:
            with self.subTest(response=response):
                def probe(binding, path, payload):
                    self.assertEqual(binding, self.binding)
                    self.assertEqual(path, 'maintenance')
                    self.assertEqual(payload, {'action': 'status', 'token': '0' * 64})
                    if isinstance(response, Exception):
                        raise response
                    return response
                with self.agent_runtime(), patch.object(agent, 'exchange', side_effect=probe), patch.object(agent, 'stop_owned_agent') as stop, patch.object(agent, 'remove') as remove_state, patch.object(agent, 'write') as write_state:
                    with self.assertRaisesRegex(Rejected, '卸载回执尚未得到后台确认'):
                        agent.run_action('unlink')
                stop.assert_not_called()
                remove_state.assert_not_called()
                write_state.assert_not_called()
                self.assertEqual(read(self.engine.path), before)
                self.assertEqual(read(self.root / 'binding.json'), self.binding)

    def test_unlink_revocation_probe_rechecks_under_agent_lock_and_preserves_busy_state(self):
        self.assertEqual(self.finish()['status'], 'succeeded')
        write(self.root / 'binding.json', self.binding)
        before = read(self.engine.path)
        def revoked():
            return HTTPError(self.binding['server'], 401, 'revoked', {'X-Node-Manager-Binding': 'revoked'}, io.BytesIO())
        with self.agent_runtime(), patch.object(agent, 'exchange', side_effect=[revoked(), {'status': 'idle'}]) as exchange, patch.object(agent, 'stop_owned_agent'):
            with self.assertRaisesRegex(Rejected, '卸载回执尚未得到后台确认'):
                agent.run_action('unlink')
        self.assertEqual(exchange.call_count, 2)
        self.assertEqual(read(self.engine.path), before)
        self.assertEqual(read(self.root / 'binding.json'), self.binding)
        write(self.controllers['snell'].journal, {'id': 'f' * 32})
        with self.agent_runtime(), patch.object(agent, 'exchange', side_effect=[revoked(), revoked()]), patch.object(agent, 'stop_owned_agent'):
            with self.assertRaisesRegex(Rejected, '仍有未恢复操作'):
                agent.run_action('unlink')
        self.assertEqual(read(self.engine.path), before)
        self.assertEqual(read(self.root / 'binding.json'), self.binding)
        self.assertTrue(self.controllers['snell'].journal.exists())

    def test_unlink_revocation_probe_requires_original_binding_and_module_scope(self):
        self.assertEqual(self.finish()['status'], 'succeeded')
        before = read(self.engine.path)
        for scope, binding in (('snell', None), ('all', self.binding)):
            with self.subTest(scope=scope):
                record = {**before, 'scope': scope}
                write(self.engine.path, record)
                if binding:
                    write(self.root / 'binding.json', binding)
                with self.agent_runtime(), patch.object(agent, 'exchange') as exchange, patch.object(agent, 'stop_owned_agent') as stop:
                    with self.assertRaisesRegex(Rejected, '卸载回执尚未得到后台确认'):
                        agent.run_action('unlink')
                exchange.assert_not_called()
                stop.assert_not_called()
                self.assertEqual(read(self.engine.path), record)
                self.assertEqual(read(self.root / 'binding.json'), binding)

    def test_poll_only_confirms_terminal_receipt_with_matching_explicit_ack(self):
        self.assertEqual(self.finish()['status'], 'succeeded')
        write(self.root / 'binding.json', self.binding)
        before = read(self.engine.path)
        for reply, confirmed in (({'task': None}, False),
                                 ({'lifecycle_ack': {'id': 'f' * 32, 'attempt': 1}}, False),
                                 ({'lifecycle_ack': {'id': self.job['id'], 'attempt': 1}}, True)):
            with self.subTest(reply=reply):
                write(self.engine.path, before)
                def poll(_binding, _path, payload):
                    self.assertEqual(payload['lifecycle_result']['status'], 'succeeded')
                    return reply
                with self.agent_runtime(), patch.object(agent, 'exchange', side_effect=poll), patch.object(agent.time, 'sleep', side_effect=KeyboardInterrupt):
                    with self.assertRaises(KeyboardInterrupt):
                        agent.run_action('run')
                self.assertEqual(read(self.engine.path).get('confirmed', False), confirmed)

    def test_a_finished_task_is_reported_without_waiting(self):
        write(self.root / 'binding.json', self.binding)
        replies = iter([{'task': {'request': {'id': 'a' * 32}}}, {'task': None}])
        sent = []
        def poll(_binding, _path, payload):
            sent.append(payload['result']); return next(replies)
        outcome = {'id': 'a' * 32, 'status': 'succeeded'}
        with self.agent_runtime(), patch.object(agent, 'exchange', side_effect=poll), \
                patch.object(agent, 'execute_module_task', return_value=outcome), \
                patch.object(agent.time, 'sleep', side_effect=KeyboardInterrupt) as sleep:
            with self.assertRaises(KeyboardInterrupt):
                agent.run_action('run')
        self.assertEqual(sent, [None, outcome], 'the result goes out on the very next poll')
        sleep.assert_called_once_with(5)

    def test_poll_acknowledges_old_receipt_before_advancing_new_job(self):
        self.assertEqual(self.finish()['status'], 'succeeded')
        write(self.root / 'binding.json', self.binding)
        new_job = {**self.job, 'id': 'e' * 32}
        old_ack = {'id': self.job['id'], 'attempt': self.job['attempt']}
        advance = lifecycle.Lifecycle.advance
        def advancing(engine, job):
            self.assertIs(read(self.engine.path)['confirmed'], True)
            return advance(engine, job)
        reply = {'lifecycle_ack': old_ack, 'lifecycle': new_job}
        with self.agent_runtime(), patch.object(agent, 'exchange', return_value=reply), patch.object(lifecycle.Lifecycle, 'advance', autospec=True, side_effect=advancing), patch.object(agent.time, 'sleep', side_effect=KeyboardInterrupt):
            with self.assertRaises(KeyboardInterrupt):
                agent.run_action('run')
        self.assertEqual(read(self.engine.path)['id'], new_job['id'])
        self.engine.acknowledge(old_ack)
        self.assertNotIn('confirmed', read(self.engine.path))
        self.assertEqual(self.finish(new_job)['status'], 'succeeded')
        self.engine.acknowledge({'id': new_job['id'], 'attempt': 1})
        self.assertIs(read(self.engine.path)['confirmed'], True)
        self.engine.advance({**new_job, 'attempt': 2})
        self.assertNotIn('confirmed', read(self.engine.path))

    def test_all_orders_every_pm_rule_before_business_and_pm_environment_last(self):
        for index, module in enumerate(('snell', 'sb', 'fw')):
            self.state(module, [10086 + index])
        self.state('pm', [10086, 10087, 10088])
        result = self.finish({**self.job, 'scope': 'all'})
        self.assertEqual(result['status'], 'cleaning')
        self.assertTrue(all(call[0] == 'pm' and call[1] == 'delete' for call in self.calls[:3]))
        self.assertEqual(self.calls[-1], ('pm', 'core_uninstall', None))
        self.assertFalse(any(controller.snapshot()['installed'] or controller.snapshot()['resources'] for controller in self.controllers.values()))


class CleanupWorkerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = pathlib.Path(self.temp.name)
        self.root, self.code, self.units = (self.base / name for name in ('state', 'code', 'units'))
        install.prepare_directories(self.root, self.code, self.base.stat().st_uid)
        self.units.mkdir()
        self.recovery = self.base / 'helper'
        self.purge = self.base / 'purge.json'
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        self.stack.enter_context(patch.multiple(install, ROOT=self.root, CODE=self.code, UNITS=self.units, PURGE_JOURNAL=self.purge,
                                                       COMMAND=self.base / 'nodehelm', LIFECYCLE_LOCK=self.base / 'lifecycle-run.lock'))
        self.stack.enter_context(patch.object(lifecycle, 'RECOVERY', self.recovery))
        self.stack.enter_context(patch.object(install, 'ctl', return_value=types.SimpleNamespace(stdout='', returncode=0)))
        self.stack.enter_context(patch.object(install, 'unit_info', return_value={'LoadState': 'loaded', 'ActiveState': 'inactive'}))
        self.stack.enter_context(patch.object(install, 'check_purge_runtimes'))
        self.contents = {name: ('# trusted ' + name).encode() for name in COMPONENT_FILES}
        for name, data in self.contents.items():
            install.write_file(self.code / name, data, 0o644)
        install.write_json(self.root / install.MANIFEST, {name: install.digest(data) for name, data in self.contents.items()})
        self.binding = {'server': 'https://panel.example.com', 'node': 1, 'credential': 'a' * 64}
        install.write_json(self.root / 'binding.json', self.binding)
        self.job = {'id': 'b' * 32, 'attempt': 1, 'scope': 'all', 'status': 'cleaning', 'stage': 'cleanup'}
        install.write_json(self.root / lifecycle.JOB, self.job)
        self.token = 'c' * 64
        self.snapshot = {'installed': False, 'resources': [], 'blocked': False, 'revision': 'd' * 64,
                         'modules': {name: {'installed': False, 'resources': [], 'blocked': False, 'revision': 'd' * 64} for name in ('sb', 'fw', 'pm')}}
    def prepare(self):
        lifecycle.prepare_worker(self.job, self.token, self.binding, self.snapshot)
        return install.read_json(self.recovery / 'state.json')
    def test_partial_unpublished_helper_can_resume_and_published_unit_can_be_repaired(self):
        write_file = install.write_file
        def interrupt(path, data, mode=0o600):
            write_file(path, data, mode)
            if path.name == 'install.py':
                raise KeyboardInterrupt
        with patch.object(install, 'write_file', side_effect=interrupt):
            with self.assertRaises(KeyboardInterrupt):
                self.prepare()
        self.assertFalse(self.recovery.exists())
        state = self.prepare()
        self.assertEqual(state['id'], self.job['id'])
        install.remove(self.units / lifecycle.UNIT)
        lifecycle.start_worker()
        self.assertEqual((self.units / lifecycle.UNIT).read_text(), lifecycle.unit_content())
    def test_unknown_source_or_helper_file_is_preserved_and_blocks_handoff(self):
        (self.code / 'lifecycle.py').write_bytes(b'changed')
        with self.assertRaises(ValueError):
            self.prepare()
        self.assertFalse(self.recovery.exists())
        install.write_file(self.code / 'lifecycle.py', self.contents['lifecycle.py'], 0o644)
        state = self.prepare()
        (self.recovery / 'unknown').write_bytes(b'keep')
        with self.assertRaises(ValueError):
            lifecycle.check_worker_files(state)
        self.assertEqual((self.recovery / 'unknown').read_bytes(), b'keep')
    def test_purge_interruption_after_code_deletion_resumes_from_copied_helper(self):
        state = self.prepare()
        remove = install.remove
        def interrupt(path):
            remove(path)
            if path == self.code / 'agent.py':
                raise KeyboardInterrupt
        with patch.object(install, 'remove', side_effect=interrupt), patch.object(lifecycle, 'finish_request') as finish:
            with self.assertRaises(KeyboardInterrupt):
                lifecycle.worker_step(state)
            finish.assert_not_called()
        self.assertFalse((self.code / 'agent.py').exists())
        durable = install.read_json(self.recovery / 'state.json')
        self.assertEqual(durable['phase'], 'purging')
        observations = []
        def acknowledge(_state, phase):
            self.assertFalse(self.root.exists())
            self.assertFalse(self.code.exists())
            observations.append(phase)
        with patch.object(lifecycle, 'finish_request', side_effect=acknowledge), patch.object(lifecycle, 'cleanup_helper'):
            lifecycle.worker_step(durable)
        self.assertEqual(observations, ['cleaned', 'finished'])
    def test_lost_finish_response_retries_same_phase_without_repurging(self):
        state = self.prepare()
        with patch.object(lifecycle, 'finish_request', side_effect=URLError('lost reply')):
            with self.assertRaises(URLError):
                lifecycle.worker_step(state)
        self.assertEqual(install.read_json(self.recovery / 'state.json')['phase'], 'cleaned')
        with patch.object(install, 'finish_purge') as purge, patch.object(lifecycle, 'finish_request') as finish, patch.object(lifecycle, 'cleanup_helper'):
            lifecycle.worker_step(install.read_json(self.recovery / 'state.json'))
        purge.assert_not_called()
        self.assertEqual([call.args[1] for call in finish.call_args_list], ['cleaned', 'finished'])
    def test_paused_worker_resumes_with_new_attempt_and_confirms_cleaning_before_finish(self):
        state = self.prepare()
        state.update(phase='paused', resume_phase='preparing')
        install.write_json(self.recovery / 'state.json', state)
        opener = MagicMock()
        observations = []
        def answer(request, **_kwargs):
            payload = json.loads(request.data)
            observations.append((payload['lifecycle_result']['status'], payload['lifecycle_result']['attempt']))
            response = MagicMock()
            response.__enter__.return_value.read.return_value = json.dumps({'lifecycle': {**self.job, 'attempt': 2, 'cleanup_token': self.token}}).encode()
            # Wire descriptors contain no local status/stage metadata.
            response.__enter__.return_value.read.return_value = json.dumps({'lifecycle': {'id': self.job['id'], 'scope': 'all', 'attempt': 2, 'cleanup_token': self.token}}).encode()
            return response
        opener.open.side_effect = answer
        def acknowledge(_state, phase):
            self.assertEqual(observations, [('paused', 1), ('cleaning', 2)])
            self.assertFalse(self.root.exists())
        with patch('urllib.request.build_opener', return_value=opener), patch.object(lifecycle, 'finish_request', side_effect=acknowledge), patch.object(lifecycle, 'cleanup_helper'):
            lifecycle.worker_step(state)
        self.assertEqual(state['attempt'], 2)
        self.assertEqual(state['phase'], 'finished')

    @contextlib.contextmanager
    def installation_attempt(self, enrollment):
        manifest = {name: install.digest(data) for name, data in self.contents.items()}
        def confirm(predicate, _message, timeout):
            self.assertEqual(timeout, 35)
            self.assertFalse(predicate())
            with patch.object(agent, 'ROOT', self.root):
                agent.confirm_connection(install.read_json(self.root / 'binding.json'))
            self.assertTrue(predicate())
        with patch.object(install, 'download', return_value=(manifest, self.contents)), patch.object(install, 'wait_for', side_effect=confirm), patch.object(install.sys, 'stdin', io.StringIO(json.dumps({'enrollment': enrollment}))), patch('builtins.print'):
            yield types.SimpleNamespace(server=self.binding['server'], node=self.binding['node'])

    def test_install_retries_accepted_enrollment_after_response_or_binding_write_interruption(self):
        install.remove(self.root / lifecycle.JOB)
        for boundary in ('response', 'binding'):
            with self.subTest(boundary=boundary):
                install.remove(self.root / 'binding.json')
                accepted = []
                def enroll(server, path, payload, credential):
                    self.assertEqual(server, self.binding['server'])
                    self.assertEqual(path, '/api/management-agent/v1/enroll')
                    self.assertEqual(payload['credential'], credential)
                    self.assertEqual(install.read_json(self.root / 'enrollment.json'), {'server': server, **payload})
                    accepted.append(payload.copy())
                    if boundary == 'response' and len(accepted) == 1:
                        raise URLError('accepted but response lost')
                write_json = install.write_json
                def persist(path, value):
                    if boundary == 'binding' and path == self.root / 'binding.json':
                        raise OSError('binding write interrupted')
                    write_json(path, value)
                with self.installation_attempt('original') as args, patch.object(install, 'request', side_effect=enroll), patch.object(install, 'write_json', side_effect=persist):
                    with self.assertRaises((URLError, OSError)):
                        install.install(args)
                pending = install.read_json(self.root / 'enrollment.json')
                self.assertFalse((self.root / 'binding.json').exists())
                self.assertEqual(pending['credential'], accepted[0]['credential'])
                with self.installation_attempt('original') as args, patch.object(install, 'request', side_effect=enroll), patch.object(install.secrets, 'token_hex', wraps=install.secrets.token_hex) as token:
                    install.install(args)
                self.assertEqual(accepted, [accepted[0], accepted[0]])
                token.assert_called_once_with(16)  # New readiness nonce, no new identity.
                self.assertEqual(install.read_json(self.root / 'binding.json'), {key: pending[key] for key in ('server', 'node', 'credential')})
                for name in ('enrollment.json', 'connection-request.json', 'connection-confirmed.json'):
                    self.assertFalse((self.root / name).exists())

    def test_install_replaces_unaccepted_expired_code_without_replacing_identity(self):
        install.remove(self.root / lifecycle.JOB)
        install.remove(self.root / 'binding.json')
        def expired(_server, _path, _payload, _credential):
            raise HTTPError(self.binding['server'], 409, 'expired code', {}, io.BytesIO())
        with self.installation_attempt('expired') as args, patch.object(install, 'request', side_effect=expired):
            with self.assertRaises(HTTPError) as failure:
                install.install(args)
            failure.exception.close()
        pending = install.read_json(self.root / 'enrollment.json')
        def enroll(server, path, payload, credential):
            self.assertEqual(path, '/api/management-agent/v1/enroll')
            self.assertEqual(payload, {'node': pending['node'], 'credential': pending['credential'], 'enrollment': 'replacement'})
            self.assertEqual(credential, pending['credential'])
            self.assertEqual(install.read_json(self.root / 'enrollment.json'), {**pending, 'enrollment': 'replacement'})
        with self.installation_attempt('replacement') as args, patch.object(install, 'request', side_effect=enroll) as request:
            install.install(args)
        request.assert_called_once()
        self.assertEqual(install.read_json(self.root / 'binding.json')['credential'], pending['credential'])
        self.assertFalse((self.root / 'enrollment.json').exists())

    def test_install_rejected_replacement_code_retains_pending_identity_and_can_be_corrected(self):
        install.remove(self.root / lifecycle.JOB)
        install.remove(self.root / 'binding.json')
        pending = {**self.binding, 'enrollment': 'expired'}
        install.write_json(self.root / 'enrollment.json', pending)
        def reject(server, path, payload, credential):
            self.assertEqual(install.read_json(self.root / 'enrollment.json'), {**pending, 'enrollment': 'incorrect'})
            self.assertEqual(credential, pending['credential'])
            raise HTTPError(server, 409, 'invalid code', {}, io.BytesIO())
        with self.installation_attempt('incorrect') as args, patch.object(install, 'request', side_effect=reject):
            with self.assertRaises(HTTPError) as failure:
                install.install(args)
            failure.exception.close()
        self.assertEqual(install.read_json(self.root / 'enrollment.json'), {**pending, 'enrollment': 'incorrect'})
        self.assertFalse((self.root / 'binding.json').exists())
        self.assertFalse((self.root / 'connection-confirmed.json').exists())
        with self.installation_attempt('correct') as args, patch.object(install, 'request') as request:
            install.install(args)
        self.assertEqual(request.call_args.args[2], {'node': pending['node'], 'credential': pending['credential'], 'enrollment': 'correct'})
        self.assertEqual(install.read_json(self.root / 'binding.json'), self.binding)

    def test_install_pending_enrollment_rejects_different_server_or_node(self):
        install.remove(self.root / lifecycle.JOB)
        install.remove(self.root / 'binding.json')
        pending = {**self.binding, 'enrollment': 'original'}
        install.write_json(self.root / 'enrollment.json', pending)
        before = (self.root / 'enrollment.json').read_bytes()
        for target in ({'server': 'https://other.example.com', 'node': self.binding['node']},
                       {'server': self.binding['server'], 'node': 2}):
            with self.subTest(target=target), self.installation_attempt('replacement'), patch.object(install, 'request') as request:
                with self.assertRaisesRegex(ValueError, '其他后台或节点'):
                    install.install(types.SimpleNamespace(**target))
                request.assert_not_called()
                self.assertEqual((self.root / 'enrollment.json').read_bytes(), before)
                self.assertFalse((self.root / 'binding.json').exists())

    def test_install_fresh_nonce_requires_current_confirmation_with_existing_identity(self):
        install.remove(self.root / lifecycle.JOB)
        install.write_json(self.root / 'connection-confirmed.json', {'server': self.binding['server'], 'node': 1, 'credential_sha256': install.digest(self.binding['credential'].encode()), 'nonce': '0' * 32})
        def wait(predicate, _message, timeout):
            self.assertEqual(timeout, 35)
            self.assertFalse(predicate(), 'historical confirmation accepted as current readiness')
            with patch.object(agent, 'ROOT', self.root):
                agent.confirm_connection(self.binding)
            self.assertTrue(predicate())
        args = types.SimpleNamespace(server=self.binding['server'], node=1)
        manifest = {name: install.digest(data) for name, data in self.contents.items()}
        with patch.object(install, 'download', return_value=(manifest, self.contents)), patch.object(install, 'wait_for', side_effect=wait), patch.object(install.sys, 'stdin', io.StringIO('{"enrollment":"original"}')), patch('builtins.print'):
            install.install(args)
        self.assertEqual(install.read_json(self.root / 'binding.json'), self.binding)
        self.assertFalse((self.root / 'connection-request.json').exists())
        self.assertFalse((self.root / 'connection-confirmed.json').exists())

    def test_install_confirmation_timeout_retains_same_identity_and_recovery_request(self):
        install.remove(self.root / lifecycle.JOB)
        args = types.SimpleNamespace(server=self.binding['server'], node=1)
        manifest = {name: install.digest(data) for name, data in self.contents.items()}
        with patch.object(install, 'download', return_value=(manifest, self.contents)), patch.object(install, 'wait_for', side_effect=ValueError('未确认')), patch.object(install.sys, 'stdin', io.StringIO('{"enrollment":"original"}')):
            with self.assertRaisesRegex(ValueError, '未确认'):
                install.install(args)
        self.assertEqual(install.read_json(self.root / 'binding.json'), self.binding)
        self.assertTrue((self.root / 'connection-request.json').exists())
        self.assertTrue((self.code / 'agent.py').exists())

    def test_finalizer_survives_missing_component_scripts_and_removes_state_last(self):
        state = self.prepare()
        state['phase'] = 'finished'
        install.write_json(self.recovery / 'state.json', state)
        # Simulate a kill after install.py deletion; persistent terminal unit is independent of it.
        install.write_file(self.units / lifecycle.UNIT, lifecycle.terminal_unit_content().encode(), 0o644)
        install.remove(self.recovery / 'install.py')
        install.write_file(install.LIFECYCLE_LOCK, b'', 0o600)
        with patch('subprocess.run', return_value=types.SimpleNamespace(returncode=0)):
            exec(compile(lifecycle.terminal_script(), '<test-finalizer>', 'exec'), {})
        self.assertFalse(self.recovery.exists())
        self.assertFalse((self.units / lifecycle.UNIT).exists())
        self.assertFalse(install.LIFECYCLE_LOCK.exists(), 'nothing of the component is left in /run')
    def test_unknown_root_file_blocks_preflight_before_any_purge(self):
        (self.root / 'foreign').write_text('keep')
        with self.assertRaisesRegex(ValueError, '未登记文件'):
            install.purge_plan(lifecycle=True, preflight=True)
        self.assertTrue((self.root / 'foreign').exists())
        self.assertTrue((self.code / 'agent.py').exists())
    def test_completed_module_job_allows_ordinary_purge_but_active_job_does_not(self):
        install.remove(self.root / 'binding.json')
        install.write_json(self.root / lifecycle.JOB, {**self.job, 'scope': 'snell', 'status': 'succeeded', 'stage': 'finished'})
        plan = install.purge_plan()
        self.assertIn(str(self.root / lifecycle.JOB), [entry[0] for entry in plan['files']])
        install.write_json(self.root / lifecycle.JOB, {**self.job, 'status': 'paused'})
        with self.assertRaises(ValueError):
            install.purge_plan()


class AbandonedJobTests(unittest.TestCase):
    def revoked(self):
        return HTTPError('https://panel.example.com', 401, 'revoked', {'X-Node-Manager-Binding': 'revoked'}, io.BytesIO())

    def state(self, **fields):
        return {'id': 'a' * 32, 'attempt': 2, 'snapshot': {}, 'protocol': 1, 'cleanup_token': 't' * 64,
                'binding': {'server': 'https://panel.example.com', 'credential': 'c' * 64}, **fields}

    def test_finish_ends_quietly_once_the_hub_dropped_the_job(self):
        with patch.object(install, 'request', side_effect=self.revoked()):
            lifecycle.finish_request(self.state(), 'cleaned')

    def test_finish_keeps_retrying_on_any_other_refusal(self):
        for code, headers in ((401, {}), (400, {}), (502, {'X-Node-Manager-Binding': 'revoked'})):
            error = HTTPError('https://panel.example.com', code, 'no', headers, io.BytesIO())
            with patch.object(install, 'request', side_effect=error), self.assertRaises(HTTPError):
                lifecycle.finish_request(self.state(), 'cleaned')

    def test_removed_node_lets_a_paused_helper_complete_the_uninstall(self):
        opener = MagicMock()
        opener.open.side_effect = self.revoked()
        with tempfile.TemporaryDirectory() as directory, patch.object(lifecycle, 'RECOVERY', pathlib.Path(directory)), \
                patch('urllib.request.build_opener', return_value=opener):
            state = self.state(phase='paused', resume_phase='purging', cleaning_attempt=1)
            self.assertTrue(lifecycle.report_state(state, 'paused'))
            self.assertEqual((state['phase'], state['cleaning_attempt']), ('purging', 2))
            self.assertEqual(install.read_json(pathlib.Path(directory) / 'state.json')['phase'], 'purging')



if __name__ == '__main__':
    unittest.main()
