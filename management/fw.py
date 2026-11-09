"""FW (Realm): TCP/UDP forwarding fields, instance config and service template."""
import ipaddress
import socket
from controller import Controller
from core_driver import CoreDriver, command
from protocol import Rejected, Spec, fields, name, port
from storage import private_directory


def resource(value):
    fields(value, {'port', 'name', 'target_host', 'target_port'})
    listen = port(value.get('port'))
    label = name(value.get('name', ''))
    try: target = str(ipaddress.IPv4Address(value.get('target_host', '')))
    except (ValueError, TypeError): raise Rejected('目标必须为有效 IPv4 地址') from None
    ip = ipaddress.ip_address(target)
    if ip.is_unspecified or ip.is_multicast or ip.is_link_local:
        raise Rejected('目标不能为未指定、组播或链路本地地址')
    target_port = port(value.get('target_port', listen), '目标端口须为 1–65535')
    return dict(port=listen, name=label, target_host=target, target_port=target_port)


SPEC = Spec('fw', frozenset({'install', 'apply', 'delete', 'check', 'core_upgrade', 'core_rollback', 'core_uninstall'}),
            resource)


class Driver(CoreDriver):
    module = SPEC.name
    title = 'FW'
    binary_name = 'realm'
    subdirectory = SPEC.name
    legacy_version = '2.9.6'

    def complete(self, item, existing):
        return item

    def config(self, item):
        return f'[network]\nuse_udp = true\n\n[[endpoints]]\nlisten = "0.0.0.0:{item["port"]}"\nremote = "{item["target_host"]}:{item["target_port"]}"\n'

    def service(self, port):
        cap = 'AmbientCapabilities=CAP_NET_BIND_SERVICE\nCapabilityBoundingSet=CAP_NET_BIND_SERVICE\n' if port < 1024 else 'CapabilityBoundingSet=\n'
        return f'''[Unit]
Description=Node Manager {self.module} {port}
After=network-online.target
Wants=network-online.target
[Service]
DynamicUser=yes
{cap}LoadCredential=config:{self.configs}/{port}.conf
ExecStart={self.binary} -c %d/config
Restart=on-failure
RestartSec=3
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
MemoryMax=256M
TasksMax=128
[Install]
WantedBy=multi-user.target
'''

    def prepare(self, action, before, after, payload):
        backup = super().prepare(action, before, after, payload)
        if action != 'apply': return backup
        item = next(r for r in after['resources'] if r['port'] == payload['port'])
        # A local self-loop is always an error, not a usable forwarding rule.
        if item['target_port'] == item['port']:
            target = item['target_host']
            local = ipaddress.IPv4Address(target).is_loopback
            if not local:
                local = target in command('hostname', '-I').stdout.decode().split()
            if local:
                raise Rejected('转发目标不能指向自身同一监听端口', 'target_loop')
        return backup

    def check_target(self, item):
        try:
            with socket.create_connection((item['target_host'], item['target_port']), timeout=5): pass
        except OSError: raise Rejected('目标 TCP 连接失败，请检查目标服务或网络', 'target_unreachable') from None


def controller(root):
    directory = private_directory(root / SPEC.name)
    private_directory(directory / 'configs')
    return Controller(directory, Driver(root), SPEC)
