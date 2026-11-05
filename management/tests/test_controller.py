import pathlib
import sys
import tempfile
import unittest
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from controller import Controller
from model import Rejected, resource
from storage import read, write

class Driver:
    def __init__(self):
        self.calls = 0
        self.fail = False
        self.restore_fail = False
    def runtime(self, state): return {}
    def consistent(self, state): return True
    def prepare(self, *args): return {'original': True}
    def apply(self, *args):
        self.calls += 1
        if self.fail: raise RuntimeError('secret-value')
    def verify(self, *args): pass
    def restore(self, *args):
        if self.restore_fail: raise RuntimeError('secret-value')

class Tests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.driver = Driver()
        self.c = Controller(self.tmp.name, self.driver)
        self.seq = 0
    def req(self, action='install', payload=None):
        self.seq += 1
        return dict(id=f'{self.seq:032x}', action=action, revision=self.c.snapshot()['revision'], payload=payload or {})
    def install(self): self.assertEqual(self.c.execute(self.req())['status'], 'succeeded')
    def item(self): return dict(name='test', host='node.example.com', port=12345)
    def test_interactive_port_range_and_privileged_capability(self):
        from snell import Driver as RealDriver
        self.assertEqual(resource({**self.item(), 'port':443})['port'], 443)
        for port in (0, 65536, True):
            with self.assertRaises(Rejected): resource({**self.item(), 'port':port})
        self.assertIn('AmbientCapabilities=CAP_NET_BIND_SERVICE', RealDriver().service(443))
        self.assertNotIn('AmbientCapabilities', RealDriver().service(10086))
    def test_replay_never_reexecutes(self):
        r = self.req()
        first = self.c.execute(r)
        restarted = Controller(self.tmp.name, self.driver)
        self.assertEqual(restarted.execute(r), first)
        self.assertEqual(self.driver.calls, 1)
        r['action'] = 'delete'; r['payload'] = {'port': 12345}
        with self.assertRaises(Rejected): restarted.execute(r)
    def test_edit_preserves_secret_delete_removes_resource(self):
        self.install()
        self.c.execute(self.req('apply', self.item()))
        psk = self.c.snapshot()['resources'][0]['psk']
        self.c.execute(self.req('apply', {**self.item(), 'name': 'edited'}))
        self.assertEqual(self.c.snapshot()['resources'][0]['psk'], psk)
        self.c.execute(self.req('delete', {'port': 12345}))
        self.assertEqual(self.c.snapshot()['resources'], [])
    def test_explicit_create_and_update_cannot_upsert(self):
        self.install()
        item = self.item()
        with self.assertRaisesRegex(Rejected, '实例不存在'):
            self.c.execute({**self.req('apply', item), 'intent': 'update'})
        created = {**self.req('apply', item), 'intent': 'create'}
        self.assertEqual(self.c.execute(created)['status'], 'succeeded')
        saved = self.c.snapshot()
        self.assertEqual(self.c.execute(created)['status'], 'succeeded')
        duplicate = self.req('apply', {**item, 'name': 'duplicate', 'psk': 'z' * 32})
        with self.assertRaisesRegex(Rejected, '该端口已有实例'):
            self.c.execute({**duplicate, 'intent': 'create'})
        self.assertEqual(self.c.snapshot(), saved)
        self.assertEqual(self.driver.calls, 2)
        self.assertEqual(self.c.execute({**self.req('apply', {**item, 'name': 'edited'}), 'intent': 'update'})['status'], 'succeeded')
        self.assertEqual(self.c.snapshot()['resources'][0]['name'], 'edited')
        self.assertEqual(self.c.snapshot()['resources'][0]['psk'], saved['resources'][0]['psk'])

    def test_conflict_does_not_touch_driver(self):
        old = self.req()
        self.install()
        with self.assertRaises(Rejected): self.c.execute(old)
        self.assertEqual(self.driver.calls, 1)
    def test_failure_recovers_without_leaking_output(self):
        self.driver.fail = True
        result = self.c.execute(self.req())
        self.assertEqual(result['status'], 'rolled_back')
        self.assertNotIn('secret-value', str(result))
        self.assertFalse(self.c.snapshot()['installed'])
    def test_recovery_failure_blocks_new_writes_and_can_resume(self):
        self.driver.fail = self.driver.restore_fail = True
        self.assertEqual(self.c.execute(self.req())['status'], 'recovery_failed')
        with self.assertRaises(Rejected): self.c.execute(self.req())
        self.driver.restore_fail = False
        restarted = Controller(self.tmp.name, self.driver)
        self.assertEqual(restarted.recover()['status'], 'rolled_back')
        self.assertFalse(restarted.snapshot()['blocked'])
    def test_crash_after_commit_does_not_restore_old_state(self):
        req = self.req()
        self.c.execute(req)
        after = read(self.c.state)
        write(self.c.journal, dict(id=req['id'], digest='digest', phase='committed', after=after))
        self.assertEqual(self.c.recover()['status'], 'succeeded')
        self.assertTrue(self.c.snapshot()['installed'])
    def test_committed_recovery_rechecks_only_journal_target(self):
        from unittest.mock import Mock
        self.install()
        self.driver.verify = Mock()
        req = self.req('delete', {'port': 12345})
        after = read(self.c.state)
        write(self.c.journal, dict(id=req['id'], digest='digest', phase='committed', after=after,
                                  backup={'kind': 'resource', 'port': 12345}))
        self.assertEqual(self.c.recover()['status'], 'succeeded')
        self.driver.verify.assert_called_once_with(after, {'port': 12345})

    def test_reject_unknown_fields_and_injection(self):
        for payload in ({**self.item(), 'command':'id'}, {**self.item(), 'host':'x\n[Proxy]'}, {**self.item(), 'name':'x, psk=y'}, {**self.item(), 'mode':'unsafe-raw'}):
            with self.assertRaises(Rejected): resource(payload)
    def test_missing_key_with_receipts_fails_closed(self):
        self.install()
        (self.c.root / 'dedup-key.json').unlink()
        with self.assertRaises(Rejected): Controller(self.tmp.name, self.driver)

if __name__ == '__main__': unittest.main()
