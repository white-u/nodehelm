#!/usr/bin/env python3
"""Opt-in Linux/root acceptance using isolated names/paths; leaves existing services untouched."""
import base64
import json
import os
import pathlib
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import uuid
sys.path.insert(0,str(pathlib.Path(__file__).resolve().parents[1]))
from controller import Controller
from model import Rejected
from services import Driver
from snell import Driver as SnellDriver, command
from storage import private_directory


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1',0)); return sock.getsockname()[1]

def wait_client(process, port):
    for _ in range(100):
        if process.poll() is not None: raise RuntimeError('Client process exited before listening')
        try:
            with socket.create_connection(('127.0.0.1',port),timeout=.1): return
        except OSError: time.sleep(.1)
    raise RuntimeError('Client did not start listening')

def udp_through_socks(port):
    query=b'\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x07example\x03com\x00\x00\x01\x00\x01'
    target=None
    for candidate in ('1.1.1.1','8.8.8.8'):
        try:
            with socket.socket(socket.AF_INET,socket.SOCK_DGRAM) as udp:
                udp.settimeout(3);udp.sendto(query,(candidate,53));reply=udp.recv(2048)
                if reply[:2]==query[:2]:target=candidate;break
        except OSError:pass
    if target is None:return False
    with socket.create_connection(('127.0.0.1',port),timeout=5) as control:
        control.sendall(b'\x05\x01\x00');assert control.recv(2)==b'\x05\x00'
        control.sendall(b'\x05\x03\x00\x01'+b'\x00'*6)
        reply=control.recv(100);assert reply[:4]==b'\x05\x00\x00\x01'
        host=socket.inet_ntoa(reply[4:8]);udp_port=struct.unpack('!H',reply[8:10])[0]
        if host=='0.0.0.0':host='127.0.0.1'
        with socket.socket(socket.AF_INET,socket.SOCK_DGRAM) as udp:
            udp.settimeout(8);udp.sendto(b'\x00\x00\x00\x01'+socket.inet_aton(target)+struct.pack('!H',53)+query,(host,udp_port))
            data=udp.recv(2048);assert data[:4]==b'\x00\x00\x00\x01' and data[10:12]==query[:2]
    return True

def main():
    if os.geteuid()!=0 or not pathlib.Path('/run/systemd/system').is_dir():raise RuntimeError('Linux systemd/root required')
    tag=uuid.uuid4().hex[:12]
    root=pathlib.Path('/opt')/('node-manager-acceptance-'+tag);root.mkdir(mode=0o755)
    state=private_directory(root/'state')
    drivers=[]; resources=[]; processes=[]; sockets=[]; failures=[]
    def build(module):
        directory=private_directory(state/module);private_directory(directory/'configs')
        driver=SnellDriver() if module=='snell' else Driver(module,state,root)
        if module=='snell':driver.binary=root/'snell-server';driver.configs=directory/'configs'
        driver.unit=lambda port,m=module:f'node-manager-acceptance-{tag}-{m}-{port}.service'
        drivers.append(driver)
        return Controller(directory,driver,module),driver
    def execute(c,action,payload=None,expected='succeeded'):
        req=dict(id=uuid.uuid4().hex,action=action,payload=payload or {},revision=c.snapshot()['revision'])
        if c.module!='snell':req['module']=c.module
        result=c.execute(req)
        if result['status']!=expected:
            diagnostic=c.root/'last-error.json'
            raise RuntimeError(c.module+': '+json.dumps(result)+(' '+diagnostic.read_text() if diagnostic.exists() else ''))
        return result
    def crud(c,d,item):
        resources.append((d,item['port']))
        execute(c,'apply',item)
        before=c.snapshot()['resources'][0]
        execute(c,'apply',{**item,'name':'edited'})
        updated=c.snapshot()['resources'][0]
        for key in ('psk','uuid','private_key'):
            if key in before:assert before[key]==updated[key]
        original=d.verify
        def fail_once(*args):
            original(*args);raise Rejected('injected verification failure')
        d.verify=fail_once
        execute(c,'apply',{**item,'name':'rollback'},expected='rolled_back')
        d.verify=original
        assert c.snapshot()['resources'][0]['name']=='edited' and d.running(item['port'])
        return updated
    try:
        c,d=build('snell');execute(c,'install')
        port=free_port();crud(c,d,dict(name='Snell',port=port,host='127.0.0.1',public_port=port,mode='default'))
        execute(c,'delete',{'port':port})
        # Privileged listen port used by script input is supported with minimal capability.
        for port in range(900,950):
            try:
                probe=socket.socket();probe.bind(('0.0.0.0',port));probe.close();break
            except OSError:probe.close()
        resources.append((d,port));execute(c,'apply',dict(name='low-port',port=port,host='127.0.0.1',public_port=port,mode='default'));execute(c,'delete',{'port':port})
        print('PASS Snell: CRUD, secret retention, rollback, low-port DynamicUser service',flush=True)
        c,d=build('sb');execute(c,'install')
        port=free_port();item=dict(name='SS2022',port=port,host='127.0.0.1',public_port=port,protocol='ss2022',sni='',psk='')
        actual=crud(c,d,item)
        mixed=free_port();client=root/'client.json'
        client.write_text(json.dumps({'log':{'level':'error'},'inbounds':[{'type':'mixed','listen':'127.0.0.1','listen_port':mixed}], 'outbounds':[{'type':'shadowsocks','server':'127.0.0.1','server_port':port,'method':'2022-blake3-aes-128-gcm','password':actual['psk']}]}));client.chmod(0o600)
        p=subprocess.Popen([str(d.binary),'run','-c',str(client)],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL);processes.append(p)
        wait_client(p,mixed)
        result=subprocess.run(['curl','--proxy',f'socks5h://127.0.0.1:{mixed}','--max-time','25','-fsS','https://example.com'],stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        if result.returncode:raise RuntimeError('SS2022 TCP client failed')
        assert b'Example Domain' in result.stdout
        if udp_through_socks(mixed): print('PASS SB SS2022: real SOCKS UDP DNS exchange',flush=True)
        else: failures.append('Direct public UDP DNS unavailable; SS2022 UDP not verified')
        print('PASS SB SS2022: real sing-box client TCP request, CRUD, secret retention, rollback',flush=True)
        p.terminate();p.wait(timeout=10);processes.remove(p)
        execute(c,'delete',{'port':port})
        # External TLS target may be unavailable. Try the same presets as the script.
        chosen=None
        for target in ('addons.mozilla.org','www.microsoft.com','www.amazon.com','swdist.apple.com','updates.cdn-apple.com'):
            try:d.check_reality(target);chosen=target;break
            except (OSError,Rejected):pass
        if chosen:
            port=free_port();actual=crud(c,d,dict(name='Reality',port=port,host='127.0.0.1',public_port=port,protocol='reality',sni=chosen,psk=''))
            mixed=free_port();client.write_text(json.dumps({'log':{'level':'error'},'inbounds':[{'type':'mixed','listen':'127.0.0.1','listen_port':mixed}], 'outbounds':[{'type':'vless','server':'127.0.0.1','server_port':port,'uuid':actual['uuid'],'flow':'xtls-rprx-vision','tls':{'enabled':True,'server_name':chosen,'utls':{'enabled':True,'fingerprint':'chrome'},'reality':{'enabled':True,'public_key':actual['public_key'],'short_id':actual['short_id']}}}]}))
            p=subprocess.Popen([str(d.binary),'run','-c',str(client)],stdout=subprocess.DEVNULL,stderr=(root/'client-error.log').open('w'));processes.append(p);wait_client(p,mixed)
            result=subprocess.run(['curl','--proxy',f'socks5h://127.0.0.1:{mixed}','--max-time','25','-fsS','https://example.com'],stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            if result.returncode:
                print('Reality target:',chosen,flush=True)
                print('Client log:',(root/'client-error.log').read_text()[-1500:],flush=True)
                print('Client curl error:',result.stderr.decode()[-1000:],flush=True)
                print(command('journalctl','-u',d.unit(port),'-n','12','--no-pager',check=False).stdout.decode(),flush=True)
                raise RuntimeError('Reality TCP client failed')
            assert b'Example Domain' in result.stdout
            p.terminate();p.wait(timeout=10);processes.remove(p)
            execute(c,'delete',{'port':port})
            print('PASS SB Reality: target TLS1.3/h2, real client handshake, CRUD, keys retained, rollback',flush=True)
        else:
            failures.append('External Reality target unavailable; handshake not verified')
        c,d=build('fw');execute(c,'install')
        tcp=socket.socket();tcp.bind(('127.0.0.1',0));tcp.listen();sockets.append(tcp);target=tcp.getsockname()[1]
        udp=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);udp.bind(('127.0.0.1',target));sockets.append(udp)
        def tcp_echo():
            while True:
                try:
                    conn,_=tcp.accept()
                    with conn:
                        data=conn.recv(1024)
                        if data:conn.sendall(data)
                except OSError:return
        def udp_echo():
            while True:
                try:data,peer=udp.recvfrom(1024);udp.sendto(data,peer)
                except OSError:return
        threading.Thread(target=tcp_echo,daemon=True).start();threading.Thread(target=udp_echo,daemon=True).start()
        port=free_port();crud(c,d,dict(name='FW',port=port,target_host='127.0.0.1',target_port=target))
        with socket.create_connection(('127.0.0.1',port),timeout=5) as sock:
            sock.sendall(b'realm-tcp');assert sock.recv(100)==b'realm-tcp'
        with socket.socket(socket.AF_INET,socket.SOCK_DGRAM) as sock:
            sock.settimeout(5);sock.sendto(b'realm-udp',('127.0.0.1',port));assert sock.recv(100)==b'realm-udp'
        before=c.snapshot()['revision'];execute(c,'check',{'port':port},expected='checked');assert c.snapshot()['revision']==before
        execute(c,'delete',{'port':port})
        print('PASS FW: actual TCP and UDP relay, CRUD, rollback',flush=True)
        for message in failures:print('UNVERIFIED:',message,flush=True)
    finally:
        for p in processes:
            p.terminate()
            try:p.wait(timeout=10)
            except subprocess.TimeoutExpired:p.kill();p.wait()
        for sock in sockets:sock.close()
        clean=True
        for d,port in resources:
            command('systemctl','stop',d.unit(port),check=False);command('systemctl','disable',d.unit(port),check=False)
            if d.running(port):clean=False;continue
            (d.units/d.unit(port)).unlink(missing_ok=True)
        command('systemctl','daemon-reload')
        if clean:shutil.rmtree(root)
        else:raise RuntimeError('Cleanup incomplete; retained '+str(root))

if __name__=='__main__':main()
