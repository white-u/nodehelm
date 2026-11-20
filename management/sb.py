"""SB (sing-box): Reality, VLESS TLS and SS2022 fields, instance config and service template."""
import base64
import hashlib
import ipaddress
import json
import pathlib
import secrets
import socket
import ssl
import sys
import tempfile
import time
import uuid
from controller import Controller
from core_driver import CoreDriver, binary_hash, command
from protocol import Rejected, Spec, address, fields, name, port
from storage import private_directory

# VLESS TLS instances share /home/ssl/fullchain.pem and privkey.pem; systemd hands
# copies to each sandboxed instance, which restarts when the originals change.
CERTIFICATES = pathlib.Path('/home/ssl')
CREDENTIALS = pathlib.Path('/run/credentials')


def certificate_files():
    return CERTIFICATES / 'fullchain.pem', CERTIFICATES / 'privkey.pem'


def resource(value):
    fields(value, {'port', 'name', 'host', 'public_port', 'protocol', 'sni', 'psk'})
    listen = port(value.get('port'))
    label = name(value.get('name', ''))
    protocol = value.get('protocol', 'reality')
    if protocol not in ('reality', 'tls', 'ss2022'):
        raise Rejected('SB 仅支持 Reality、VLESS TLS 和 SS2022')
    public_port = port(value.get('public_port', listen), '访问端口须为 1–65535')
    psk = value.get('psk', '')
    if not isinstance(psk, str): raise Rejected('密钥格式错误')
    if psk:
        try:
            if protocol != 'ss2022' or len(base64.b64decode(psk, validate=True)) != 16:
                raise ValueError()
        except ValueError: raise Rejected('SS2022 密钥须为 16 字节的标准 Base64；留空生成或保留') from None
    sni = value.get('sni', '')
    if protocol in ('reality', 'tls'):
        sni = address(sni)
        if '.' not in sni or ':' in sni or sni.replace('.', '').isdigit():
            raise Rejected('Reality 目标须为完整域名' if protocol == 'reality' else '证书域名须为完整域名')
    else: sni = ''
    return dict(port=listen, name=label, host=address(value.get('host')), public_port=public_port, protocol=protocol, sni=sni, psk=psk)


SPEC = Spec('sb', frozenset({'install', 'apply', 'delete', 'core_upgrade', 'core_rollback', 'core_uninstall'}),
            resource)


class Driver(CoreDriver):
    module = SPEC.name
    title = 'SB'
    binary_name = 'sing-box'
    subdirectory = SPEC.name
    base_version = '1.14.4'
    version_arg = 'version'

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        # Per port, the certificate digests last refused, so each renewal is reported once.
        self.skipped = {}

    def complete(self, item, existing):
        if existing and item['protocol'] != existing['protocol']:
            raise Rejected('不能原地切换协议，请新建实例后删除旧实例')
        if item['protocol'] == 'ss2022':
            item['psk'] = item['psk'] or (existing['psk'] if existing else base64.b64encode(secrets.token_bytes(16)).decode())
        elif item['protocol'] == 'tls':
            item['uuid'] = existing['uuid'] if existing else str(uuid.uuid4())
        else:
            if existing:
                for key in ('uuid', 'private_key', 'public_key', 'short_id'): item[key] = existing[key]
            else:
                if binary_hash(self.binary) not in {self.asset(v)['sha256'] for v in self.versions()}:
                    raise Rejected('SB 核心校验失败')
                output = command(str(self.binary), 'generate', 'reality-keypair').stdout.decode()
                keys = dict(line.strip().split(':', 1) for line in output.splitlines() if ':' in line)
                try:
                    private = keys['PrivateKey'].strip(); public = keys['PublicKey'].strip()
                    for key in (private, public):
                        if len(base64.urlsafe_b64decode(key + '=')) != 32: raise ValueError()
                except (ValueError, KeyError): raise Rejected('Reality 密钥生成失败') from None
                item.update(uuid=str(uuid.uuid4()), private_key=private, public_key=public, short_id=secrets.token_hex(4))
        return item

    def config(self, item, certificate=None):
        """`certificate` replaces the credential copies with other paths, for `sing-box check`."""
        inbound = {'listen':'::', 'listen_port':item['port']}
        if item['protocol'] == 'ss2022':
            inbound.update(type='shadowsocks', method='2022-blake3-aes-128-gcm', password=item['psk'])
        elif item['protocol'] == 'tls':
            loaded = CREDENTIALS / self.unit(item['port'])
            cert, key = certificate or (loaded / 'cert', loaded / 'key')
            inbound.update(type='vless', users=[{'uuid':item['uuid'],'flow':'xtls-rprx-vision'}], tls={
                'enabled':True, 'server_name':item['sni'], 'certificate_path':str(cert), 'key_path':str(key)})
        else:
            inbound.update(type='vless', users=[{'uuid':item['uuid'],'flow':'xtls-rprx-vision'}], tls={
                'enabled':True, 'server_name':item['sni'], 'reality':{'enabled':True,
                'handshake':{'server':item['sni'],'server_port':443}, 'private_key':item['private_key'], 'short_id':[item['short_id']]}})
        return json.dumps({'log':{'level':'warn'}, 'dns':{'servers':[{'type':'local','tag':'local'}]},
            'inbounds':[inbound], 'outbounds':[{'type':'direct','tag':'direct'}],
            'route':{'default_domain_resolver':'local','rules':[{'ip_is_private':True,'action':'reject'}]}}, sort_keys=True) + '\n'

    def service(self, item):
        port = item['port']
        cap = 'AmbientCapabilities=CAP_NET_BIND_SERVICE\nCapabilityBoundingSet=CAP_NET_BIND_SERVICE\n' if port < 1024 else 'CapabilityBoundingSet=\n'
        if item['protocol'] == 'tls':
            cert, key = certificate_files()
            cap += f'LoadCredential=cert:{cert}\nLoadCredential=key:{key}\n'
        return f'''[Unit]
Description=Node Manager {self.module} {port}
After=network-online.target
Wants=network-online.target
[Service]
DynamicUser=yes
{cap}LoadCredential=config:{self.configs}/{port}.conf
ExecStart={self.binary} run -c %d/config
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
        previous = next((r for r in before['resources'] if r['port'] == item['port']), None)
        if item['protocol'] == 'reality' and (not previous or previous['sni'] != item['sni']):
            self.check_reality(item['sni'])
        if item['protocol'] == 'tls': self.check_certificate(item['sni'])
        self.check_config(item)
        return backup

    def check_config(self, item, executable=None):
        # The check reads the certificate originals: credential copies exist only while running.
        tls = item['protocol'] == 'tls'
        with tempfile.NamedTemporaryFile(dir=self.configs, suffix='.json') as candidate:
            candidate.write(self.config(item, certificate_files() if tls else None).encode()); candidate.flush()
            result = command(str(executable or self.binary), 'check', '-c', candidate.name, check=False)
        if result.returncode and tls and executable is None:
            raise Rejected('证书与私钥不匹配或格式无效，原实例未修改', 'cert_invalid')
        if result.returncode:
            raise Rejected('SB 配置未通过核心校验，原实例未修改' if executable is None
                           else 'SB 实例配置未通过所选核心校验', 'invalid_config')

    @staticmethod
    def check_reality(host):
        try:
            answers = socket.getaddrinfo(host, 443, type=socket.SOCK_STREAM)
        except OSError:
            raise Rejected('Reality 目标域名无法解析，请检查域名与 VPS DNS', 'target_dns') from None
        if not answers or any(not ipaddress.ip_address(a[4][0]).is_global for a in answers):
            raise Rejected('Reality 目标必须解析到公网地址', 'target_dns')
        context = ssl.create_default_context()
        context.minimum_version = ssl.TLSVersion.TLSv1_3
        context.set_alpn_protocols(['h2'])
        try:
            # Connect to the already-validated address, retaining hostname verification.
            with socket.create_connection(answers[0][4][:2], timeout=8) as sock:
                with context.wrap_socket(sock, server_hostname=host) as tls:
                    if tls.selected_alpn_protocol() != 'h2': raise Rejected('Reality 目标不支持 HTTP/2', 'target_h2')
        except OSError: raise Rejected('Reality 目标 TLS 1.3 验证失败，请更换目标或检查网络', 'target_tls') from None

    @staticmethod
    def check_certificate(domain):
        cert, key = certificate_files()
        for path in (cert, key):
            if not path.is_file():
                raise Rejected(f'未找到证书文件 {path}', 'cert_missing')
        # A private CPython helper, kept everywhere since 3.x; the core check still
        # covers the pair when an interpreter lacks it.
        decode = getattr(getattr(ssl, '_ssl', None), '_test_decode_cert', None)
        if decode is None: return
        try:
            info = decode(str(cert))
            expired = ssl.cert_time_to_seconds(info['notAfter']) <= time.time()
        except (ssl.SSLError, OSError, KeyError, ValueError):
            raise Rejected(f'证书 {cert} 不是有效的 PEM 证书', 'cert_invalid') from None
        if expired: raise Rejected(f'证书 {cert} 已过期', 'cert_expired')
        names = [v.lower() for k, v in info.get('subjectAltName', ()) if k == 'DNS']
        if not any(n == domain or (n.startswith('*.') and n[2:] == domain.split('.', 1)[1]) for n in names):
            raise Rejected(f'证书 {cert} 不包含域名 {domain}', 'cert_domain')

    def upkeep(self, state):
        """Restart running TLS instances whose certificate files differ from the copies they loaded."""
        running = [r for r in state['resources'] if r['protocol'] == 'tls' and self.running(r['port'])]
        # Hashing the core is only worth it when there is something to restart.
        if not running or not self.consistent(state): return
        for item in running:
            port = item['port']
            loaded = CREDENTIALS / self.unit(port)
            source = None
            try:
                source = [file_digest(path) for path in certificate_files()]
                if source == [file_digest(loaded / 'cert'), file_digest(loaded / 'key')] or self.skipped.get(port) == source:
                    continue
                self.check_certificate(item['sni'])
                self.check_config(item)
            except (OSError, Rejected) as error:
                # Keep serving with the loaded certificate; a renewal may still be writing.
                if port not in self.skipped or self.skipped[port] != source:
                    print(f'SB {port} 证书已变化但未通过校验，继续使用已加载的证书：{error}', file=sys.stderr, flush=True)
                self.skipped[port] = source
                continue
            self.skipped.pop(port, None)
            command('systemctl', 'restart', self.unit(port))
            print(f'SB {port} 证书已更新，实例已重启', file=sys.stderr, flush=True)

    def check_core_candidate(self, state, executable):
        super().check_core_candidate(state, executable)
        for item in state['resources']:
            self.check_config(item, executable)


def file_digest(path):
    return hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest()


def controller(root):
    directory = private_directory(root / SPEC.name)
    private_directory(directory / 'configs')
    return Controller(directory, Driver(root), SPEC)
