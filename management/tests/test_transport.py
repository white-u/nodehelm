import io
import json
import pathlib
import re
import sys
import tempfile
import time
import unittest
from urllib.error import HTTPError
from unittest.mock import patch, MagicMock
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from agent import base_url, component_digest, execute_module_task, execute_task, exchange, files_digest, recover_idle, run_action
from controller import Controller
from model import PROTOCOL, Rejected, request
from storage import read, write

class Driver:
    def __init__(self): self.calls=0; self.crash=False
    def runtime(self, state): return {}
    def consistent(self, state): return True
    def prepare(self,*args): return {'state':'old'}
    def apply(self,*args):
        self.calls+=1
        if self.crash: raise SystemExit('simulated process death')
    def verify(self,*args): pass
    def restore(self,*args): pass

class SharedOperationTests(unittest.TestCase):
    def test_agent_agrees_with_the_hub_on_every_operation(self):
        # The same fixture is asserted against validate_request in src/management/mod.rs.
        cases=json.loads((pathlib.Path(__file__).parent/'operations.json').read_text())
        for case in cases:
            raw={'id':'a'*32,'revision':'b'*64,'module':case['module'],'action':case['action'],'payload':case['payload']}
            with self.subTest(case=case):
                if case['valid']: request(raw)
                else:
                    with self.assertRaises(Rejected): request(raw)

class ComponentDigestTests(unittest.TestCase):
    def test_matches_the_hub_fixture(self):
        # The same fixture is asserted in src/management/mod.rs.
        with tempfile.TemporaryDirectory() as directory:
            root=pathlib.Path(directory)
            (root/'a.py').write_text('x');(root/'b.json').write_text('y')
            self.assertEqual(files_digest(root,{'b.json','a.py'}),'9729e99f400a67b08485e1f9a207d5df22e349dd671e85a8233b7ec9b1488101')
    def test_hub_embeds_exactly_the_installed_file_set(self):
        source=pathlib.Path(__file__).resolve().parents[2]/'src/management/mod.rs'
        management=source.parents[2]/'management'
        shipped={path.name for path in management.iterdir() if path.suffix in ('.py','.json') and not path.name.startswith('.')}
        self.assertEqual(set(re.findall(r'include_str!\("\.\./\.\./management/([^"/]+)"\)',source.read_text())),shipped)
    def test_real_component_digest_covers_the_installed_manifest(self):
        component_digest.cache_clear();self.addCleanup(component_digest.cache_clear)
        with tempfile.TemporaryDirectory() as directory:
            with patch('agent.ROOT',pathlib.Path(directory)):
                self.assertEqual(component_digest(),'')
                component_digest.cache_clear()
                write(pathlib.Path(directory)/'component-manifest.json',{'agent.py':'a'*64,'model.py':'b'*64})
                self.assertRegex(component_digest(),r'^[a-f0-9]{64}$')

class TransportTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.addCleanup(self.tmp.cleanup)
        self.d=Driver();self.c=Controller(self.tmp.name,self.d)
        self.req={'id':'a'*32,'action':'install','revision':self.c.snapshot()['revision'],'payload':{}}
    def test_machine_requests_identify_client_and_keep_authorization(self):
        response = MagicMock()
        response.__enter__.return_value.read.return_value = b'{"task":null}'
        opener = MagicMock()
        opener.open.return_value = response
        with patch('agent.urllib.request.build_opener', return_value=opener):
            self.assertEqual(exchange({'server':'https://panel.example.com','credential':'a'*64}, 'poll', {}), {'task':None})
        request = opener.open.call_args.args[0]
        self.assertEqual(request.get_header('User-agent'), 'NodeManager/1')
        self.assertEqual(request.get_header('Authorization'), 'Bearer '+'a'*64)
        self.assertEqual(request.get_header('X-node-manager-protocol'), str(PROTOCOL))
        self.assertEqual(request.full_url, 'https://panel.example.com/api/management-agent/v1/poll')

    def test_unrelated_unprocessable_poll_is_not_retried(self):
        binding = {'server': 'https://panel.example.com', 'credential': 'a' * 64}
        opener = MagicMock()
        opener.open.side_effect = HTTPError(binding['server'], 422, 'invalid data', {},
                                            io.BytesIO(b'snapshot: missing field `revision`'))
        with patch('agent.urllib.request.build_opener', return_value=opener):
            with self.assertRaises(HTTPError):
                exchange(binding, 'poll', {'snapshot': {'core': {}}})
        opener.open.assert_called_once()

    def test_idle_recovery_clears_old_null_receipt_without_touching_poller(self):
        pending = self.c.root / 'pending-result.json'
        with patch('agent.ROOT', self.c.root), patch('snell.command') as command, patch('builtins.print'):
            run_action('recover')
            self.assertFalse(pending.exists())
            write(pending, None)
            run_action('recover')
            self.assertFalse(pending.exists())
            actual = {'id': 'd' * 32, 'status': 'succeeded'}
            write(pending, actual)
            run_action('recover')
            self.assertEqual(read(pending), actual)
        command.assert_not_called()

    def test_idle_recovery_resumes_bound_poller_after_receipt_was_committed(self):
        pending = self.c.root / 'pending-result.json'
        write(self.c.root / 'binding.json', {'server': 'https://panel.example.com'})
        unit = {'LoadState': 'loaded', 'ActiveState': 'inactive'}
        with patch('agent.ROOT', self.c.root), patch('install.check_agent_unit', return_value=unit), \
                patch('install.ctl') as ctl:
            self.assertEqual(recover_idle(pending)['status'], 'idle')
        ctl.assert_called_once_with('start', 'node-manager-agent.service')

    def test_idle_recovery_never_removes_a_receipt_written_before_agent_lock(self):
        from contextlib import contextmanager

        pending = self.c.root / 'pending-result.json'
        write(pending, None)
        actual = {'id': 'e' * 32, 'status': 'succeeded'}

        @contextmanager
        def writes_before_lock(_path):
            write(pending, actual)
            yield

        with patch('agent.ROOT', self.c.root), patch('install.lock', side_effect=writes_before_lock):
            self.assertEqual(recover_idle(pending)['status'], 'idle')
        self.assertEqual(read(pending), actual)

    def test_https_root_only(self):
        self.assertEqual(base_url('https://panel.example.com/'),'https://panel.example.com')
        for url in ('http://panel.example.com','https://user:pass@example.com','https://example.com/path','https://example.com/?token=x'):
            with self.assertRaises(Rejected):base_url(url)
    def test_rejection_preserves_safe_local_diagnostic(self):
        self.req['revision'] = 'b'*64
        result = execute_task(self.c, {'request':self.req,'expires':time.time()+60})
        self.assertEqual(result['status'], 'rejected')
        self.assertEqual(read(self.c.root/'last-error.json')['reason'], '配置已变化，请刷新后重新编辑')
        self.assertEqual(self.d.calls, 0)

    def test_invalid_hub_requests_receive_terminal_replies(self):
        base = {**self.req, 'action': 'apply', 'payload': {'name': 'demo', 'port': 12345, 'host': 'node..example.com'}}
        for req in (base, {**base, 'unknown': 'private'}, {**base, 'module': 'invalid'}):
            with self.subTest(req=req):
                result = execute_module_task({'snell': self.c}, {'request': req, 'expires': time.time()+60})
                self.assertEqual(result['id'], self.req['id'])
                self.assertEqual(result['status'], 'rejected')
                self.assertNotIn('private', str(result))
        self.assertEqual(self.d.calls, 0)

    def test_unlink_clears_enrollment_before_agent_unit_exists(self):
        path = self.c.root / 'enrollment.json'
        write(path, {'enrollment': 'expired'})
        unit = {'LoadState': 'not-found', 'FragmentPath': '', 'DropInPaths': ''}
        with patch('agent.ROOT', self.c.root), \
                patch('agent.Driver', return_value=self.d), \
                patch('agent.ServiceDriver', side_effect=lambda _module: Driver()), \
                patch('agent.port_monitor.snapshot', return_value={'status': 'not_installed', 'rows': []}), \
                patch('install.check_agent_unit', return_value=unit), patch('install.ctl') as ctl:
            unit['DropInPaths'] = '/etc/systemd/system/node-manager-agent.service.d/foreign.conf'
            with self.assertRaisesRegex(Rejected, '拒绝解绑'):
                run_action('unlink')
            self.assertTrue(path.exists())
            unit['DropInPaths'] = ''
            run_action('unlink')
        self.assertFalse(path.exists())
        ctl.assert_not_called()

    def test_expired_new_task_has_no_effect(self):
        self.assertEqual(execute_task(self.c,{'request':self.req,'expires':time.time()-1})['status'],'expired')
        self.assertEqual(self.d.calls,0)
    def test_expired_completed_task_returns_receipt(self):
        self.c.execute(self.req)
        result=execute_task(self.c,{'request':self.req,'expires':time.time()-1})
        self.assertEqual(result['status'],'succeeded');self.assertEqual(self.d.calls,1)
    def test_expired_uncertain_task_is_not_declared_unexecuted(self):
        self.d.crash=True
        with self.assertRaises(SystemExit):self.c.execute(self.req)
        restarted=Controller(self.tmp.name,self.d)
        self.assertTrue(restarted.snapshot()['blocked'])
        result=execute_task(restarted,{'request':self.req,'expires':time.time()-1})
        self.assertEqual(result['status'],'recovery_failed')
        self.assertEqual(self.d.calls,1)
        self.assertEqual(restarted.recover()['status'],'rolled_back')
        self.assertEqual(restarted.execute(self.req)['status'],'rolled_back')
    def test_corrupt_local_state_is_not_treated_as_empty(self):
        self.c.state.write_text('{bad-json')
        with self.assertRaises(ValueError):self.c.snapshot()

if __name__=='__main__':unittest.main()
