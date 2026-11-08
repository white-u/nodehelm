"""Snell 6: accepted fields, instance config and service template.
Owned Snell services only. Never sources interactive scripts or changes firewall."""
import re
import secrets
from controller import Controller
from core_driver import CoreDriver
from protocol import Rejected, address, fields, name, port

MODULE = 'snell'
ACTIONS = frozenset({'install', 'apply', 'delete', 'core_upgrade', 'core_rollback', 'core_uninstall'})


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


class Driver(CoreDriver):
    module = MODULE
    title = 'Snell'
    binary_name = 'snell-server'
    subdirectory = ''
    legacy_version = '6.0.0'
    archive = 'zip'
    exact_url = True

    def complete(self, item, existing):
        item['psk'] = item['psk'] or (existing['psk'] if existing else secrets.token_urlsafe(24))
        return item

    def config(self, item):
        return ('[snell-server]\n'
                f"listen = 0.0.0.0:{item['port']}\npsk = {item['psk']}\n"
                f"ipv6 = true\ntfo = true\nmode = {item['mode']}\ndns-ip-preference = default\n")

    def service(self, port):
        capabilities = 'AmbientCapabilities=CAP_NET_BIND_SERVICE\nCapabilityBoundingSet=CAP_NET_BIND_SERVICE\n' if port < 1024 else ''
        return f'''[Unit]
Description=Node Manager Snell {port}
After=network-online.target
Wants=network-online.target
[Service]
DynamicUser=yes
{capabilities}LoadCredential=config:{self.configs}/{port}.conf
ExecStart={self.binary} -c %d/config
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
    return Controller(root, Driver(root), MODULE)
