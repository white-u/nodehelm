#!/usr/bin/env python3
"""Offline, checksum-verified first install. Existing installations use hub-lifecycle.py."""
import argparse
import hashlib
import fcntl
import json
import ipaddress
import os
import pathlib
import platform
import re
import stat
import subprocess
import sys
import tempfile
import urllib.parse

from hub_health import wait_for_hub

CODE = pathlib.Path('/opt/node-manager-hub')
DATA = pathlib.Path('/var/lib/node-manager-hub')
UNIT = pathlib.Path('/etc/systemd/system/node-manager-hub.service')
PLAN = pathlib.Path('/opt/.node-manager-hub-install.json')

def validate_site(site):
    u = urllib.parse.urlsplit(site)
    if u.scheme != 'https' or not u.hostname or u.username or u.password or u.query or u.fragment or u.path not in ('', '/'):
        raise ValueError('请填写 HTTPS 根域名地址，并自行配置证书及反向代理')
    if any(c.isspace() or ord(c) < 32 or c in '%\\"$' for c in site) or not site.isascii():
        raise ValueError('域名地址包含不支持的字符；国际域名请使用 ASCII 编码')
    if '?' in site or '#' in site:raise ValueError('后台地址不能包含查询或片段')
    try:ipaddress.ip_address(u.hostname)
    except ValueError:pass
    else:raise ValueError('请使用指向服务器的 HTTPS 域名')
    if u.hostname=='localhost' or not re.fullmatch(r'(?=.{1,253}$)(?:[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?\.)+[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?',u.hostname):
        raise ValueError('请使用有效的 HTTPS 域名')
    try: u.port
    except ValueError: raise ValueError('域名端口无效') from None
    return site.rstrip('/')

def unit_text(site, port):
    site = validate_site(site)
    if type(port) is not int or not 1024 <= port <= 65535:
        raise ValueError('后台本地端口须为 1024–65535')
    return f"""[Unit]
Description=Node Manager independent hub
After=network-online.target
Wants=network-online.target
[Service]
DynamicUser=yes
StateDirectory=node-manager-hub
StateDirectoryMode=0700
WorkingDirectory=/var/lib/node-manager-hub
ExecStart=/opt/node-manager-hub/monitor-hub --listen 127.0.0.1:{port} --db /var/lib/node-manager-hub/hub.db --themes /var/lib/node-manager-hub/themes --site {site}
Restart=on-failure
RestartSec=5
UMask=0077
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
MemoryMax=512M
TasksMax=128
[Install]
WantedBy=multi-user.target
"""

def verify_binary(source, digest):
    if not re.fullmatch('[0-9a-f]{64}', digest): raise ValueError('须提供可信构建产物的 SHA256')
    if source.is_symlink() or not source.is_file(): raise ValueError('程序须为本地普通文件')
    data=source.read_bytes()
    if not data.startswith(b'\x7fELF') or hashlib.sha256(data).hexdigest()!=digest:
        raise ValueError('不是 Linux 程序或 SHA256 不匹配，未安装')
    machine = platform.machine().lower()
    expected = {'x86_64': 62, 'amd64': 62, 'aarch64': 183, 'arm64': 183}.get(machine)
    if expected is not None and (len(data) < 64 or data[4:6] != b'\x02\x01'
                                 or int.from_bytes(data[18:20], 'little') != expected):
        raise ValueError(f'程序架构与本机 {machine} 不匹配，未安装')
    return data

def atomic(path, data, mode):
    if path.parent == CODE:
        tmp=str(path.parent/('.install-'+path.name))
        fd=os.open(tmp,os.O_CREAT|os.O_EXCL|os.O_WRONLY|os.O_NOFOLLOW,0o600)
    else:
        fd,tmp=tempfile.mkstemp(prefix='.install-',dir=path.parent)
    try:
        with os.fdopen(fd,'wb') as out:
            out.write(data);out.flush();os.fchmod(out.fileno(),mode);os.fsync(out.fileno())
        os.replace(tmp,path)
        descriptor=os.open(path.parent,os.O_RDONLY)
        try:os.fsync(descriptor)
        finally:os.close(descriptor)
    finally:
        if os.path.exists(tmp):os.unlink(tmp)

def check_unit(resume):
    result=subprocess.run(['systemctl','show',UNIT.name,'-p','LoadState','-p','FragmentPath','-p','DropInPaths','-p','ActiveState','-p','MainPID'],check=True,capture_output=True,text=True,timeout=15)
    properties=dict(line.split('=',1) for line in result.stdout.splitlines() if '=' in line)
    fragment=properties.get('FragmentPath','')
    if properties.get('DropInPaths') or (fragment and (not resume or fragment!=str(UNIT))):
        raise ValueError('发现已有同名服务或额外覆盖配置，拒绝接管')
    if not fragment and properties.get('LoadState') not in ('not-found','loaded'):
        raise ValueError('无法确认服务归属')
    return properties


def prepare_install(digest, unit, resume, reuse=False, owner=0):
    expected={'sha256':digest,'unit':unit}
    if PLAN.is_symlink():raise ValueError('安装恢复记录不能是符号链接')
    if resume:
        if not PLAN.is_file() or PLAN.stat().st_uid != owner or PLAN.stat().st_mode & 0o077:
            raise ValueError('缺少可信安装恢复记录，拒绝接管现有路径')
        if json.loads(PLAN.read_text()) != expected:
            raise ValueError('恢复须使用原程序摘要、域名和端口')
        if CODE.is_symlink() or (CODE.exists() and (not CODE.is_dir() or CODE.stat().st_uid != owner or CODE.stat().st_mode & 0o022)):
            raise ValueError('程序目录归属或权限已变化')
        if CODE.exists():
            if set(path.name for path in CODE.iterdir()) - {'monitor-hub','.node-manager-install','.install-monitor-hub','.install-.node-manager-install'}:
                raise ValueError('程序目录含未确认文件，请保留现场核实')
            for path,content in ((CODE/'.node-manager-install',b'node-manager-hub-v1\n'),(UNIT,unit.encode())):
                if path.is_symlink() or (path.exists() and path.read_bytes()!=content):
                    raise ValueError('已有安装文件与记录不一致')
            for name in ('.install-monitor-hub','.install-.node-manager-install'):
                stale=CODE/name
                if stale.is_symlink() or (stale.exists() and (not stale.is_file() or stale.stat().st_uid!=owner or stale.stat().st_mode & 0o022)):
                    raise ValueError('中断临时文件归属异常，保留现场')
                stale.unlink(missing_ok=True)
            binary=CODE/'monitor-hub'
            if binary.exists() or binary.is_symlink():
                verify_binary(binary,digest)
                meta=binary.lstat()
                if meta.st_uid!=owner or stat.S_IMODE(meta.st_mode)!=0o755 or meta.st_nlink!=1:
                    raise ValueError('程序归属或权限与原安装不符')
        elif UNIT.exists() or UNIT.is_symlink() or DATA.exists() or DATA.is_symlink():
            raise ValueError('缺少程序目录但已有服务/数据，拒绝接管')
        if DATA.is_symlink():
            if DATA.readlink()!=pathlib.Path('private/node-manager-hub') and DATA.readlink()!=pathlib.Path('/var/lib/private/node-manager-hub'):
                raise ValueError('数据目录链接不是 systemd 独立私有目录')
        elif DATA.exists() and (not DATA.is_dir() or DATA.stat().st_mode & 0o077):
            raise ValueError('数据目录权限或归属异常')
    else:
        if reuse:
            marker=CODE/'.node-manager-install'
            if CODE.is_symlink() or not CODE.is_dir() or CODE.stat().st_uid!=owner or CODE.stat().st_mode & 0o022 or set(p.name for p in CODE.iterdir())!={'.node-manager-install'}:
                raise ValueError('复用只支持本项目正常卸载后保留的目录')
            if marker.is_symlink() or not marker.is_file() or marker.stat().st_uid!=owner or marker.stat().st_mode & 0o022 or marker.read_bytes()!=b'node-manager-hub-v1\n':
                raise ValueError('缺少可信卸载归属标记')
            if DATA.is_symlink():
                if DATA.readlink() not in (pathlib.Path('private/node-manager-hub'),pathlib.Path('/var/lib/private/node-manager-hub')):
                    raise ValueError('数据目录不是本项目 systemd 私有目录')
            elif DATA.exists() and (not DATA.is_dir() or DATA.stat().st_mode & 0o077):
                raise ValueError('数据目录权限不安全')
        for path in ((UNIT,PLAN) if reuse else (CODE,DATA,UNIT,PLAN)):
            if path.exists() or path.is_symlink():
                raise ValueError(f'{path} 已存在；失败安装可核实后用相同参数加 --resume，升级请用 hub-lifecycle.py')
        # Durable intent precedes the first directory or service side effect.
        atomic(PLAN,json.dumps(expected).encode(),0o600)
    CODE.mkdir(mode=0o755,exist_ok=True)
    CODE.chmod(0o755)
    atomic(CODE/'.node-manager-install',b'node-manager-hub-v1\n',0o644)


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary',required=True,type=pathlib.Path)
    p.add_argument('--sha256',required=True)
    p.add_argument('--site',required=True)
    p.add_argument('--port',type=int,default=9911)
    p.add_argument('--resume',action='store_true',help='仅恢复有本机持久记录且参数完全匹配的首次安装')
    p.add_argument('--reuse-data',action='store_true',help='明确复用本项目正常卸载后保留的数据和主题')
    args=p.parse_args()
    if args.reuse_data and args.resume:p.error('--reuse-data 与 --resume 不能同时使用')
    unit=unit_text(args.site,args.port)
    data=verify_binary(args.binary,args.sha256)
    if os.geteuid()!=0 or not pathlib.Path('/run/systemd/system').is_dir():
        raise ValueError('安装需要 Linux systemd 和 root')
    descriptor=os.open('/run/node-manager-hub-install.lock',os.O_RDWR|os.O_CREAT|os.O_NOFOLLOW,0o600)
    install_lock=os.fdopen(descriptor,'w')
    try:fcntl.flock(install_lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
    except BlockingIOError:raise ValueError('已有首次安装进程正在运行') from None
    if pathlib.Path('/var/lib/node-manager-hub-lifecycle/node-manager-hub.service/recovery.json').exists():
        raise ValueError('后台有未完成的升级或卸载，请先运行 hub-lifecycle.py recover')
    for parent in (CODE.parent,DATA.parent,UNIT.parent):
        if parent.is_symlink() or parent.stat().st_uid!=0 or parent.stat().st_mode & 0o022:
            raise ValueError('安装父目录须为 root 所有且不可由其他用户写入')
    if not args.resume:
        import socket
        with socket.socket() as sock:
            sock.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
            try:sock.bind(('127.0.0.1',args.port))
            except OSError:raise ValueError('后台端口已占用，请选择另一个端口') from None
    install_hub(args, unit, data)


def install_hub(args, unit, data):
    check_unit(args.resume)
    prepare_install(args.sha256, unit, args.resume, args.reuse_data, owner=os.geteuid())
    binary = CODE / 'monitor-hub'
    # An interrupted installation may already be serving from this verified inode.
    if not args.resume or not binary.exists():
        atomic(binary, data, 0o755)
    atomic(UNIT, unit.encode(), 0o644)
    subprocess.run(['systemctl', 'daemon-reload'], check=True, timeout=30)
    service = check_unit(True)
    if args.resume and service.get('ActiveState') in ('failed', 'activating'):
        subprocess.run(['systemctl', 'reset-failed', UNIT.name], check=True, timeout=30)
    subprocess.run(['systemctl', 'enable', '--now', UNIT.name], check=True, timeout=30)
    try:
        wait_for_hub(lambda: check_unit(True), binary, f'http://127.0.0.1:{args.port}/api/me',
                     '127.0.0.1', args.port)
    except ValueError as error:
        raise ValueError(f'{error}；保留安装记录，请查看 journalctl -u node-manager-hub 并按恢复说明处理') from None
    PLAN.unlink()
    directory = os.open(PLAN.parent, os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)
    print(f'后台已启动，仅监听 127.0.0.1:{args.port}。请将 {args.site} 的 HTTPS 反代指向该地址。')
    print('应急密码请在本机 journalctl -u node-manager-hub 查看，勿公开日志。原 monitor 和 agent 未修改。')

if __name__=='__main__':
    try:main()
    except (ValueError,OSError,subprocess.SubprocessError) as error:
        print('安装未完成：'+str(error),file=sys.stderr);sys.exit(1)
