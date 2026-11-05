import base64
import hashlib
import json
import pathlib
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from model import Rejected, request
from controller import Controller
from services import Driver
from snell import Driver as SnellDriver, atomic
from port_monitor import snapshot
from test_controller import Driver as FakeDriver

class TemplateRefreshTests(unittest.TestCase):
    def driver(self, directory, rendered):
        driver = SnellDriver()
        driver.configs = directory / 'configs'
        driver.units = directory / 'units'
        driver.configs.mkdir();driver.units.mkdir()
        driver.config = lambda item: rendered[item['port']]
        driver.service = lambda port: f'unit {port}\n'
        for port in rendered:
            (driver.configs / f'{port}.conf').write_text(f'old {port}\n')
            (driver.units / driver.unit(port)).write_text(f'unit {port}\n')
        return driver

    def test_only_changed_instances_are_rewritten_and_only_running_ones_restart(self):
        with tempfile.TemporaryDirectory() as directory:
            driver = self.driver(pathlib.Path(directory), {1: 'new 1\n', 2: 'old 2\n', 3: 'new 3\n'})
            driver.running = lambda port: port == 1
            driver.verify = Mock()
            state = {'installed': True, 'resources': [{'port': 1}, {'port': 2}, {'port': 3}]}
            with patch('snell.command') as command:
                driver.refresh(state)
                self.assertEqual((driver.configs / '3.conf').read_text(), 'new 3\n')
                self.assertEqual([call.args for call in command.call_args_list],
                                 [('systemctl', 'daemon-reload'), ('systemctl', 'restart', driver.unit(1))])
                driver.verify.assert_called_once_with(state, {'port': 1})
                # Everything now matches, so an interrupted upgrade can safely repeat this.
                command.reset_mock();driver.verify.reset_mock()
                driver.refresh(state)
                command.assert_not_called()

    def test_a_config_the_core_refuses_changes_nothing(self):
        with tempfile.TemporaryDirectory() as directory:
            driver = self.driver(pathlib.Path(directory), {1: 'new 1\n'})
            driver.check_config = Mock(side_effect=Rejected('invalid'))
            with patch('snell.command') as command, self.assertRaises(Rejected):
                driver.refresh({'installed': True, 'resources': [{'port': 1}]})
            command.assert_not_called()
            self.assertEqual((driver.configs / '1.conf').read_text(), 'old 1\n')

    def test_state_missing_a_field_the_template_needs_reads_as_drift(self):
        with tempfile.TemporaryDirectory() as directory:
            driver = self.driver(pathlib.Path(directory), {1: 'old 1\n'})
            driver.config = lambda item: item['added_later']
            self.assertFalse(driver.consistent({'installed': True, 'resources': [{'port': 1}]}, core_valid=True))


class ModuleTests(unittest.TestCase):
    def test_check_is_read_only_and_deduplicated(self):
        from storage import write
        class Probe(FakeDriver):
            def check_target(self,item): self.calls += 1
        with tempfile.TemporaryDirectory() as root:
            d=Probe();c=Controller(root,d,'fw')
            write(c.state,{'installed':True,'resources':[{'name':'test','port':12345,'target_host':'127.0.0.1','target_port':12346}]})
            before=c.state.read_bytes();req={'module':'fw','id':'c'*32,'action':'check','revision':c.snapshot()['revision'],'payload':{'port':12345}}
            self.assertEqual(c.execute(req)['status'],'checked');self.assertEqual(c.execute(req)['status'],'checked')
            self.assertEqual(d.calls,1);self.assertEqual(c.state.read_bytes(),before)
    def test_other_modules_reserved_ports_cannot_be_reused(self):
        from agent import execute_module_task
        from storage import write
        import time
        with tempfile.TemporaryDirectory() as root:
            a=Controller(pathlib.Path(root)/'snell',FakeDriver());b=Controller(pathlib.Path(root)/'fw',FakeDriver(),'fw')
            write(a.state,{'installed':True,'resources':[{'name':'Snell','port':12345,'host':'example.com','public_port':12345,'psk':'x'*32,'mode':'default'}]})
            task={'expires':time.time()+60,'request':{'id':'d'*32,'module':'fw','action':'apply','revision':b.snapshot()['revision'],'payload':{'name':'FW','port':12345,'target_host':'192.0.2.1','target_port':23456}}}
            self.assertEqual(execute_module_task({'snell':a,'fw':b},task)['status'],'rejected')
            self.assertEqual(b.driver.calls,0)
    def test_fixed_inputs_and_defaults(self):
        base={'id':'a'*32,'revision':'b'*64,'action':'apply','module':'fw','payload':{'name':'转发','port':12345,'target_host':'192.0.2.1'}}
        self.assertEqual(request(base)['payload']['target_port'],12345)
        for change in ({'target_host':'x;id'},{'target_host':'169.254.169.254'},{'target_port':0},{'command':'id'}):
            with self.assertRaises(Rejected):request({**base,'payload':{**base['payload'],**change}})
        with self.assertRaises(Rejected):request({**base,'module':'pm'})
    def test_ss2022_generates_and_preserves_keys(self):
        d=Driver('sb'); item={'protocol':'ss2022','psk':'','port':12345}
        first=d.complete(dict(item),None)
        self.assertEqual(len(base64.b64decode(first['psk'])),16)
        self.assertEqual(d.complete(dict(item),first)['psk'],first['psk'])
        with self.assertRaises(Rejected):d.complete({'protocol':'reality'},first)
    def test_configs_do_not_enable_remote_management_or_change_system_routing(self):
        d=Driver('sb')
        text=d.config({'protocol':'ss2022','port':12345,'psk':base64.b64encode(b'a'*16).decode()})
        cfg=json.loads(text)
        self.assertEqual(cfg['inbounds'][0]['listen_port'],12345)
        self.assertEqual(cfg['route']['rules'],[{'ip_is_private':True,'action':'reject'}])
        self.assertNotIn('experimental',cfg)
        fw=Driver('fw').config({'port':12345,'target_host':'192.0.2.1','target_port':23456})
        self.assertIn('use_udp = true',fw)
        self.assertIn('remote = "192.0.2.1:23456"',fw)
    def test_module_identity_cannot_reuse_another_controllers_journal(self):
        with tempfile.TemporaryDirectory() as root:
            c=Controller(root,FakeDriver(),'fw')
            req={'id':'a'*32,'action':'install','revision':c.snapshot()['revision'],'payload':{}}
            with self.assertRaises(Rejected):c.execute(req)
            self.assertEqual(c.execute({**req,'module':'fw'})['status'],'succeeded')
    def test_fw_rejects_all_same_port_loopback_targets(self):
        d = Driver('fw')
        item = {'port': 12345, 'target_port': 12345, 'target_host': '127.0.0.2'}
        before, after = {'resources': []}, {'resources': [item]}
        with patch.object(SnellDriver, 'prepare', return_value={}), patch('services.command') as command:
            with self.assertRaises(Rejected) as failure:
                d.prepare('apply', before, after, item)
            self.assertEqual(failure.exception.code, 'target_loop')
            item['target_port'] = 12346
            self.assertEqual(d.prepare('apply', before, after, item), {})
            command.assert_not_called()

    def test_reality_dns_failure_is_actionable(self):
        import socket
        with patch('services.socket.getaddrinfo', side_effect=socket.gaierror('sensitive text')):
            with self.assertRaises(Rejected) as failure:
                Driver.check_reality('bad.example')
            self.assertEqual(failure.exception.code, 'target_dns')
            self.assertNotIn('sensitive text', str(failure.exception))

    def test_verify_scopes_health_but_checks_all_config_ownership(self):
        with tempfile.TemporaryDirectory() as root:
            d = SnellDriver()
            d.binary = pathlib.Path(root) / 'snell'
            d.binary.write_bytes(b'test core')
            d.expected_hash = lambda: hashlib.sha256(b'test core').hexdigest()
            d.configs = d.units = pathlib.Path(root)
            active, stopped = 12345, 12346
            state = {'installed': True, 'resources': [
                {'port': port, 'mode': 'default', 'psk': 'p' * 32} for port in (active, stopped)
            ]}
            def metadata(ports):
                return {port: {'LoadState': 'loaded', 'FragmentPath': str(d.files(port)[1]), 'DropInPaths': ''}
                        if d.files(port)[1].exists() else {'LoadState': 'not-found', 'FragmentPath': '', 'DropInPaths': ''}
                        for port in ports}
            for item in state['resources']:
                config, unit = d.files(item['port'])
                atomic(config, d.config(item).encode())
                atomic(unit, d.service(item['port']).encode())
            d.unit_metadata = Mock(side_effect=metadata)
            d.running = Mock(side_effect=lambda port: port == active)
            with patch('snell.socket.create_connection') as connect, patch('snell.time.sleep'):
                d.verify(state, {'port': active})
                d.running.assert_called_once_with(active)
                connect.assert_called_once_with(('127.0.0.1', active), timeout=2)
                with self.assertRaisesRegex(Rejected, '实例未正常运行'):
                    d.verify(state, {'port': stopped})
                config, unit = d.files(stopped)
                config.write_text('changed by administrator')
                with self.assertRaisesRegex(Rejected, '实际配置'):
                    d.verify(state, {'port': active})
                config.unlink()
                unit.unlink()
                state['resources'].pop()
                d.verify(state, {'port': stopped})
                fields = metadata([active])
                fields[active]['DropInPaths'] = '/etc/systemd/system/foreign.conf'
                d.unit_metadata.side_effect = None
                d.unit_metadata.return_value = fields
                with self.assertRaisesRegex(Rejected, '实际配置'):
                    d.verify(state, {'port': active})

    def test_reality_does_not_probe_private_targets(self):
        with patch('services.socket.getaddrinfo',return_value=[(2,1,6,'',('127.0.0.1',443))]):
            with self.assertRaises(Rejected):Driver.check_reality('target.example')

class PortMonitorTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.addCleanup(self.tmp.cleanup)
        self.root=pathlib.Path(self.tmp.name);(self.root/'state').mkdir()
        self.lock=self.root/'absent.lock'
        self.config={'config_version':6,'ports':{'12345':{'quota_gb':10,'quota_mode':'in_out','reset_day':0,'comment':'demo','dyn_limit':{'enable':False}}}}
        (self.root/'config.json').write_text(json.dumps(self.config))
    def test_missing_or_corrupt_state_is_unknown_not_zero(self):
        result=snapshot(self.root,self.lock)
        self.assertEqual(result['status'],'ready');self.assertEqual(result['rows'][0]['state'],'unknown')
        self.assertNotIn('rx',result['rows'][0])
        target=self.root/'state/12345.txt';target.write_text('s_state_version=2\ns_acc_in=$(touch /tmp/should-not-run)')
        before=target.read_bytes();self.assertEqual(snapshot(self.root,self.lock)['rows'][0]['state'],'unknown');self.assertEqual(target.read_bytes(),before)
    def test_counters_are_read_without_executing_or_rewriting_state(self):
        p=self.root/'state/12345.txt';p.write_text('s_state_version=2\ns_acc_in=123\ns_acc_out=456\ns_last_k_in=0\ns_last_k_out=0\ns_last_reset_ts=1\ns_last_sample_ts=2\n')
        before=p.stat().st_mtime_ns;result=snapshot(self.root,self.lock)
        self.assertEqual(result['rows'][0]['rx'],123);self.assertEqual(result['rows'][0]['tx'],456);self.assertEqual(p.stat().st_mtime_ns,before)
    def test_symlink_and_unknown_config_versions_are_not_adopted(self):
        self.config['config_version']=999;(self.root/'config.json').write_text(json.dumps(self.config))
        self.assertEqual(snapshot(self.root,self.lock)['status'],'unavailable')
        (self.root/'config.json').unlink();(self.root/'config.json').symlink_to(self.root/'anything')
        self.assertEqual(snapshot(self.root,self.lock)['status'],'unavailable')

if __name__=='__main__':unittest.main()
