import hashlib
import io
import os
from unittest.mock import MagicMock, patch
import importlib.util
import pathlib
import tempfile
import sys
import types
import unittest
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2] / 'scripts'))
spec=importlib.util.spec_from_file_location('hub_install',pathlib.Path(__file__).resolve().parents[2]/'scripts/hub-install.py')
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
import hub_health


def elf(machine):
    return b'\x7fELF\x02\x01' + bytes(12) + machine.to_bytes(2, 'little') + bytes(44)


class HubInstallTests(unittest.TestCase):
    def test_safe_independent_unit(self):
        unit=m.unit_text('https://panel.example.com/',9911)
        self.assertIn('--listen 127.0.0.1:9911',unit)
        self.assertIn('DynamicUser=yes',unit)
        self.assertNotIn('/opt/monitor/',unit)
        self.assertIn('--site https://panel.example.com',unit)
    def test_reject_service_injection_or_non_https(self):
        for site in ('http://panel.example.com','https://a/%n','https://a\nExecStart=/bin/sh','https://user:pass@a','https://a/?x=1','https://panel.example.com?','https://panel.example.com#','https://127.0.0.1','https://localhost','https://a --listen=0.0.0.0:9911','https://a\t--db=x'):
            with self.assertRaises(ValueError):m.unit_text(site,9911)
        with self.assertRaises(ValueError):m.unit_text('https://a',22)
    @patch.object(m.platform, 'machine', return_value='x86_64')
    def test_binary_hash_and_symlinks(self, _machine):
        with tempfile.TemporaryDirectory() as folder:
            p=pathlib.Path(folder)/'binary';data=elf(62);p.write_bytes(data)
            digest=hashlib.sha256(data).hexdigest()
            self.assertEqual(m.verify_binary(p,digest),data)
            with self.assertRaises(ValueError):m.verify_binary(p,'0'*64)
            q=pathlib.Path(folder)/'linked';q.symlink_to(p)
            with self.assertRaises(ValueError):m.verify_binary(q,digest)

    @patch.object(m.platform, 'machine', return_value='x86_64')
    def test_other_release_architecture_is_refused_before_writes(self, _machine):
        with tempfile.TemporaryDirectory() as folder:
            binary = pathlib.Path(folder) / 'monitor-hub'
            payload = elf(183)
            binary.write_bytes(payload)
            argv = ['hub-install.py', '--binary', str(binary), '--sha256', hashlib.sha256(payload).hexdigest(),
                    '--site', 'https://panel.example.com']
            with patch.object(sys, 'argv', argv), patch.object(m, 'install_hub') as install:
                with self.assertRaisesRegex(ValueError, '程序架构与本机 x86_64 不匹配'):
                    m.main()
            install.assert_not_called()

    def test_private_umask_crash_and_resume(self):
        with tempfile.TemporaryDirectory() as folder:
            root=pathlib.Path(folder);code=root/'code';data=root/'data';unit=root/'unit';plan=root/'plan'
            with patch.multiple(m,CODE=code,DATA=data,UNIT=unit,PLAN=plan):
                old=os.umask(0o077)
                try:m.prepare_install('a'*64,'unit',False,owner=os.geteuid())
                finally:os.umask(old)
                self.assertEqual(code.stat().st_mode & 0o777,0o755)
                stale=code/'.install-monitor-hub';stale.write_bytes(b'partial');stale.chmod(0o755)
                m.prepare_install('a'*64,'unit',True,owner=os.geteuid())
                self.assertFalse(stale.exists())
                with self.assertRaises(ValueError):m.prepare_install('b'*64,'unit',True,owner=os.geteuid())
                (code/'unknown').write_text('keep')
                with self.assertRaises(ValueError):m.prepare_install('a'*64,'unit',True,owner=os.geteuid())
                self.assertEqual((code/'unknown').read_text(),'keep')

    @patch.object(m.platform, 'machine', return_value='x86_64')
    def test_resumed_install_requires_its_own_listener_and_keeps_running_binary(self, _machine):
        with tempfile.TemporaryDirectory() as folder:
            root = pathlib.Path(folder)
            code, data, unit, plan = (root / name for name in ('code', 'data', 'unit', 'plan'))
            proc = root / 'proc' / '123'
            (proc / 'net').mkdir(parents=True)
            (proc / 'fd').mkdir()
            table = proc / 'net' / 'tcp'
            table.write_text('header\n0: 0100007F:26B7 00000000:0000 0A 0 0 0 0 0 9876\n')
            (proc / 'exe').symlink_to(code / 'monitor-hub')
            payload = elf(62)
            args = types.SimpleNamespace(sha256=hashlib.sha256(payload).hexdigest(),
                                         site='https://panel.example.com', port=9911,
                                         resume=False)
            unit_contents = m.unit_text(args.site, args.port)
            service = {'ActiveState': 'active', 'MainPID': '123'}
            response = MagicMock(status=200, url='http://127.0.0.1:9911/api/me')
            response.read.return_value = b'{"authed": false}'
            opener = MagicMock()
            opener.open.return_value.__enter__.return_value = response
            with patch.multiple(m, CODE=code, DATA=data, UNIT=unit, PLAN=plan), \
                    patch.object(m, 'check_unit', return_value=service), \
                    patch.object(m.subprocess, 'run', return_value=types.SimpleNamespace(
                        returncode=0, stdout='Emergency password: fresh-one\n')) as run, \
                    patch.object(hub_health, 'PROC', proc.parent), \
                    patch.object(hub_health.urllib.request, 'build_opener', return_value=opener), \
                    patch.object(hub_health.time, 'monotonic', side_effect=(0, 31, 0, 31, 0)):
                with self.assertRaisesRegex(ValueError, '保留安装记录'):
                    m.install_hub(args, unit_contents, payload)
                opener.open.assert_not_called()
                self.assertTrue(plan.is_file())
                binary = code / 'monitor-hub'
                original_inode = binary.stat().st_ino
                # The already-running Hub now owns the socket on the next try.
                (proc / 'fd' / '7').symlink_to('socket:[9876]')
                args.resume = True
                response.read.return_value = b'["unexpected payload"]'
                with self.assertRaisesRegex(ValueError, '保留安装记录'):
                    m.install_hub(args, unit_contents, payload)
                self.assertTrue(plan.is_file())
                opener.open.reset_mock()
                response.read.return_value = b'{"authed": false}'
                with patch('sys.stdout', new_callable=io.StringIO) as shown:
                    m.install_hub(args, unit_contents, payload)
                # The password the first start logged is replaced; only the new one is shown.
                self.assertIn('--reset-password', run.call_args.args[0])
                self.assertIn('应急密码: fresh-one\n', shown.getvalue())
                opener.open.assert_called_once_with('http://127.0.0.1:9911/api/me', timeout=3)
                self.assertEqual(binary.stat().st_ino, original_inode)
                self.assertFalse(plan.exists())
