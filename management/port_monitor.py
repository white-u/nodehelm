"""Read-only PM adapter. Never sources shell state, changes nft/tc, or resets counters."""
import contextlib
import fcntl
import json
import pathlib
import re


def data_file(path, limit=1024*1024):
    if path.is_symlink() or not path.is_file() or path.stat().st_size > limit:
        raise ValueError('invalid file')
    return path.read_text()


def snapshot(root=pathlib.Path('/etc/port_monitor'), lock=pathlib.Path('/var/run/pm_cron.lock')):
    result = {'status':'not_installed','rows':[], 'read_only':True}
    if not root.exists(): return result
    try:
        if root.is_symlink() or (root/'state').is_symlink(): raise ValueError('invalid directory')
        with contextlib.ExitStack() as stack:
            if lock.exists():
                if lock.is_symlink(): raise ValueError('invalid lock')
                handle = stack.enter_context(lock.open('r'))
                try: fcntl.flock(handle, fcntl.LOCK_SH | fcntl.LOCK_NB)
                except BlockingIOError: return {**result,'status':'busy'}
            config = json.loads(data_file(root/'config.json'))
            if config.get('config_version') != 6 or not isinstance(config.get('ports'), dict) or len(config['ports'])>64:
                raise ValueError('unsupported config')
            for port, item in config['ports'].items():
                if not re.fullmatch(r'[1-9][0-9]{0,4}',port) or not 1<=int(port)<=65535: raise ValueError('invalid port')
                quota=item.get('quota_gb');mode=item.get('quota_mode','in_out');day=item.get('reset_day',0)
                if type(quota) is not int or not 1<=quota<=8589934591 or mode not in ('in_out','out_only') or type(day) is not int or not 0<=day<=31:
                    raise ValueError('invalid policy')
                row={'port':int(port),'quota_gb':quota,'quota_mode':mode,'reset_day':day,
                     'comment':str(item.get('comment',''))[:128],'expiry_date':str(item.get('expiry_date',''))[:10],
                     'burst_enabled':item.get('dyn_limit',{}).get('enable') is True,'state':'unknown'}
                try:
                    values={}
                    for line in data_file(root/'state'/(port+'.txt'),65536).splitlines():
                        key,sep,value=line.partition('=')
                        if key in values: raise ValueError('duplicate key')
                        if sep:values[key]=value
                    if values.get('s_state_version')!='2':raise ValueError('unsupported state')
                    for key in ('s_acc_in','s_acc_out','s_last_k_in','s_last_k_out','s_last_reset_ts','s_last_sample_ts'):
                        if not re.fullmatch(r'0|[1-9][0-9]{0,18}',values.get(key,'')) or int(values[key])>9223372036854775807:
                            raise ValueError('invalid counters')
                    row.update(state='ready',rx=int(values['s_acc_in']),tx=int(values['s_acc_out']),last_sample=int(values['s_last_sample_ts']))
                except (OSError,ValueError):pass  # Unknown is explicit; no fabricated zero counters.
                result['rows'].append(row)
            result['rows'].sort(key=lambda r:r['port'])
            return {**result,'status':'ready'}
    except (OSError,ValueError,TypeError,AttributeError):
        return {'status':'unavailable','rows':[],'read_only':True}
