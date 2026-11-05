"""Check that the expected systemd Hub process owns and answers its loopback socket."""
import http.client
import json
import os
import pathlib
import re
import socket
import sys
import time
import urllib.request

PROC = pathlib.Path('/proc')


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args):
        return None


def owns_listener(proc, host, port):
    ipv6 = ':' in host
    address = socket.inet_pton(socket.AF_INET6 if ipv6 else socket.AF_INET, host)
    encoded = ''.join(f'{int.from_bytes(address[i:i+4], sys.byteorder):08X}'
                      for i in range(0, len(address), 4))
    target = f'{encoded}:{port:04X}'
    table = proc / 'net' / ('tcp6' if ipv6 else 'tcp')
    sockets = {f'socket:[{fields[9]}]' for row in table.read_text().splitlines()[1:]
               if (fields := row.split())[1] == target and fields[3] == '0A'}
    for descriptor in (proc / 'fd').iterdir():
        try:
            if os.readlink(descriptor) in sockets:
                return True
        except FileNotFoundError:
            continue
    return False


def wait_for_hub(unit_info, binary, url, host, port):
    pid = unit_info().get('MainPID', '')
    if not re.fullmatch(r'[1-9][0-9]*', pid):
        raise ValueError('后台进程启动失败')
    proc = PROC / pid
    end = time.monotonic() + 30
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    while True:
        info = unit_info()
        if info.get('MainPID') != pid:
            raise ValueError('后台进程启动失败或发生了重启')
        try:
            # Type=simple may return before the service has exec'd or bound.
            if (info.get('ActiveState') == 'active' and (proc / 'exe').samefile(binary)
                    and owns_listener(proc, host, port)):
                with opener.open(url, timeout=3) as response:
                    body = response.read(65537) if response.url == url and response.status == 200 else b''
                    payload = json.loads(body) if 0 < len(body) <= 65536 else None
                    healthy = isinstance(payload, dict) and type(payload.get('authed')) is bool
                if healthy:
                    info = unit_info()
                    if (info.get('MainPID') == pid and info.get('ActiveState') == 'active'
                            and (proc / 'exe').samefile(binary) and owns_listener(proc, host, port)):
                        return
        except (OSError, ValueError, http.client.HTTPException):
            pass
        if time.monotonic() >= end:
            raise ValueError('后台健康检查超时')
        time.sleep(1)
