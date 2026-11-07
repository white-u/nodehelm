#!/usr/bin/env python3
"""Optional outbound-only manager. Root authority is deliberately separate from monitor-agent."""
import argparse
from contextlib import ExitStack
import fcntl
import functools
import hashlib
import http.client
import io
import json
import os
import pathlib
import ssl
import sys
import time
import urllib.error
import urllib.parse
from controller import Controller
from model import PROTOCOL, Rejected, request, revision
from snell import Driver
from storage import read, write, remove, private_directory
from services import Driver as ServiceDriver
from pm import Driver as PMDriver

ROOT = pathlib.Path('/var/lib/node-manager')

def base_url(value):
    u = urllib.parse.urlsplit(value)
    if u.scheme != 'https' or not u.hostname or u.username or u.password or u.query or u.fragment or u.path not in ('', '/'):
        raise Rejected('后台地址须为 HTTPS 根地址')
    return value.rstrip('/')


def files_digest(directory, names):
    lines = ''.join(f'{name} {hashlib.sha256((directory / name).read_bytes()).hexdigest()}\n' for name in sorted(names))
    return hashlib.sha256(lines.encode()).hexdigest()


@functools.lru_cache(maxsize=1)
def component_digest():
    # What actually runs. Files change only through an upgrade, which restarts this process.
    try:
        names = read(ROOT / 'component-manifest.json')
        return files_digest(pathlib.Path(__file__).resolve().parent, names) if names else ''
    except OSError:
        return ''


@functools.lru_cache(maxsize=1)
def tls():
    return ssl.create_default_context()


# One kept-alive connection for the poll loop: a new TLS handshake every five
# seconds cost about fifteen times the CPU of the request itself. http.client
# follows no redirect and reads no proxy variable, as this channel requires.
_connection = {}


def exchange(binding, path, payload):
    base = base_url(binding['server'])
    server = urllib.parse.urlsplit(base)
    target = (server.hostname, server.port or 443)
    route = '/api/management-agent/v1/' + path
    body = json.dumps(payload).encode()
    headers = {'User-Agent': 'NodeManager/1', 'Content-Type': 'application/json', 'Authorization':'Bearer ' + binding['credential'], 'X-Node-Manager-Protocol':str(PROTOCOL), 'X-Node-Manager-Component':component_digest()}
    for attempt in (0, 1):
        reused = _connection.get('target') == target
        if not reused:
            close_connection()
            _connection.update(target=target, conn=http.client.HTTPSConnection(*target, timeout=20, context=tls()))
        conn = _connection['conn']
        try:
            conn.request('POST', route, body, headers)
            response = conn.getresponse()
            data = response.read(256 * 1024 + 1)
            if len(data) > 256 * 1024:
                close_connection()
                raise Rejected('后台响应过大')
        except (http.client.RemoteDisconnected, BrokenPipeError, ConnectionResetError) as error:
            close_connection()
            # A kept-alive connection the hub or a proxy closed while idle; the
            # same exchange on a fresh one is what the next round would send.
            if reused and attempt == 0:
                continue
            raise urllib.error.URLError(error) from None
        except (OSError, http.client.HTTPException) as error:
            close_connection()
            raise urllib.error.URLError(error) from None
        if 300 <= response.status < 400:
            close_connection()
            raise Rejected('管理地址发生跳转，请核实反向代理')
        if response.status >= 400:
            raise urllib.error.HTTPError(base + route, response.status, response.reason, response.headers, io.BytesIO(data))
        return json.loads(data)


def close_connection():
    conn = _connection.pop('conn', None)
    _connection.pop('target', None)
    if conn is not None:
        conn.close()


def confirm_connection(binding):
    confirmation = read(ROOT / 'connection-request.json')
    if confirmation is not None:
        expected = {'server': binding['server'], 'node': binding['node'],
                    'credential_sha256': hashlib.sha256(binding['credential'].encode()).hexdigest()}
        nonce = confirmation.get('nonce') if isinstance(confirmation, dict) else None
        if (not isinstance(nonce, str) or len(nonce) != 32 or any(c not in '0123456789abcdef' for c in nonce)
                or confirmation != {**expected, 'nonce': nonce}):
            raise Rejected('安装确认记录与当前绑定不一致')
        if read(ROOT / 'connection-confirmed.json') != confirmation:
            write(ROOT / 'connection-confirmed.json', confirmation)


def execute_task(controller, task):
    req = task['request']
    # A completed receipt wins over transport expiry: only a new request expires.
    receipt = read(controller.receipts / (req['id'] + '.json')) if isinstance(req.get('id'), str) and len(req['id']) == 32 and all(c in '0123456789abcdef' for c in req['id']) else None
    if receipt is None and not controller.journal.exists() and task['expires'] <= time.time():
        return {'id': req['id'], 'status': 'expired', 'message': '操作已过期，未执行'}
    try:
        return controller.execute(req)
    except Rejected as error:
        write(controller.root / 'last-error.json', {'id': req.get('id', ''), 'stage': 'prepare', 'reason': str(error)})
        return {'id': req.get('id', ''), 'status': 'rejected', 'code': error.code, 'message': '请求被拒绝，请刷新并检查本机配置'}


def execute_module_task(controllers, task):
    raw = task['request']
    try:
        req = request(raw)
    except Rejected:
        # Use the same expiry and rejection handling as local validation.
        if not isinstance(raw, dict):
            raise
        name = raw.get('module', 'snell')
        controller = controllers.get(name) if isinstance(name, str) else None
        return execute_task(controller or controllers['snell'], task)
    name = req.get('module', 'snell')
    controller = controllers.get(name)
    if controller is None:
        return reject_module(controllers['snell'], req, 'unsupported_module', '后台请求了不支持的模块')
    if (controller.receipts / (req['id'] + '.json')).exists() or controller.journal.exists() or task['expires'] <= time.time():
        return execute_task(controller, task)
    port, action = req['payload'].get('port'), req['action']
    related = {module: other.snapshot() for module, other in controllers.items() if module != name}
    if any(state['blocked'] for state in related.values()):
        return reject_module(controllers[name], req, 'config_changed', '其他模块有未恢复状态')
    owners = {module for module, state in related.items() if any(r['port'] == port for r in state['resources'])}
    if name == 'pm' and action == 'apply':
        if not any(related[module]['runtime'].get(str(port)) == 'active' for module in owners):
            return reject_module(controllers[name], req, 'pm_port', 'PM 只允许管理仍在运行的受管业务端口')
    if name != 'pm' and action == 'apply' and owners - {'pm'}:
        return reject_module(controllers[name], req, 'port_in_use', '监听端口已被其他模块保留')
    if name != 'pm' and action in ('delete', 'core_uninstall'):
        if action == 'delete' and 'pm' in owners:
            return reject_module(controller, req, 'pm_dependency', '请先移除该端口的 PM 规则')
        if action == 'core_uninstall':
            other_ports = {r['port'] for module, state in related.items() if module != 'pm'
                           for r in state['resources']}
            if any(rule['port'] not in other_ports for rule in related.get('pm', {}).get('resources', [])):
                return reject_module(controller, req, 'pm_dependency', '存在无法关联到其他业务的 PM 规则，请先核实')
    return execute_task(controllers[name], task)


def reject_module(controller, req, code, message):
    write(controller.root / 'last-error.json', {'id': req['id'], 'stage': 'prepare', 'reason': message})
    return {'id': req['id'], 'status': 'rejected', 'code': code, 'message': message}


class PMController(Controller):
    def snapshot(self):
        with (self.root / 'write.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_SH)
            state = read(self.state, {'installed': False, 'resources': []})
            drift, usage = self.driver.status(state)
            return {**state, 'revision': revision(state), 'blocked': self.journal.exists() or drift,
                    'usage': usage,
                    'core': {'version': None, 'sha256': None, 'previous_version': None, 'candidates': []}}


def recover_idle(receipt_path):
    from install import SERVICE, check_agent_unit, ctl, lock

    if receipt_path.exists() and read(receipt_path) is None:
        try:
            with lock(ROOT / 'agent.lock'):
                if receipt_path.exists() and read(receipt_path) is None:
                    remove(receipt_path)
        except ValueError as error:
            print(f'暂未清理旧空回执：{error}', file=sys.stderr, flush=True)
    if read(ROOT / 'binding.json'):
        info = check_agent_unit()
        if info['LoadState'] == 'loaded' and info.get('ActiveState') != 'active':
            ctl('start', SERVICE)
    return {'status': 'idle', 'message': '没有待恢复操作'}


def stop_owned_agent():
    from install import SERVICE, check_agent_unit, ctl

    try:
        info = check_agent_unit()
        if info['LoadState'] == 'not-found':
            if info.get('FragmentPath') != '' or info.get('DropInPaths') != '':
                raise ValueError('检测到同名管理服务或额外配置，拒绝解绑')
        else:
            ctl('stop', SERVICE)
    except ValueError as error:
        raise Rejected(str(error)) from None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('action', choices=['run', 'recover', 'status', 'unlink', 'refresh'])
    args = parser.parse_args()
    if os.geteuid() != 0:
        raise Rejected('管理组件需要 root 权限')
    # The unit's UMask=0077, also from a shell: files created here are checked as private later.
    os.umask(0o077)
    from install import LIFECYCLE_LOCK, PURGE_JOURNAL, lock, present

    try:
        with ExitStack() as locks:
            # refresh runs on behalf of install.py, which already holds this lock.
            if args.action not in ('run', 'refresh'):
                locks.enter_context(lock(LIFECYCLE_LOCK))
            if present(PURGE_JOURNAL):
                raise Rejected('组件清理尚未完成，请使用相同安装器重试 uninstall --purge')
            if args.action in ('recover', 'unlink'):
                locks.enter_context(lock(private_directory(ROOT) / 'lifecycle.lock'))
                if (ROOT / 'component-recovery.json').exists():
                    raise Rejected('组件升级或卸载尚未恢复，请先执行 install.py recover')
            if args.action == 'run':
                locks.enter_context(lock(ROOT / 'agent.lock'))
                if present(PURGE_JOURNAL):
                    raise Rejected('组件清理尚未完成，请使用相同安装器重试 uninstall --purge')
            run_action(args.action)
    except Rejected:
        raise
    except ValueError as error:
        raise Rejected(str(error)) from None


def run_action(action):
    if action == 'run' and read(ROOT / 'component-recovery.json', {}).get('phase') == 'switching':
        raise Rejected('组件文件正在切换，暂停轮询直至升级恢复')
    controllers = {'snell': Controller(ROOT, Driver())}
    controllers['pm'] = PMController(ROOT / 'pm', PMDriver(ROOT / 'pm'), 'pm')
    for module in ('sb', 'fw'):
        directory = private_directory(ROOT / module)
        private_directory(directory / 'configs')
        controllers[module] = Controller(directory, ServiceDriver(module), module)
    controller = controllers['snell']
    if action == 'refresh':
        for current in controllers.values():
            current.refresh()
        # An upgrade runs the old installer, which reaches the new code only here.
        from install import place_command
        try:
            place_command()
        except (OSError, ValueError) as error:
            print(f'未安装 nodehelm 命令：{error}', file=sys.stderr)
        return
    def snapshot():
        state = controller.snapshot()
        state['modules'] = {name:c.snapshot() for name,c in controllers.items() if name != 'snell'}
        state['blocked'] = state['blocked'] or any(v['blocked'] for v in state['modules'].values())
        return state
    if action == 'status':
        state = snapshot()
        for current in [state, *state['modules'].values()]:
            for item in current['resources']:
                for key in ('psk', 'uuid', 'private_key'): item.pop(key, None)
        state['last_error'] = {name:read(c.root / 'last-error.json') for name,c in controllers.items()}
        print(json.dumps(state, ensure_ascii=False)); return
    if action == 'recover':
        pm = controllers['pm']
        if read(pm.journal, {}).get('local'):
            # The PM writer lock serializes local recovery. No Hub task owns
            # the local operation, so leave the poller and its receipt alone.
            print(json.dumps(pm.recover(), ensure_ascii=False))
            return
        receipt_path = ROOT / 'pending-result.json'
        lifecycle_job = read(ROOT / 'lifecycle-job.json')
        current = lifecycle_job.get('current') if lifecycle_job else None
        if current and any(c.journal.exists() for c in controllers.values()):
            from snell import command
            command('systemctl', 'stop', 'node-manager-agent.service')
            owner = controllers.get(current['module'])
            pending = [c for c in controllers.values() if c.journal.exists()]
            if len(pending) != 1 or owner not in pending or read(owner.journal).get('id') != current['id']:
                raise Rejected('恢复事务不属于当前生命周期步骤，请保留现场')
            result = owner.recover()
            if result['status'] == 'succeeded':
                lifecycle_job.update(index=lifecycle_job['index'] + 1, current=None)
            lifecycle_job.update(status='paused', message='lifecycle_recovery')
            write(ROOT / 'lifecycle-job.json', lifecycle_job)
            print(json.dumps(result, ensure_ascii=False))
            if result['status'] != 'recovery_failed': command('systemctl', 'start', 'node-manager-agent.service')
            return
        if not any(c.journal.exists() for c in controllers.values()):
            print(json.dumps(recover_idle(receipt_path), ensure_ascii=False))
            return
        # Stop the poller before selecting the module because its operation
        # can complete and a different task can arrive during this command.
        from snell import command
        command('systemctl', 'stop', 'node-manager-agent.service')
        pending = [c for c in controllers.values() if c.journal.exists()]
        if len(pending) > 1: raise Rejected('多个模块有未恢复操作，请保留现场检查')
        result = (pending[0] if pending else controller).recover()
        if result.get('id'):
            write(receipt_path, result)
        print(json.dumps(result, ensure_ascii=False))
        if result['status'] != 'recovery_failed': command('systemctl', 'start', 'node-manager-agent.service')
        return
    if action == 'unlink':
        from install import lock, present
        from lifecycle import ACTIVE, JOB, RECOVERY

        def revocation():
            """None once the Hub explicitly revoked this binding, else why that is unknown."""
            binding = read(ROOT / 'binding.json')
            if not binding:
                return '缺少可核实的原绑定或不支持此卸载范围'
            try:
                # Read-only: never poll here, since polling can claim work.
                exchange(binding, 'maintenance', {'action': 'status', 'token': '0' * 64})
                return '后台仍接受原绑定'
            except urllib.error.HTTPError as error:
                with error:
                    if error.code == 401 and (error.headers or {}).get('X-Node-Manager-Binding') == 'revoked':
                        return None
                    return f'后台未明确确认撤权（HTTP {error.code}）'
            except (urllib.error.URLError, OSError, ValueError) as error:
                return f'绑定状态探测失败（{type(error).__name__}）'

        def checked_job():
            job = read(ROOT / JOB)
            if present(RECOVERY):
                raise Rejected('节点卸载尚未完成，禁止清除身份；请从后台继续生命周期操作')
            if job and job.get('status') in ACTIVE:
                # A Hub that gave up on this node can never continue the job.
                if revocation() is not None:
                    raise Rejected('节点卸载尚未完成，禁止清除身份；请从后台继续生命周期操作')
            elif job and job.get('status') == 'succeeded' and job.get('confirmed') is not True:
                reason = '缺少可核实的原绑定或不支持此卸载范围' if job.get('scope') == 'all' else revocation()
                if reason is not None:
                    raise Rejected(f'卸载回执尚未得到后台确认，请恢复连接后重试；{reason}')
            return job

        checked_job()
        stop_owned_agent()
        with lock(ROOT / 'agent.lock'):
            # The poller may have advanced after the first check, before it stopped.
            job = checked_job()
            if snapshot()['blocked']:
                raise Rejected('仍有未恢复操作，禁止重新绑定；请先 recover')
            # Terminal transport state belongs to the old binding. Preserve module
            # receipts and audit history, but never send this job to a new identity.
            if job and (job.get('status') == 'succeeded' or job.get('status') in ACTIVE):
                remove(ROOT / JOB)
            remove(ROOT / 'binding.json')
            remove(ROOT / 'enrollment.json')
            remove(ROOT / 'pending-result.json')
        print('本机已解绑；业务服务和监控保留。旧后台若离线，请恢复后撤销原绑定。')
        return
    binding = read(ROOT / 'binding.json')
    if not binding: raise Rejected('尚未绑定，请运行安装命令')
    from lifecycle import Lifecycle
    lifecycle = Lifecycle(controllers, execute_module_task, snapshot, binding, ROOT)
    # Never automatically erase an unresolved journal on boot.
    while True:
        executed = False
        try:
            receipt_path = ROOT / 'pending-result.json'
            pending = read(receipt_path)
            payload = {'snapshot': snapshot(), 'result': pending}
            lifecycle_result = lifecycle.receipt()
            if lifecycle_result:
                payload['lifecycle_result'] = lifecycle_result
            reply = exchange(binding, 'poll', payload)
            lifecycle.acknowledge(reply.get('lifecycle_ack'))
            confirm_connection(binding)
            if receipt_path.exists():
                remove(receipt_path)
            if reply.get('lifecycle'):
                lifecycle.advance(reply['lifecycle'])
            elif reply.get('task') and lifecycle.active():
                raise Rejected('生命周期卸载尚未完成，拒绝普通操作')
            elif reply.get('task'):
                result = execute_module_task(controllers, reply['task'])
                write(ROOT / 'pending-result.json', result)
                executed = True
        except urllib.error.HTTPError as error:
            if error.code == 409:
                # Nothing was exchanged, so pending results survive until both sides match.
                print('管理组件与后台协议不一致，暂停任务；请升级管理组件或后台。', file=sys.stderr, flush=True)
                time.sleep(5)
                continue
            print(f'后台拒绝管理请求（HTTP {error.code}）；检查绑定是否撤销、接口版本及反向代理；403 还需检查 CDN 是否拦截机器请求。', file=sys.stderr, flush=True)
        except urllib.error.URLError as error:
            print(f'HTTPS 连接失败（{type(error.reason).__name__}）；检查证书、DNS 和后台可达性。', file=sys.stderr, flush=True)
        except (OSError, ValueError, KeyError) as error:
            print(f'本地状态或响应异常（{type(error).__name__}）；保留操作记录，请执行 status 检查。', file=sys.stderr, flush=True)
        # A finished task is reported at once with the state after it: the page
        # waits on that result. Lifecycle steps keep the interval, as a step can
        # be waiting on the Hub without doing anything.
        if not executed:
            time.sleep(5)

if __name__ == '__main__':
    try: main()
    except (Rejected, OSError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
