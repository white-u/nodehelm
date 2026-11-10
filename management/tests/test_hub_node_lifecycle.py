import importlib.util
import pathlib
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = pathlib.Path(__file__).resolve().parents[2] / 'scripts'
sys.path.insert(0, str(SCRIPTS))
spec = importlib.util.spec_from_file_location('hub_node_lifecycle', SCRIPTS / 'hub-lifecycle.py')
hub = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hub)


class HubNodeLifecycleGuards(unittest.TestCase):
    def test_active_uninstall_blocks_hub_switch_and_preserves_key_requirement(self):
        with tempfile.TemporaryDirectory() as folder:
            database = pathlib.Path(folder) / 'hub.db'
            with sqlite3.connect(database) as db:
                db.execute("CREATE TABLE managed_lifecycle(status TEXT, cleanup_cipher TEXT)")
                db.execute("INSERT INTO managed_lifecycle VALUES('running','encrypted-token')")
            for state in ('running', 'paused', 'cleaning', 'finalizing'):
                with sqlite3.connect(database) as db:
                    db.execute('UPDATE managed_lifecycle SET status=?', (state,))
                with self.assertRaisesRegex(ValueError, '卸载'):
                    hub.ensure_idle(database)
            with sqlite3.connect(database) as db:
                db.execute("UPDATE managed_lifecycle SET status='succeeded'")
            hub.ensure_idle(database)
            with self.assertRaisesRegex(ValueError, '密钥'):
                hub.check_key(database, pathlib.Path(folder) / 'missing-key')

    def test_recovery_reads_the_recorded_address_once_the_unit_is_gone(self):
        with tempfile.TemporaryDirectory() as folder:
            state = pathlib.Path(folder)
            (state / hub.UNIT).mkdir()
            journal = state / hub.UNIT / 'recovery.json'
            journal.write_text('{"identity": {"health": "http://127.0.0.1:19912/api/me"}}')
            journal.chmod(0o600)
            with patch.object(hub, 'STATE', state), \
                    patch.object(hub, 'service_listen', return_value='127.0.0.1:9911'):
                self.assertEqual(hub.default_health(hub.UNIT, 'recover'), 'http://127.0.0.1:19912/api/me')
                self.assertEqual(hub.default_health(hub.UNIT, 'backup'), 'http://127.0.0.1:9911/api/me')
                journal.unlink()
                self.assertEqual(hub.default_health(hub.UNIT, 'recover'), 'http://127.0.0.1:9911/api/me')
