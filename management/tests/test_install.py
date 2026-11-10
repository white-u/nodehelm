import contextlib
import io
import os
import pathlib
import sys
import tempfile
import unittest
from unittest.mock import MagicMock
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from install import prepare_directories, check_owned_unit, SERVICE_UNIT

class InstallTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(); self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name) / 'state'
        self.code = pathlib.Path(self.tmp.name) / 'code'
    def test_unlinked_owned_service_allows_rebinding(self):
        unit = pathlib.Path(self.tmp.name) / 'agent.service'
        unit.write_text(SERVICE_UNIT); unit.chmod(0o644)
        check_owned_unit(unit)
    def test_modified_service_is_rejected_even_when_bound(self):
        unit = pathlib.Path(self.tmp.name) / 'agent.service'
        unit.write_text(SERVICE_UNIT + '\nEnvironment=UNEXPECTED=yes\n')
        with self.assertRaises(ValueError): check_owned_unit(unit)
    def test_private_umask_keeps_code_executable_and_state_private(self):
        old = os.umask(0o077)
        try: prepare_directories(self.root, self.code, os.geteuid())
        finally: os.umask(old)
        self.assertEqual(self.code.stat().st_mode & 0o777, 0o755)
        self.assertEqual(self.root.stat().st_mode & 0o777, 0o700)
        self.assertEqual((self.root / 'configs').stat().st_mode & 0o777, 0o700)
    def test_writable_existing_code_is_rejected(self):
        self.code.mkdir(); self.code.chmod(0o777)
        with self.assertRaises(ValueError): prepare_directories(self.root, self.code, os.geteuid())
    def test_symlink_target_is_not_adopted(self):
        self.code.symlink_to(self.tmp.name, target_is_directory=True)
        with self.assertRaises(ValueError): prepare_directories(self.root, self.code, os.geteuid())

if __name__ == '__main__': unittest.main()


class NodeCommandTests(unittest.TestCase):
    """`sudo nodehelm` belongs to the component: placed with it, never over another, gone with it."""
    def setUp(self):
        import install
        from unittest.mock import patch
        tmp = tempfile.TemporaryDirectory(); self.addCleanup(tmp.cleanup)
        self.command = pathlib.Path(tmp.name) / 'nodehelm'
        patcher = patch.object(install, 'COMMAND', self.command); patcher.start(); self.addCleanup(patcher.stop)
        self.install = install

    def test_placed_executable_and_rewritten_only_when_changed(self):
        self.install.place_command()
        self.assertEqual(self.command.read_bytes(), self.install.COMMAND_TEXT)
        self.assertEqual(self.command.stat().st_mode & 0o777, 0o755)
        inode = self.command.stat().st_ino
        self.install.place_command()
        self.assertEqual(self.command.stat().st_ino, inode)

    def test_another_programs_command_is_left_alone(self):
        self.command.write_bytes(b'#!/bin/sh\necho mine\n')
        self.install.place_command()
        self.install.remove_command()
        self.assertEqual(self.command.read_bytes(), b'#!/bin/sh\necho mine\n')

    def test_removed_with_the_component(self):
        self.install.place_command()
        self.install.remove_command()
        self.assertFalse(self.command.exists())
        self.install.remove_command()


class MenuTests(unittest.TestCase):
    def test_unknown_argument_is_refused_before_anything_runs(self):
        import menu
        from unittest.mock import patch
        # main() restores the default SIGPIPE, which would then kill this test process.
        with patch('os.geteuid', return_value=0), patch('signal.signal'), patch.object(menu, 'run') as run:
            with self.assertRaises(SystemExit):
                menu.main(['purge'])
            run.assert_not_called()

    def test_probe_is_reinstalled_only_when_the_hub_serves_a_different_binary(self):
        import menu, subprocess
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as tmp:
            binary = pathlib.Path(tmp, 'monitor-agent')
            binary.write_bytes(b'v1')
            served = lambda body, code=0: patch('subprocess.run', return_value=subprocess.CompletedProcess((), code, body, b''))
            with patch.object(menu, 'AGENT', binary), patch('platform.machine', return_value='x86_64'):
                with served(b'v1') as run:
                    self.assertTrue(menu.agent_current('https://hub.example'))
                self.assertEqual(run.call_args.args[0][-1], 'https://hub.example/agent/x86_64')
                with served(b'v2'):
                    self.assertFalse(menu.agent_current('https://hub.example'))
                with served(b'', 22):
                    self.assertFalse(menu.agent_current('https://hub.example'))
            with patch.object(menu, 'AGENT', binary), patch('platform.machine', return_value='riscv64'):
                self.assertFalse(menu.agent_current('https://hub.example'))

    def test_interrupted_work_is_finished_first_but_a_running_one_is_left_alone(self):
        import fcntl, menu
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / 'pm').mkdir()
            (root / 'pm' / 'recovery.json').write_text('{}')
            upgrade = MagicMock()
            with patch.object(menu, 'ROOT', root), patch('os.geteuid', return_value=0), patch('signal.signal'), \
                    patch('os.umask'), patch.dict(menu.ACTIONS, upgrade=upgrade):
                self.assertEqual(menu.interrupted(), (False, [root / 'pm']))
                with (root / 'pm' / 'write.lock').open('a') as held:
                    fcntl.flock(held, fcntl.LOCK_EX)
                    with patch.object(menu.subprocess, 'run') as run, self.assertRaisesRegex(SystemExit, '正在进行'):
                        menu.main(['upgrade'])
                    run.assert_not_called()
                done = menu.subprocess.CompletedProcess((), 0, '{"status": "rolled_back"}', '')
                shown = io.StringIO()
                shown.reconfigure = lambda **_: None  # main() line-buffers the real stdout.
                with patch.object(menu.subprocess, 'run', return_value=done) as run, contextlib.redirect_stdout(shown):
                    menu.main(['upgrade'])
                self.assertEqual(run.call_args.args[0][-1], 'recover')
                self.assertIn('已完成', shown.getvalue())
                failed = menu.subprocess.CompletedProcess((), 0, '{"status": "recovery_failed", "message": "x"}', '')
                with patch.object(menu.subprocess, 'run', return_value=failed), \
                        contextlib.redirect_stdout(shown), self.assertRaisesRegex(SystemExit, '恢复失败：x'):
                    menu.main(['upgrade'])
            upgrade.assert_not_called()


class PrivateUmaskTests(unittest.TestCase):
    """Run from a root shell, the tools still create state the way the unit's UMask=0077 does."""
    def test_agent_and_menu_set_the_unit_umask(self):
        import agent, menu
        from unittest.mock import patch
        with patch('os.geteuid', return_value=0), patch('os.umask') as umask, \
                patch.object(sys, 'argv', ['agent.py', 'status']), patch.object(agent, 'run_action'), \
                patch('install.present', return_value=False), patch('install.lock'):
            agent.main()
        umask.assert_called_with(0o077)
        with patch('os.geteuid', return_value=0), patch('signal.signal'), patch('os.umask') as umask, \
                patch.object(menu, 'interrupted', return_value=(False, [])), patch.dict(menu.ACTIONS, logs=lambda: None):
            menu.main(['logs'])
        umask.assert_called_with(0o077)
