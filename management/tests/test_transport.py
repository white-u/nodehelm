import http.client
import http.server
import json
import pathlib
import re
import sys
import tempfile
import threading
import time
import unittest
from urllib.error import HTTPError, URLError
from unittest.mock import patch
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import agent
from agent import base_url, component_digest, execute_module_task, execute_task, exchange, files_digest, recover_idle, run_action
from controller import Controller
from modules import SPECS
from modules import request
from protocol import PROTOCOL, Rejected
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
        # The same fixture is asserted against validate_request in src/management/tests.rs.
        cases=json.loads((pathlib.Path(__file__).parent/'operations.json').read_text())
        for case in cases:
            raw={'id':'a'*32,'revision':'b'*64,'module':case['module'],'action':case['action'],'payload':case['payload']}
            with self.subTest(case=case):
                if case['valid']: request(raw)
                else:
                    with self.assertRaises(Rejected): request(raw)

class ComponentDigestTests(unittest.TestCase):
    def test_matches_the_hub_fixture(self):
        # The same fixture is asserted in src/management/lifecycle_tests.rs.
        with tempfile.TemporaryDirectory() as directory:
            root=pathlib.Path(directory)
            (root/'a.py').write_text('x');(root/'b.json').write_text('y')
            self.assertEqual(files_digest(root,{'b.json','a.py'}),'9729e99f400a67b08485e1f9a207d5df22e349dd671e85a8233b7ec9b1488101')
    def test_hub_embeds_exactly_the_installed_file_set(self):
        source=pathlib.Path(__file__).resolve().parents[2]/'src/management/enroll.rs'
        management=source.parents[2]/'management'
        shipped={path.name for path in management.iterdir() if path.suffix in ('.py','.json') and not path.name.startswith('.')}
        self.assertEqual(set(re.findall(r'include_str!\("\.\./\.\./management/([^"/]+)"\)',source.read_text())),shipped)
    def test_real_component_digest_covers_the_installed_manifest(self):
        component_digest.cache_clear();self.addCleanup(component_digest.cache_clear)
        with tempfile.TemporaryDirectory() as directory:
            with patch('agent.ROOT',pathlib.Path(directory)):
                self.assertEqual(component_digest(),'')
                component_digest.cache_clear()
                write(pathlib.Path(directory)/'component-manifest.json',{'agent.py':'a'*64,'protocol.py':'b'*64})
                self.assertRegex(component_digest(),r'^[a-f0-9]{64}$')

class Hub(http.server.BaseHTTPRequestHandler):
    """A loopback stand-in for the hub: answers from `replies` in order, counting connections."""
    protocol_version = 'HTTP/1.1'
    replies, requests, connections = [], [], 0

    def setup(self):
        super().setup(); Hub.connections += 1

    def do_POST(self):
        body = self.rfile.read(int(self.headers['Content-Length']))
        Hub.requests.append((self.path, dict(self.headers), body))
        status, headers, data, then_drop = Hub.replies.pop(0)
        self.send_response(status)
        for name, value in {**headers, 'Content-Length': str(len(data))}.items():
            self.send_header(name, value)
        self.end_headers(); self.wfile.write(data)
        # Closed without saying so, as an idle keep-alive connection is.
        self.close_connection = then_drop

    def log_message(self, *args): pass


class ExchangeTests(unittest.TestCase):
    binding = {'server': 'https://panel.example.com', 'credential': 'a' * 64}

    def setUp(self):
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Hub)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close); self.addCleanup(server.shutdown)
        Hub.replies, Hub.requests, Hub.connections = [], [], 0
        port = server.server_address[1]
        # Plain HTTP to the loopback server in place of TLS to the hub.
        local = lambda host, _port, timeout, context: http.client.HTTPConnection('127.0.0.1', port, timeout=timeout)
        patcher = patch('agent.http.client.HTTPSConnection', side_effect=local)
        patcher.start(); self.addCleanup(patcher.stop)
        agent.close_connection(); self.addCleanup(agent.close_connection)

    def reply(self, status=200, data=b'{"task":null}', headers=None, then_drop=False):
        Hub.replies.append((status, {'Content-Type': 'application/json', **(headers or {})}, data, then_drop))

    def test_identifies_the_client_and_reuses_one_connection(self):
        self.reply(); self.reply()
        for _ in range(2):
            self.assertEqual(exchange(self.binding, 'poll', {}), {'task': None})
        self.assertEqual(Hub.connections, 1)
        path, headers, body = Hub.requests[0]
        self.assertEqual(path, '/api/management-agent/v1/poll')
        self.assertEqual(headers['User-Agent'], 'NodeManager/1')
        self.assertEqual(headers['Authorization'], 'Bearer ' + 'a' * 64)
        self.assertEqual(headers['X-Node-Manager-Protocol'], str(PROTOCOL))
        self.assertEqual(json.loads(body), {})

    def test_a_connection_dropped_while_idle_is_reopened_once(self):
        self.reply(then_drop=True); self.reply()
        exchange(self.binding, 'poll', {})
        self.assertEqual(exchange(self.binding, 'poll', {}), {'task': None})
        self.assertEqual((len(Hub.requests), Hub.connections), (2, 2))

    def test_an_error_status_is_raised_with_its_headers_and_not_retried(self):
        self.reply(422, b'snapshot: missing field `revision`')
        with self.assertRaises(HTTPError) as caught:
            exchange(self.binding, 'poll', {'snapshot': {'core': {}}})
        with caught.exception as error:
            self.assertEqual(error.code, 422)
        self.reply(401, b'', {'X-Node-Manager-Binding': 'revoked'})
        with self.assertRaises(HTTPError) as caught:
            exchange(self.binding, 'maintenance', {})
        with caught.exception as error:
            self.assertEqual(error.headers.get('X-Node-Manager-Binding'), 'revoked')
        self.assertEqual(len(Hub.requests), 2)

    def test_a_redirect_or_an_oversized_reply_is_refused(self):
        self.reply(302, b'', {'Location': 'https://elsewhere.example.com/'})
        with self.assertRaisesRegex(Rejected, '跳转'):
            exchange(self.binding, 'poll', {})
        self.reply(data=b' ' * (256 * 1024 + 1))
        with self.assertRaisesRegex(Rejected, '过大'):
            exchange(self.binding, 'poll', {})

    def test_an_unreachable_hub_reads_as_a_connection_failure(self):
        with patch('agent.http.client.HTTPSConnection', side_effect=lambda *a, **k: http.client.HTTPConnection('127.0.0.1', 9, timeout=1)):
            agent.close_connection()
            with self.assertRaises(URLError):
                exchange(self.binding, 'poll', {})


class TransportTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.addCleanup(self.tmp.cleanup)
        self.d=Driver();self.c=Controller(self.tmp.name,self.d, SPECS['snell'])
        self.req={'id':'a'*32,'action':'install','revision':self.c.snapshot()['revision'],'payload':{}}
    def test_idle_recovery_clears_old_null_receipt_without_touching_poller(self):
        pending = self.c.root / 'pending-result.json'
        with patch('agent.ROOT', self.c.root), patch('agent.command') as command, patch('builtins.print'):
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
                patch('snell.controller', lambda root: Controller(root, self.d, SPECS['snell'])), \
                patch('sb.controller', lambda root: Controller(root / 'sb', Driver(), SPECS['sb'])), \
                patch('fw.controller', lambda root: Controller(root / 'fw', Driver(), SPECS['fw'])), \
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
        restarted=Controller(self.tmp.name,self.d, SPECS['snell'])
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
