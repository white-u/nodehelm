import os
import pathlib
import sys
import tempfile
import unittest
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from install import prepare_directories, check_owned_unit, SERVICE_UNIT

class InstallTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(); self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name) / 'state'
        self.code = pathlib.Path(self.tmp.name) / 'code'
    def test_unlinked_owned_service_allows_rebinding(self):
        unit = pathlib.Path(self.tmp.name) / 'agent.service'
        unit.write_text(SERVICE_UNIT)
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
