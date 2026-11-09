#!/usr/bin/env python3
"""Durable, node-local removal. Only fixed Controller actions and owned files."""
import contextlib
import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import time
import urllib.error

import install

JOB = 'lifecycle-job.json'
RECOVERY = pathlib.Path('/var/lib/node-manager-cleanup')
UNIT = 'node-manager-cleanup.service'
MODULES = ('snell', 'sb', 'fw', 'pm')
ACTIVE = ('running', 'paused', 'cleaning')
STAGES = ('preflight', 'pm', 'instances', 'cores', 'cleanup', 'finished')


def unit_content():
    return f'''[Unit]
Description=Finish owned Node Manager removal
After=network-online.target
Wants=network-online.target
[Service]
Type=oneshot
ExecStart=/usr/bin/python3 -B {RECOVERY}/lifecycle.py finish
Restart=on-failure
RestartSec=30
UMask=0077
Environment=PYTHONDONTWRITEBYTECODE=1
[Install]
WantedBy=multi-user.target
'''


def descriptor(value):
    if (not isinstance(value, dict) or set(value) - {'id', 'scope', 'attempt', 'cleanup_token'}
            or not isinstance(value.get('id'), str) or not re.fullmatch('[0-9a-f]{32}', value['id'])
            or value.get('scope') not in (*MODULES, 'all')
            or type(value.get('attempt')) is not int or value['attempt'] < 1
            or ('cleanup_token' in value and (not isinstance(value['cleanup_token'], str)
                or not re.fullmatch('[0-9a-f]{64}', value['cleanup_token'])))):
        raise ValueError('生命周期请求格式无效')
    return value


def result(record):
    return {key: record[key] for key in ('id', 'attempt', 'status', 'stage', 'message') if key in record}


def step_id(job, index):
    return hashlib.sha256(f"{job['id']}:{job['attempt']}:{index}".encode()).hexdigest()[:32]


class Lifecycle:
    def __init__(self, controllers, execute, snapshot, binding, root=None, local=False):
        self.controllers = controllers
        self.execute = execute
        self.snapshot = snapshot
        self.binding = binding
        self.root = pathlib.Path(root) if root is not None else install.ROOT
        self.path = self.root / JOB
        # Started on the node itself: no Hub hears of it or waits on it.
        self.local = local

    def saved(self):
        return install.read_json(self.path)

    def active(self):
        saved = self.saved()
        return bool((saved and saved['status'] in ACTIVE) or install.present(RECOVERY))

    def receipt(self):
        saved = self.saved()
        return result(saved) if saved and not saved.get('local') else None

    def acknowledge(self, ack):
        record = self.saved()
        if (not isinstance(ack, dict) or not record or record.get('status') != 'succeeded'
                or record.get('scope') == 'all' or ack.get('id') != record.get('id')
                or type(ack.get('attempt')) is not int or ack['attempt'] != record.get('attempt')):
            return
        if record.get('confirmed') is not True:
            record['confirmed'] = True
            self.save(record)

    def save(self, record):
        install.write_json(self.path, record)
        return result(record)

    def pause(self, record, error, code):
        record.update(status='paused', message=code)
        # Diagnostics are local and may contain paths; the wire contains a fixed code.
        record['diagnostic'] = str(error)
        return self.save(record)

    def preflight(self, scope):
        if install.present(install.PURGE_JOURNAL) or install.present(self.root / install.JOURNAL):
            raise ValueError('组件维护或清理尚未完成')
        if install.present(self.root / 'pending-result.json') or install.present(self.root / 'enrollment.json'):
            raise ValueError('存在未确认普通操作或登记')
        states = {name: controller.snapshot() for name, controller in self.controllers.items()}
        if any(state['blocked'] for state in states.values()):
            raise ValueError('模块归属或恢复状态尚未核实')
        for controller in self.controllers.values():
            if install.present(controller.journal) or install.present(controller.root / 'nft-transition.json'):
                raise ValueError('存在未恢复业务事务')
        selected = MODULES if scope == 'all' else (scope,)
        ports = {item['port'] for name in selected if name != 'pm' for item in states[name]['resources']}
        for name in selected:
            controller = self.controllers[name]
            if name != 'pm':
                controller.driver.inspect_inventory(states[name])
        if scope == 'all':
            install.purge_plan(lifecycle=True, preflight=True)
        steps = []
        for item in states['pm']['resources']:
            if scope in ('all', 'pm') or item['port'] in ports:
                steps.append({'module': 'pm', 'action': 'delete', 'payload': {'port': item['port']}, 'stage': 'pm'})
        for name in selected:
            if name != 'pm':
                steps.extend({'module': name, 'action': 'delete', 'payload': {'port': item['port']}, 'stage': 'instances'}
                             for item in states[name]['resources'])
        for name in selected:
            if states[name]['installed']:
                steps.append({'module': name, 'action': 'core_uninstall', 'payload': {}, 'stage': 'cores'})
        return steps

    def advance(self, raw):
        incoming = descriptor(raw)
        worker = install.read_json(RECOVERY / 'state.json') if install.present(RECOVERY) else None
        if worker:
            if worker['id'] != incoming['id'] or incoming['scope'] != 'all':
                raise ValueError('另一个节点清理执行器尚未收尾')
            start_worker()
            return {'id': incoming['id'], 'attempt': incoming['attempt'], 'status': 'cleaning', 'stage': 'cleanup'}
        with install.lock(self.root / 'lifecycle.lock'):
            record = self.saved()
            if record and record['id'] != incoming['id'] and record['status'] in ACTIVE:
                raise ValueError('已有未完成的本机卸载')
            if not record or record['id'] != incoming['id']:
                record = {**incoming, 'status': 'running', 'stage': 'preflight', 'index': 0, 'steps': None, 'current': None}
                record.pop('cleanup_token', None)
                if self.local:
                    record['local'] = True
                self.save(record)
            if bool(record.get('local')) != self.local:
                raise ValueError('本机卸载与后台卸载不能互相接续')
            if record['scope'] != incoming['scope'] or incoming['attempt'] < record['attempt']:
                raise ValueError('生命周期请求与本机持久记录不一致')
            if incoming['attempt'] > record['attempt']:
                record.update(attempt=incoming['attempt'], status='running')
                record.pop('message', None)
                record.pop('confirmed', None)
                # An interrupted own transaction must be resolved before choosing a new revision/id.
                current = record.get('current')
                if current:
                    controller = self.controllers[current['module']]
                    if install.present(controller.journal):
                        journal = install.read_json(controller.journal)
                        if journal.get('id') != current['id']:
                            return self.pause(record, ValueError('恢复事务不属于当前步骤'), 'lifecycle_recovery')
                        recovered = controller.recover()
                        if recovered['status'] == 'recovery_failed':
                            return self.pause(record, ValueError('步骤恢复未完成'), 'lifecycle_recovery')
                    saved = install.read_json(controller.receipts / (current['id'] + '.json'))
                    if saved and saved['result']['status'] == 'succeeded':
                        record['index'] += 1
                    record['current'] = None
                self.save(record)
            if record['status'] in ('paused', 'succeeded'):
                return result(record)
            try:
                current = record.get('current')
                if current:
                    controller = self.controllers[current['module']]
                    if install.present(controller.journal):
                        journal = install.read_json(controller.journal)
                        if journal.get('id') != current['id']:
                            raise ValueError('存在不属于当前步骤的恢复事务')
                        recovered = controller.recover()
                        if recovered['status'] == 'recovery_failed':
                            raise ValueError('当前步骤恢复未完成')
                # Preflight again on resume and before any new deletion. This never changes the plan.
                planned = self.preflight(record['scope'])
                if record['steps'] is None:
                    record['steps'] = planned
                    self.save(record)
                if record['index'] < len(record['steps']):
                    step = record['steps'][record['index']]
                    record['stage'] = step['stage']
                    if not current:
                        state = self.controllers[step['module']].snapshot()
                        current = {'id': step_id(record, record['index']), 'module': step['module'],
                                   'action': step['action'], 'payload': step['payload'], 'revision': state['revision']}
                        record['current'] = current
                        self.save(record)
                    executed = self.execute(self.controllers, {'request': current, 'expires': int(time.time()) + 3600})
                    if executed['status'] != 'succeeded':
                        raise ValueError('步骤未成功，已完成步骤保留；请检查本机记录')
                    record.update(index=record['index'] + 1, current=None, status='running')
                    return self.save(record)
                if planned:
                    raise ValueError('卸载计划完成后仍有受管资源，拒绝扩大删除范围')
                record.update(status='cleaning' if record['scope'] == 'all' else 'succeeded',
                              stage='cleanup' if record['scope'] == 'all' else 'finished')
                self.save(record)
                if record['scope'] == 'all' and incoming.get('cleanup_token'):
                    prepare_worker(record, incoming['cleanup_token'], self.binding, self.snapshot(), self.local)
                return result(record)
            except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
                return self.pause(record, error, 'lifecycle_step' if record.get('current') else 'lifecycle_preflight')


def terminal_script():
    # This program lives in the persistent unit before any helper file is removed.
    # It deliberately has no imports from the component or recovery directories.
    return f"""import hashlib,json,os,pathlib,stat,subprocess
root=pathlib.Path({str(RECOVERY)!r})
unit=pathlib.Path({str(install.UNITS / UNIT)!r})
def owned(path,directory=False):
 s=path.lstat()
 if s.st_uid!=os.geteuid() or s.st_mode & (0o077 if path.is_relative_to(root) else 0o022) or (not stat.S_ISDIR(s.st_mode) if directory else not stat.S_ISREG(s.st_mode) or s.st_nlink!=1):
  raise ValueError('unowned cleanup residue')
def sync(path):
 fd=os.open(path,os.O_RDONLY|os.O_DIRECTORY)
 try: os.fsync(fd)
 finally: os.close(fd)
if root.exists() or root.is_symlink():
 owned(root,True)
 paths=list(root.iterdir())
 if any(p.name not in ('install.py','lifecycle.py','state.json','worker.lock') for p in paths): raise ValueError('unknown cleanup residue')
 state=root/'state.json'
 if state.exists() or state.is_symlink():
  owned(state)
  saved=json.loads(state.read_bytes())
  if saved.get('phase')!='finished': raise ValueError('completion unconfirmed')
  for path in paths:
   owned(path)
   if path.name in saved['hashes'] and hashlib.sha256(path.read_bytes()).hexdigest()!=saved['hashes'][path.name]: raise ValueError('changed helper')
  for name in ('install.py','lifecycle.py','worker.lock'):
   path=root/name
   if path.exists(): path.unlink(); sync(root)
  state.unlink(); sync(root)
 elif paths: raise ValueError('missing completion state')
 root.rmdir(); sync(root.parent)
# Held by this process until it exits; with the component gone there is nothing left to serialise.
lock=pathlib.Path({str(install.LIFECYCLE_LOCK)!r})
if lock.exists() and not lock.is_symlink():
 owned(lock); lock.unlink()
subprocess.run(('/usr/bin/systemctl','disable',{UNIT!r}),check=True,stdin=subprocess.DEVNULL,capture_output=True,timeout=45)
if unit.exists() or unit.is_symlink():
 owned(unit)
 unit.unlink(); sync(unit.parent)
subprocess.run(('/usr/bin/systemctl','daemon-reload'),check=True,stdin=subprocess.DEVNULL,capture_output=True,timeout=45)
"""


def terminal_unit_content():
    return unit_content().replace(f'ExecStart=/usr/bin/python3 -B {RECOVERY}/lifecycle.py finish',
                                  'ExecStart=/usr/bin/python3 -c ' + json.dumps(terminal_script()))


def check_worker_files(state):
    install.owned_directory(RECOVERY, True)
    if not isinstance(state, dict):
        raise ValueError('清理执行器缺少持久状态')
    expected = {'install.py', 'lifecycle.py', 'state.json', 'worker.lock'}
    if any(path.name not in expected for path in RECOVERY.iterdir()):
        raise ValueError('清理执行器目录含未知文件')
    for name, digest in state['hashes'].items():
        path = RECOVERY / name
        if state['phase'] == 'finished' and not install.present(path):
            continue
        if install.digest(install.owned_file(path, private=True).read_bytes()) != digest:
            raise ValueError('清理执行器文件未通过持久清单校验')
    unit = install.UNITS / UNIT
    if install.present(unit):
        actual = install.owned_file(unit).read_bytes()
        allowed = {unit_content().encode()}
        if state['phase'] == 'finished':
            allowed.add(terminal_unit_content().encode())
        if actual not in allowed:
            raise ValueError('清理执行器单元被修改')
    return state


def start_worker():
    state = check_worker_files(install.read_json(RECOVERY / 'state.json'))
    unit = install.UNITS / UNIT
    if not install.present(unit):
        install.write_file(unit, (terminal_unit_content() if state['phase'] == 'finished' else unit_content()).encode(), 0o644)
    install.ctl('daemon-reload')
    install.unit_info(UNIT)
    install.ctl('enable', UNIT)
    install.ctl('start', '--no-block', UNIT)


def prepare_worker(job, token, binding, snapshot, local=False):
    # Saved in the state because the helper outlives protocol.py.
    from protocol import PROTOCOL
    current = install.installed_manifest()
    install.check_current(current)
    install.check_owned_unit(install.UNITS / UNIT, unit_content())
    if install.present(RECOVERY):
        state = check_worker_files(install.read_json(RECOVERY / 'state.json'))
        if state['id'] != job['id']:
            raise ValueError('其他清理执行器仍保留恢复材料')
        start_worker()
        return
    contents = {name: install.owned_file(install.CODE / name).read_bytes() for name in ('install.py', 'lifecycle.py')}
    hashes = {name: install.digest(data) for name, data in contents.items()}
    if any(current.get(name) != digest for name, digest in hashes.items()):
        raise ValueError('清理执行器来源未通过组件清单校验')
    state = {'id': job['id'], 'attempt': job['attempt'], 'scope': 'all', 'binding': binding,
             'snapshot': snapshot, 'cleanup_token': token, 'phase': 'preparing', 'hashes': hashes,
             'cleaning_attempt': job['attempt'], 'protocol': PROTOCOL, 'local': local}
    staged = RECOVERY.with_name('.' + RECOVERY.name + '-' + job['id'] + '.preparing')
    if install.present(staged):
        install.owned_directory(staged, True)
        # This job-specific unpublished directory is owned by the durable local job.
        for path in staged.iterdir():
            install.owned_file(path, private=True)
            if path.name in hashes:
                if install.digest(path.read_bytes()) != hashes[path.name]:
                    raise ValueError('未发布执行器含未知代码')
            elif path.name == 'state.json':
                saved = install.read_json(path)
                if saved.get('id') != job['id'] or saved.get('hashes') != hashes or saved.get('phase') != 'preparing':
                    raise ValueError('未发布执行器记录不一致')
            elif not re.fullmatch(r'\.node-manager-[a-z0-9_]{8}', path.name):
                raise ValueError('未发布执行器含未知文件')
        for path in staged.iterdir():
            install.remove(path)
    else:
        staged.mkdir(mode=0o700)
        install.owned_directory(staged, True)
    install.write_json(staged / 'state.json', state)
    for name, data in contents.items():
        install.write_file(staged / name, data)
    install.sync(staged)
    os.rename(staged, RECOVERY)
    install.sync(RECOVERY.parent)
    install.write_file(install.UNITS / UNIT, unit_content().encode(), 0o644)
    start_worker()


def revoked(error):
    return error.code == 401 and (error.headers or {}).get('X-Node-Manager-Binding') == 'revoked'


def finish_request(state, phase):
    if state.get('local'):
        return
    try:
        response = json.loads(install.request(state['binding']['server'], '/api/management-agent/v1/lifecycle/finish',
                                             {'id': state['id'], 'phase': phase}, state['cleanup_token']))
    except urllib.error.HTTPError as error:
        # The Hub gave up on this job while the node was away. The purge already ran, so finish locally.
        if revoked(error):
            return
        raise
    if response.get('id') != state['id'] or response.get('status') not in ('finalizing', 'succeeded'):
        raise ValueError('后台清理确认响应无效')
    if phase == 'finished' and response['status'] != 'succeeded':
        raise ValueError('后台尚未持久确认卸载完成')


def resume(state):
    state.update(phase=state.get('resume_phase') if state['phase'] == 'paused' else state['phase'],
                 cleaning_attempt=state['attempt'])
    install.write_json(RECOVERY / 'state.json', state)
    return True


def report_state(state, status):
    if state.get('local'):
        # Nobody else decides when a local removal continues: every run retries it.
        return resume(state)
    # The normal credential is kept only until substantive cleanup is acknowledged.
    from urllib.request import Request, ProxyHandler, build_opener
    payload = {'snapshot': state['snapshot'], 'result': None, 'lifecycle_result': {
        'id': state['id'], 'attempt': state['attempt'], 'status': status, 'stage': 'cleanup', **({'message': 'lifecycle_cleanup'} if status == 'paused' else {})}}
    request = Request(install.base_url(state['binding']['server']) + '/api/management-agent/v1/poll',
                      json.dumps(payload).encode(), headers={'Content-Type': 'application/json',
                      'User-Agent': 'NodeManager/1', 'Authorization': 'Bearer ' + state['binding']['credential'],
                      'X-Node-Manager-Protocol': str(state['protocol'])})
    try:
        with build_opener(ProxyHandler({}), install.NoRedirect()).open(request, timeout=25) as response:
            reply = json.loads(response.read(256 * 1024))
    except urllib.error.HTTPError as error:
        if not revoked(error):
            raise
        # The node was removed from the Hub; nobody can resume this, so complete what was asked.
        return resume(state)
    following = reply.get('lifecycle')
    if following:
        descriptor(following)
        if following['id'] == state['id'] and following['attempt'] > state['attempt']:
            state.update(attempt=following['attempt'], phase=state['resume_phase'], cleaning_attempt=None)
            install.write_json(RECOVERY / 'state.json', state)
            return True
        if status == 'cleaning' and following['id'] == state['id'] and following['attempt'] == state['attempt'] and following.get('cleanup_token') == state['cleanup_token']:
            state['cleaning_attempt'] = state['attempt']
            install.write_json(RECOVERY / 'state.json', state)
            return True
    return False


def cleanup_helper(state):
    check_worker_files(state)
    install.unit_info(UNIT)
    # Publish a standalone finalizer before removing any of its Python dependencies.
    install.write_file(install.UNITS / UNIT, terminal_unit_content().encode(), 0o644)
    install.ctl('daemon-reload')
    exec(compile(terminal_script(), '<node-manager-finalizer>', 'exec'), {'__name__': '__main__'})


def worker_step(state):
    check_worker_files(state)
    if state['phase'] == 'paused':
        if not report_state(state, 'paused'):
            raise ValueError('清理已暂停，等待后台继续操作；恢复入口仍保留')
    if state['phase'] in ('preparing', 'purging') and state.get('cleaning_attempt') != state['attempt']:
        if not report_state(state, 'cleaning'):
            raise ValueError('后台尚未确认本次清理恢复')
    if state['phase'] == 'preparing':
        install.check_agent_unit()
        install.ctl('stop', install.SERVICE)
        with install.lock(install.ROOT / 'agent.lock'):
            job = install.read_json(install.ROOT / JOB)
            if job and (job['id'] != state['id'] or job['scope'] != 'all'):
                raise ValueError('本机卸载记录与清理执行器不一致')
            if job:
                install.remove(install.ROOT / JOB)
            plan = install.read_json(install.PURGE_JOURNAL)
            if plan is None:
                plan = install.purge_plan(lifecycle=True)
                install.write_json(install.PURGE_JOURNAL, plan)
            state.update(phase='purging', plan=plan)
            install.write_json(RECOVERY / 'state.json', state)
    if state['phase'] == 'purging':
        install.finish_purge(state['plan'])
        if any(install.present(path) for path in (install.ROOT, install.CODE, install.UNITS / install.SERVICE)):
            raise ValueError('组件目录或管理单元仍未移除')
        state.update(phase='cleaned')
        install.write_json(RECOVERY / 'state.json', state)
    if state['phase'] == 'cleaned':
        finish_request(state, 'cleaned')
        state['phase'] = 'finishing'
        install.write_json(RECOVERY / 'state.json', state)
    if state['phase'] == 'finishing':
        finish_request(state, 'finished')
        state['phase'] = 'finished'
        install.write_json(RECOVERY / 'state.json', state)
    if state['phase'] == 'finished':
        cleanup_helper(state)


def local_uninstall(controllers, execute, snapshot, binding, revocation, stop):
    """Removes everything the component manages, as the panel's uninstall-all does,
    for a node whose Hub is gone or not wanted. Rerunning continues it."""
    import secrets

    if install.present(RECOVERY):
        if not install.read_json(RECOVERY / 'state.json', {}).get('local'):
            raise ValueError('后台发起的卸载尚未完成，请在后台继续')
        return  # The cleanup executor finishes it.
    record = install.read_json(install.ROOT / JOB)
    if record and record['status'] in ACTIVE and not record.get('local'):
        # Only a Hub that gave up on its uninstall lets the node finish alone.
        if revocation() is not None:
            raise ValueError('后台发起的卸载尚未完成，请在后台继续')
        record = None
    if record and record.get('local') and record['status'] in ACTIVE:
        job = {'id': record['id'], 'scope': 'all', 'attempt': record['attempt'] + (record['status'] == 'paused')}
    else:
        job = {'id': secrets.token_hex(16), 'scope': 'all', 'attempt': 1}
    job['cleanup_token'] = secrets.token_hex(32)
    lifecycle = Lifecycle(controllers, execute, snapshot, binding, local=True)
    stop()
    with install.lock(install.ROOT / 'agent.lock'):
        if record is None:
            install.remove(install.ROOT / JOB)
        # Receipts and registrations owed to a Hub mean nothing once everything goes.
        install.remove(install.ROOT / 'pending-result.json')
        install.remove(install.ROOT / 'enrollment.json')
        while (outcome := lifecycle.advance(job))['status'] == 'running':
            pass
    if outcome['status'] == 'paused':
        saved = install.read_json(install.ROOT / JOB) or {}
        raise ValueError(f"卸载暂停：{saved.get('diagnostic') or outcome.get('message')}；处理后重新运行 nodehelm uninstall")


def await_local_cleanup(timeout=120):
    """Starts the cleanup executor and waits for it to remove the component."""
    if install.present(RECOVERY):
        install.ctl('start', '--no-block', UNIT)
    deadline = time.monotonic() + timeout
    while install.present(RECOVERY) or install.present(install.CODE):
        if time.monotonic() >= deadline:
            raise ValueError(f'清理仍在进行，可查看 journalctl -u {UNIT}')
        time.sleep(1)
    print('已全部卸载')


def main():
    if sys.argv[1:] != ['finish'] or os.geteuid() != 0:
        raise ValueError('仅支持 root 清理执行器')
    os.umask(0o077)
    sys.dont_write_bytecode = True
    with install.lock(install.LIFECYCLE_LOCK), install.lock(RECOVERY / 'worker.lock'):
        state = check_worker_files(install.read_json(RECOVERY / 'state.json'))
        try:
            worker_step(state)
        except (urllib.error.URLError, urllib.error.HTTPError):
            # Completion responses may be lost: preserve phase and retry the same idempotent acknowledgement.
            raise
        except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
            if state['phase'] in ('preparing', 'purging'):
                state.update(resume_phase=state['phase'], phase='paused', diagnostic=str(error))
                install.write_json(RECOVERY / 'state.json', state)
                if not state.get('local'):
                    with contextlib.suppress(urllib.error.URLError, OSError, ValueError):
                        report_state(state, 'paused')
            raise


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError) as error:
        print(f'清理未完成：{type(error).__name__}；保留 root 私有恢复入口 {RECOVERY}/lifecycle.py finish', file=sys.stderr)
        sys.exit(1)
