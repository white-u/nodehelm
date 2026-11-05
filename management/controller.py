"""Serialized state machine; backend restarts never imply replay of an operation."""
import fcntl
import hashlib
import hmac
import secrets
from model import CORE_ACTIONS, Rejected, canonical, request, revision
from storage import private_directory, read, write, remove


class Controller:
    def __init__(self, directory, driver, module="snell"):
        self.module = module
        self.root = private_directory(directory)
        self.driver = driver
        self.state = self.root / 'state.json'
        self.journal = self.root / 'recovery.json'
        self.receipts = private_directory(self.root / 'receipts')
        self.key = read(self.root / 'dedup-key.json')
        if self.key is None:
            if any(self.receipts.iterdir()) or self.journal.exists():
                raise Rejected('去重密钥丢失，禁止自动重新执行')
            self.key = secrets.token_hex(32)
            write(self.root / 'dedup-key.json', self.key)

    def snapshot(self):
        state = read(self.state, {'installed': False, 'resources': []})
        extra = {}
        if hasattr(self.driver, 'core_status'):
            extra['core'], core_valid = self.driver.core_status(state)
            consistent = self.driver.consistent(state, core_valid=core_valid)
        else:
            consistent = self.driver.consistent(state)
        return {**state, **extra, 'revision': revision(state),
                'blocked': self.journal.exists() or not consistent,
                'runtime': self.driver.runtime(state)}

    def refresh(self):
        """Make files match confirmed state under the code now installed. A component
        upgrade calls this: state is the source of truth, so repeating it is safe."""
        with (self.root / 'write.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            self.driver.refresh(read(self.state, {'installed': False, 'resources': []}))

    def execute(self, raw, *, local=False):
        req = request(raw)
        if req.get('module', 'snell') != self.module:
            raise Rejected('模块与本地执行器不匹配')
        with (self.root / 'write.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            digest = hmac.new(bytes.fromhex(self.key), canonical(req).encode(), hashlib.sha256).hexdigest()
            receipt = self.receipts / (req['id'] + '.json')
            old = read(receipt)
            if old:
                if not hmac.compare_digest(old['digest'], digest):
                    raise Rejected('相同操作编号的内容不同')
                return old['result']
            if self.journal.exists():
                journal = read(self.journal)
                if journal['id'] == req['id'] and hmac.compare_digest(journal['digest'], digest):
                    return {'id': req['id'], 'status': 'recovery_failed', 'message': '有未核实操作，请在 VPS 执行 recover'}
                raise Rejected('有未核实操作，请先在 VPS 执行 recover')
            before = read(self.state, {'installed': False, 'resources': []})
            if revision(before) != req['revision']:
                raise Rejected('配置已变化，请刷新后重新编辑', 'revision_changed')
            after = {**before, 'resources': [dict(r) for r in before['resources']]}
            action, payload = req['action'], req['payload']
            if action == 'check':
                existing = next((r for r in before['resources'] if r['port'] == payload['port']), None)
                if existing is None: raise Rejected('规则不存在')
                self.driver.check_target(existing)
                result = {'id':req['id'], 'status':'checked', 'message':'目标 TCP 可达；UDP 和公网入口需单独验证', 'revision':revision(before)}
                write(receipt, {'digest':digest, 'result':result})
                return result
            if hasattr(self.driver, 'prepare_core') and (action == 'install' or action in CORE_ACTIONS):
                after, backup = self.driver.prepare_core(action, before, payload)
                with self.driver.stage_core(after, backup) as candidate:
                    return self._apply(req, digest, receipt, before, after, backup, candidate, local=local)
            if action == 'install':
                if before['installed']:
                    raise Rejected('核心已安装，请选择升级操作')
                after['installed'] = True
            else:
                if not before['installed']:
                    raise Rejected('请先安装所选模块核心')
                if action == 'core_uninstall':
                    if self.module != 'pm':
                        raise Rejected('此模块不支持核心卸载', 'core_version')
                    if before['resources']:
                        raise Rejected('请先删除所有受管 PM 规则', 'core_in_use')
                    after['installed'] = False
                elif action in CORE_ACTIONS:
                    raise Rejected('此模块不支持核心版本切换', 'core_version')
                if hasattr(self.driver, 'require_core'):
                    self.driver.require_core(before)
                existing = next((r for r in before['resources'] if r['port'] == payload.get('port')), None)
                if req.get('intent') == 'create' and existing is not None:
                    raise Rejected('该端口已有实例，请换用端口或编辑现有实例', 'instance_exists')
                if (action in ('delete', 'reset') or req.get('intent') == 'update') and existing is None:
                    raise Rejected('实例不存在，请刷新配置', 'instance_missing')
                if action in ('apply', 'delete'):
                    after['resources'] = [r for r in after['resources'] if r['port'] != payload['port']]
                if action == 'apply':
                    item = dict(payload)
                    if self.module == 'snell':
                        item['psk'] = item['psk'] or (existing['psk'] if existing else secrets.token_urlsafe(24))
                    else:
                        item = self.driver.complete(item, existing)
                    if len(after['resources']) >= 64:
                        raise Rejected('每台 VPS 最多支持 64 个实例')
                    after['resources'].append(item)
                    after['resources'].sort(key=lambda r: r['port'])
            # Driver inspection must not mutate anything.
            backup = self.driver.prepare(action, before, after, payload)
            return self._apply(req, digest, receipt, before, after, backup, local=local)

    def _apply(self, req, digest, receipt, before, after, backup, candidate=None, *, local=False):
        journal = {'id': req['id'], 'digest': digest, 'before': before, 'after': after,
                   'backup': backup, 'phase': 'prepared'}
        if local:
            journal['local'] = True
        write(self.journal, journal)
        try:
            if backup.get('kind') == 'core':
                def begin_mutation():
                    # Publish the intent before the first stop or binary switch.
                    # A failed write must leave the in-memory no-change marker.
                    marked = {**journal, 'backup': {**backup, 'mutation_started': True}}
                    write(self.journal, marked)
                    backup['mutation_started'] = True

                self.driver.apply_core(before, after, backup, candidate, begin_mutation)
                self.driver.verify_core(after, backup)
            else:
                self.driver.apply(req['action'], before, after, req['payload'])
                self.driver.verify(after, req['payload'])
            write(self.state, after)
        except Exception as error:
            diagnostic = str(error) if isinstance(error, Rejected) else type(error).__name__
            write(self.root / 'last-error.json', {'id': req['id'], 'stage': 'apply', 'reason': diagnostic})
            return self._restore(journal, receipt, error.code if isinstance(error, Rejected) else None)
        # A failed commit-marker write can leave either durable phase. Recover
        # according to the persisted marker rather than guessing here.
        journal['phase'] = 'committed'
        write(self.journal, journal)
        return self._commit(journal, receipt, '已应用；公网连通性需另行验证')

    def _commit(self, journal, receipt, message):
        if journal.get('backup', {}).get('kind') == 'core':
            self.driver.clean_core(journal['backup'])
        result = {'id': journal['id'], 'status': 'succeeded', 'message': message,
                  'revision': revision(journal['after'])}
        write(receipt, {'digest': journal['digest'], 'result': result})
        remove(self.journal)
        return result

    def _restore(self, journal, receipt, code=None):
        try:
            backup = journal['backup']
            if backup.get('kind') == 'core':
                self.driver.restore_core(backup, journal['before'])
                self.driver.clean_core(backup)
            else:
                self.driver.restore(backup)
            write(self.state, journal['before'])
        except Exception as error:
            diagnostic = str(error) if isinstance(error, Rejected) else type(error).__name__
            write(self.root / 'last-error.json', {'id': journal['id'], 'stage': 'restore', 'reason': diagnostic})
            return {'id': journal['id'], 'status': 'recovery_failed',
                    'message': '恢复未完成，已暂停写入。请在 VPS 检查服务并执行 recover'}
        message = '应用失败，已恢复原配置及服务状态'
        if backup.get('kind') == 'core' and backup.get('mutation_started') is False:
            message = '核心切换前已中止，保留当前配置和运行状态'
        result = {'id': journal['id'], 'status': 'rolled_back', 'message': message,
                  'revision': revision(journal['before'])}
        if code is not None:
            result['code'] = code
        write(receipt, {'digest': journal['digest'], 'result': result})
        remove(self.journal)
        return result

    def recover(self):
        with (self.root / 'write.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            journal = read(self.journal)
            if journal is None:
                return {'status': 'idle', 'message': '没有待恢复操作'}
            receipt = self.receipts / (journal['id'] + '.json')
            saved = read(receipt)
            if saved and hmac.compare_digest(saved['digest'], journal['digest']):
                # Finalize a crash between persisting the verified result and deleting the journal.
                remove(self.journal)
                return saved['result']
            if journal['phase'] == 'committed':
                # The successful state was durable before this marker. Verify before confirming.
                try:
                    if journal.get('backup', {}).get('kind') == 'core':
                        self.driver.verify_core(journal['after'], journal['backup'])
                    elif self.module == 'pm':
                        self.driver.recover_committed(journal['after'])
                    else:
                        # Resource backups already record the affected port, including in older journals.
                        port = journal.get('backup', {}).get('port')
                        self.driver.verify(journal['after'], {'port': port} if port is not None else {})
                except Exception:
                    return {'id': journal['id'], 'status': 'recovery_failed', 'message': '已提交状态核实失败，请检查服务'}
                return self._commit(journal, receipt, '已核实此前应用成功')
            return self._restore(journal, receipt)
