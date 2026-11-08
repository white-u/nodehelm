#!/usr/bin/env python3
"""Explicit Linux/root test: isolated paths, units and loopback probes. Never installs manager."""
import pathlib
import shutil
import socket
import sys
import uuid
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from controller import Controller
from protocol import Rejected
from core_driver import command
from snell import Driver


def main():
    tag = uuid.uuid4().hex[:12]
    root = pathlib.Path('/opt') / ('node-manager-test-' + tag)
    root.mkdir(mode=0o755)
    state = root / 'state'; state.mkdir(mode=0o700)
    configs = state / 'configs'; configs.mkdir(mode=0o700)
    class IsolatedDriver(Driver):
        fail_verify = False
        def unit(self, port): return f'node-manager-test-{tag}-{port}.service'
        def verify(self, after, payload):
            super().verify(after, payload)
            if self.fail_verify:
                self.fail_verify = False
                raise Rejected('injected post-apply failure')
    driver = IsolatedDriver()
    driver.binary = root / 'snell-server'; driver.configs = configs
    controller = Controller(state, driver)
    ports = []
    def execute(action, payload={}):
        return controller.execute(dict(id=uuid.uuid4().hex, action=action, revision=controller.snapshot()['revision'], payload=payload))
    clean = True
    try:
        assert execute('install')['status'] == 'succeeded'
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0)); port = sock.getsockname()[1]
        ports.append(port)
        item = dict(port=port, public_port=port, name='integration', host='127.0.0.1', mode='default')
        assert execute('apply', item)['status'] == 'succeeded'
        original = controller.snapshot()['resources'][0]
        assert driver.running(port)
        assert execute('apply', {**item, 'name':'edited'})['status'] == 'succeeded'
        assert controller.snapshot()['resources'][0]['psk'] == original['psk']
        driver.fail_verify = True
        assert execute('apply', {**item, 'name':'failed', 'psk':'b' * 32})['status'] == 'rolled_back'
        assert controller.snapshot()['resources'][0]['name'] == 'edited'
        assert controller.snapshot()['resources'][0]['psk'] == original['psk']
        assert driver.running(port)
        # Detect external edits without replacing them or publishing stale credentials.
        config, _ = driver.files(port)
        saved = config.read_text(); config.write_text(saved + '# external change\n')
        assert controller.snapshot()['blocked']
        config.write_text(saved)
        dropin = driver.units / (driver.unit(port) + '.d')
        dropin.mkdir()
        override = dropin / 'isolated-test.conf'
        try:
            override.write_text('[Service]\nEnvironment=NODE_MANAGER_ACCEPTANCE_OVERRIDE=1\n')
            command('systemctl', 'daemon-reload')
            assert controller.snapshot()['blocked']
            try:
                execute('apply', item)
            except Rejected as error:
                assert error.code == 'config_changed'
            else:
                raise AssertionError('Managed unit with a drop-in must not be modified')
        finally:
            override.unlink(missing_ok=True)
            dropin.rmdir()
            command('systemctl', 'daemon-reload')
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0)); sibling = sock.getsockname()[1]
        ports.append(sibling)
        second = dict(item, port=sibling, public_port=sibling, name='sibling')
        foreign = pathlib.Path('/run/systemd/system') / driver.unit(sibling)
        with foreign.open('x') as unit_file:
            unit_file.write('[Service]\nExecStart=/bin/true\n')
        try:
            command('systemctl', 'daemon-reload')
            try:
                execute('apply', second)
            except Rejected as error:
                assert error.code == 'config_changed'
            else:
                raise AssertionError('A foreign unit must not be taken over')
            assert not driver.files(sibling)[1].exists()
        finally:
            foreign.unlink()
            command('systemctl', 'daemon-reload')
        command('systemctl', 'stop', driver.unit(port))
        assert not driver.running(port)
        assert execute('apply', second)['status'] == 'succeeded'
        assert execute('apply', {**second, 'name':'sibling-edited'})['status'] == 'succeeded'
        assert execute('delete', {'port':sibling})['status'] == 'succeeded'
        assert not driver.running(port)
        assert execute('apply', item)['status'] == 'succeeded'
        assert driver.running(port)
        command('systemctl', 'stop', driver.unit(port))
        assert execute('delete', {'port':port})['status'] == 'succeeded'
        assert not driver.running(port)
        assert not controller.snapshot()['resources']
        print('PASS: fixed-hash install, actual systemd service, CRUD, credential preservation, rollback, drift and systemd ownership, stopped sibling isolation and repair, cleanup')
    finally:
        for port in ports:
            command('systemctl', 'stop', driver.unit(port), check=False)
            command('systemctl', 'disable', driver.unit(port), check=False)
            if driver.running(port):
                clean = False
                print('CLEANUP FAILED: service still active; retained ' + str(root), file=sys.stderr)
                continue
            (driver.units / driver.unit(port)).unlink(missing_ok=True)
        command('systemctl', 'daemon-reload')
        if clean: shutil.rmtree(root)
        else: raise RuntimeError('cleanup failed')

if __name__ == '__main__': main()
