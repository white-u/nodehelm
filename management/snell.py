"""Snell 6: accepted fields, instance config and service template.
Owned Snell services only. Never sources interactive scripts or changes firewall."""
import errno
import os
import pathlib
import re
import secrets
import socket
import sys


def listen_line(port, ipv6=True):
    return f'listen = 0.0.0.0:{port}' + (f',[::]:{port}' if ipv6 else '') + '\n'


def start(binary, credential, port):
    try:
        with socket.socket(socket.AF_INET6, socket.SOCK_STREAM) as probe:
            probe.bind(('::', 0))
    except OSError as error:
        if error.errno not in (errno.EAFNOSUPPORT, errno.EPROTONOSUPPORT):
            raise
        print(f'IPv6 监听不可用，使用 IPv4 监听：{error}', file=sys.stderr, flush=True)
        config = pathlib.Path(credential).read_text()
        if listen_line(port) not in config:
            sys.exit('配置缺少预期的监听地址，无法回退到 IPv4')
        config = config.replace(listen_line(port), listen_line(port, ipv6=False), 1)
        # Keep the runtime credential in memory and let the core inherit its descriptor.
        with os.fdopen(os.memfd_create('snell-config', flags=0), 'w') as fallback:
            os.fchmod(fallback.fileno(), 0o600)
            fallback.write(config)
            fallback.flush()
            os.execv(binary, [binary, '-c', f'/proc/self/fd/{fallback.fileno()}'])
    else:
        os.execv(binary, [binary, '-c', credential])


# systemd runs this file as the launcher: keep that path on the standard library.
if __name__ == '__main__':
    start(*sys.argv[1:])

from controller import Controller
from core_driver import CoreDriver
from protocol import Rejected, Spec, address, fields, name, port


def resource(value):
    fields(value, {'port', 'name', 'host', 'public_port', 'psk', 'mode'})
    listen = port(value.get('port'))
    public_port = port(value.get('public_port', listen), '访问端口须为 1–65535')
    label = name(value.get('name', ''))
    mode = value.get('mode', 'default')
    if mode not in ('default', 'unshaped'):
        raise Rejected('首版仅支持加密模式 default 和 unshaped')
    psk = value.get('psk', '')
    if not isinstance(psk, str) or (psk and not re.fullmatch(r'[A-Za-z0-9_-]{22,128}', psk)):
        raise Rejected('PSK 必须为 22–128 位字母、数字、横线或下划线；留空自动生成或保留')
    return dict(port=listen, name=label, host=address(value.get('host')), public_port=public_port, mode=mode, psk=psk)


SPEC = Spec('snell', frozenset({'install', 'apply', 'delete', 'core_upgrade', 'core_rollback', 'core_uninstall'}),
            resource)


class Driver(CoreDriver):
    module = SPEC.name
    title = 'Snell'
    binary_name = 'snell-server'
    subdirectory = ''
    base_version = '6.0.0'
    archive = 'zip'
    exact_url = True

    def complete(self, item, existing):
        item['psk'] = item['psk'] or (existing['psk'] if existing else secrets.token_urlsafe(24))
        return item

    def config(self, item):
        return ('[snell-server]\n'
                f"{listen_line(item['port'])}psk = {item['psk']}\n"
                f"ipv6 = true\ntfo = true\nmode = {item['mode']}\ndns-ip-preference = default\n")

    def service(self, item):
        port = item['port']
        capabilities = 'AmbientCapabilities=CAP_NET_BIND_SERVICE\nCapabilityBoundingSet=CAP_NET_BIND_SERVICE\n' if port < 1024 else ''
        return f'''[Unit]
Description=Node Manager Snell {port}
After=network-online.target
Wants=network-online.target
[Service]
DynamicUser=yes
{capabilities}LoadCredential=config:{self.configs}/{port}.conf
ExecStart=/usr/bin/python3 -B {self.binary.with_name('snell.py')} {self.binary} %d/config {port}
Restart=on-failure
RestartSec=3
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
[Install]
WantedBy=multi-user.target
'''


def controller(root):
    return Controller(root, Driver(root), SPEC)
