"""Exercise the built hub's real HTTP routes with disposable state, not mocked handlers."""
import json
import http.client
import base64
import os
import pathlib
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid
import sys


def maximum_snapshot():
    name='𠀀'*48
    host='.'.join(['a'*63]*3+['b'*61])
    key=base64.urlsafe_b64encode(b'x'*32).decode().rstrip('=')
    state={'installed':True,'resources':[], 'revision':'a'*64,'blocked':False,'runtime':{}}
    state['resources']=[dict(name=name,host=host,port=10000+i,public_port=10000+i,psk='x'*128,mode='unshaped') for i in range(64)]
    state['runtime']={str(r['port']):'active' for r in state['resources']}
    modules={m:{'installed':True,'resources':[],'revision':'e'*64,'blocked':False,'runtime':{}} for m in ('sb','fw','pm')}
    modules['sb']['resources']=[dict(name=name,host=host,port=20000+i,public_port=20000+i,protocol='reality',sni=host,psk='',uuid='12345678-1234-4234-8234-123456789abc',private_key=key,public_key=key,short_id='a'*8) for i in range(64)]
    modules['fw']['resources']=[dict(name=name,port=30000+i,target_host='192.0.2.1',target_port=65535) for i in range(64)]
    modules['pm']['resources']=[dict(name=name,port=10000+i,quota_gb=8589934591,quota_mode='in_out',reset_day=31,expiry_date='2099-12-31',egress_mbps=10000) for i in range(64)]
    modules['pm']['usage']={str(10000+i):dict(rx=9223372036854775807,tx=9223372036854775807,sampled_at=1793850000,period_start='2026-10-31',alert='quota_reached') for i in range(64)}
    for m in ('sb','fw'):modules[m]['runtime']={str(r['port']):'active' for r in modules[m]['resources']}
    state['modules']=modules
    state['port_monitor']={'read_only':True,'status':'ready','rows':[dict(port=50000+i,quota_gb=8589934591,quota_mode='in_out',reset_day=31,comment='𠀀'*128,expiry_date='2099-12-31',burst_enabled=True,state='ready',rx=9223372036854775807,tx=9223372036854775807,last_sample=1793850000) for i in range(64)]}
    return state

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self,*args):return None


def main():
    binary=sys.argv[1]
    with tempfile.TemporaryDirectory(prefix='node-manager-http-') as directory:
        logpath=pathlib.Path(directory)/'hub.log'
        with logpath.open('w+') as log:
            process=subprocess.Popen([binary,'--listen','127.0.0.1:19913','--db',directory+'/hub.db','--themes',directory+'/themes','--site','https://panel.example.com'],stdout=log,stderr=log)
            try:
                password=None
                for _ in range(100):
                    text=logpath.read_text()
                    for line in text.splitlines():
                        if 'Emergency password: ' in line: password=line.split('Emergency password: ',1)[1].strip()
                    if password: break
                    if process.poll() is not None: raise RuntimeError(text)
                    time.sleep(.1)
                assert password
                cookie=''
                def call(path, data=None, method=None, credential=None, anonymous=False, extra_headers=None):
                    headers={'Content-Type':'application/json','Origin':'https://panel.example.com','Sec-Fetch-Site':'same-origin'}
                    if cookie and not anonymous: headers['Cookie']=cookie
                    if credential: headers.update({'Authorization':'Bearer '+credential,'X-Node-Manager-Protocol':'1'})
                    headers.update(extra_headers or {})
                    request=urllib.request.Request('http://127.0.0.1:19913'+path,data=json.dumps(data).encode() if data is not None else None,method=method,headers=headers)
                    with urllib.request.urlopen(request,timeout=5) as r:
                        raw=r.read();return r, json.loads(raw) if 'json' in r.headers.get('Content-Type','') else raw.decode()
                try:call('/api/auth/login',{'password':'incorrect'},anonymous=True);raise AssertionError('bad password accepted')
                except urllib.error.HTTPError as e:assert e.code==401
                r,_=call('/api/auth/login',{'password':password})
                cookie=r.headers['Set-Cookie'].split(';')[0]
                assert all(flag in r.headers['Set-Cookie'] for flag in ('HttpOnly','SameSite=Lax','Secure'))
                try:call('/api/management',{'enabled':True},extra_headers={'Origin':'https://attacker.example','Sec-Fetch-Site':'cross-site'});raise AssertionError('cross-origin write accepted')
                except urllib.error.HTTPError as e:assert e.code==403
                # Exercise local OAuth start/failure without external account credentials.
                call('/api/settings',{'github_client_id':'fixture-client','github_client_secret':'fixture-secret','github_allowed_users':''},'PUT')
                _,settings=call('/api/settings');assert 'github_client_secret' not in settings
                opener=urllib.request.build_opener(NoRedirect())
                try:opener.open('http://127.0.0.1:19913/api/auth/github');raise AssertionError('expected OAuth redirect')
                except urllib.error.HTTPError as e:
                    assert e.code==303 and e.headers['Location'].startswith('https://github.com/login/oauth/authorize?')
                    state_cookie=e.headers['Set-Cookie'].split(';')[0]
                    state=state_cookie.split('=',1)[1]
                request=urllib.request.Request('http://127.0.0.1:19913/api/auth/github/callback?code=fixture&state='+state,headers={'Cookie':state_cookie})
                try:opener.open(request);raise AssertionError('empty allowlist accepted')
                except urllib.error.HTTPError as e:
                    assert e.code==303 and 'login_error=' in e.headers['Location']
                    assert 'Max-Age=0' in e.headers['Set-Cookie']
                call('/api/settings',{'github_client_id':''},'PUT')

                try: call('/api/management',anonymous=True);raise AssertionError('anonymous accepted')
                except urllib.error.HTTPError as e: assert e.code==401
                call('/api/nodes',{'name':'http-e2e'})
                _,nodes=call('/api/nodes');node=nodes['nodes'][0]['id']
                call('/api/management',{'enabled':True})
                _,enrollment=call(f'/api/management/{node}/enrollment',{},'POST')
                credential='c'*64
                registration={'node':node,'enrollment':enrollment['enrollment'],'credential':credential}
                call('/api/management-agent/v1/enroll',registration,credential=credential,anonymous=True)
                snapshot={'installed':False,'resources':[],'revision':'b'*64,'blocked':False}
                call('/api/management-agent/v1/poll',{'snapshot':snapshot,'result':None},credential=credential,anonymous=True)
                operation={'id':uuid.uuid4().hex,'action':'install','revision':'b'*64,'payload':{}}
                call(f'/api/management/{node}/operations',operation)
                _,result=call('/api/management-agent/v1/poll',{'snapshot':snapshot,'result':None},credential=credential,anonymous=True)
                assert result['task']['request']==operation
                snell={'name':'Snell','port':23456,'host':'node.example.com','public_port':23456,'psk':'s'*32,'mode':'default'}
                snapshot.update(installed=True,revision='a'*64,resources=[snell])
                receipt={'id':operation['id'],'status':'succeeded','message':'okay','revision':'a'*64}
                call('/api/management-agent/v1/poll',{'snapshot':snapshot,'result':receipt},credential=credential,anonymous=True)
                _,state=call(f'/api/management/{node}')
                assert state['operations'][0]['status']=='succeeded'
                assert state['snapshot']['installed']
                _,snell_export=call(f'/api/management/{node}/export?format=surge')
                assert snell_export.startswith('[Proxy]\n') and 'psk='+snell['psk'] in snell_export
                if validator:=os.environ.get('SNELL_VALIDATOR'):
                    profile=pathlib.Path(directory)/'snell.conf'
                    profile.write_text('[General]\n\n'+snell_export+'\n[Rule]\nFINAL,DIRECT\n')
                    subprocess.run([validator,'--check',str(profile)],check=True,timeout=10)
                    print('PASS: Surge parsed the downloaded Snell configuration')
                # Module-specific revisions and export must use only the bound node's state.
                ss={'name':'SS2022','port':12345,'host':'node.example.com','public_port':12345,'protocol':'ss2022','sni':'','psk':'AQEBAQEBAQEBAQEBAQEBAQ=='}
                snapshot['modules']={'sb':{'installed':True,'resources':[ss],'revision':'d'*64,'blocked':False,'runtime':{'12345':'active'}}, 'fw':{'installed':True,'resources':[],'revision':'e'*64,'blocked':False,'runtime':{}}}
                snapshot['port_monitor']={'status':'not_installed','read_only':True,'rows':[]}
                call('/api/management-agent/v1/poll',{'snapshot':snapshot,'result':None},credential=credential,anonymous=True)
                _,state=call(f'/api/management/{node}')
                assert 'psk' not in state['snapshot']['modules']['sb']['resources'][0]
                exported,export=call(f'/api/management/{node}/export?format=sing-box')
                assert exported.headers['Cache-Control']=='no-store'
                try: call(f'/api/management/{node}/export?format=sing-box',anonymous=True);raise AssertionError('anonymous export accepted')
                except urllib.error.HTTPError as e: assert e.code==401
                client=json.loads(export)
                assert client['outbounds'][1]['password']==ss['psk'] and 'private_key' not in export
                op={'id':uuid.uuid4().hex,'module':'sb','action':'apply','revision':'d'*64,'payload':{**ss,'psk':''}}
                call(f'/api/management/{node}/operations',op)
                _,reply=call('/api/management-agent/v1/poll',{'snapshot':snapshot,'result':None},credential=credential,anonymous=True)
                assert reply['task']['request']['module']=='sb'
                call('/api/management-agent/v1/poll',{'snapshot':snapshot,'result':{'id':op['id'],'status':'rejected','message':'DO NOT DISPLAY SECRET','code':'port_in_use'}},credential=credential,anonymous=True)
                _,state=call(f'/api/management/{node}')
                assert state['operations'][0]['module']=='sb' and state['operations'][0]['action']=='apply'
                assert 'DO NOT DISPLAY SECRET' not in str(state) and '监听端口已被占用' in str(state)
                # Core writes require an admin, a snapshot with core state and an eligible version.
                route=f'/api/management/{node}/operations'
                upgrade={'id':uuid.uuid4().hex,'module':'sb','action':'core_upgrade','revision':'d'*64,'payload':{'version':'1.15.0'}}
                for kwargs in ({'anonymous':True},{'anonymous':True,'credential':credential}):
                    try: call(route,upgrade,**kwargs);raise AssertionError('core write accepted without an admin session')
                    except urllib.error.HTTPError as e: assert e.code==401
                try: call(route,upgrade);raise AssertionError('core write accepted without core state')
                except urllib.error.HTTPError as e: assert e.code==400
                snapshot['modules']['sb']['core']={'version':'1.14.4','sha256':'a'*64,'previous_version':'1.14.3','candidates':['1.15.0']}
                call('/api/management-agent/v1/poll',{'snapshot':snapshot,'result':None},credential=credential,anonymous=True)
                for invalid in (
                    {**upgrade,'payload':{'version':'1.15.0','url':'https://example.com/binary'}},
                    {**upgrade,'payload':{'version':'1.16.0'}},
                    {**upgrade,'action':'core_rollback','payload':{'version':'1.14.2'}},
                    {**upgrade,'action':'core_uninstall','payload':{}},
                ):
                    try: call(route,invalid);raise AssertionError('ineligible core write accepted')
                    except urllib.error.HTTPError as e: assert e.code==400
                call(route,upgrade)
                # A component on another protocol reports presence but can't claim the queued write.
                try: call('/api/management-agent/v1/poll',{'snapshot':snapshot,'result':None},credential=credential,anonymous=True,extra_headers={'X-Node-Manager-Protocol':'0'});raise AssertionError('other protocol exchanged work')
                except urllib.error.HTTPError as e: assert e.code==409
                _,state=call(f'/api/management/{node}')
                assert state['online'] and not state['compatible'] and state['operations'][0]['status']=='queued'
                _,reply=call('/api/management-agent/v1/poll',{'snapshot':snapshot,'result':None},credential=credential,anonymous=True)
                assert reply['task']['request']['id']==upgrade['id']
                call('/api/management-agent/v1/poll',{'snapshot':snapshot,'result':{'id':upgrade['id'],'status':'succeeded','message':'ok'}},credential=credential,anonymous=True)
                rollback={**upgrade,'id':uuid.uuid4().hex,'action':'core_rollback','payload':{'version':'1.14.3'}}
                call(route,rollback)
                _,reply=call('/api/management-agent/v1/poll',{'snapshot':snapshot,'result':None},credential=credential,anonymous=True)
                assert reply['task']['request']==rollback
                call('/api/management-agent/v1/poll',{'snapshot':snapshot,'result':{'id':rollback['id'],'status':'rolled_back','message':'fixture failure','code':'core_runtime'}},credential=credential,anonymous=True)
                _,state=call(f'/api/management/{node}')
                assert state['operations'][0]['status']=='rolled_back'
                assert '已保留或恢复原配置，请检查实例运行状态' in state['operations'][0]['message']
                assert '健康检查失败' in state['operations'][0]['message'] and '目标版本 1.14.3' in state['operations'][0]['message']
                snapshot['modules']['pm']={'installed':True,'resources':[],'revision':'f'*64,'blocked':False,'runtime':{},
                                           'core':{'version':None,'sha256':None,'previous_version':None,'candidates':[]}}
                call('/api/management-agent/v1/poll',{'snapshot':snapshot,'result':None},credential=credential,anonymous=True)
                uninstall={**upgrade,'id':uuid.uuid4().hex,'module':'pm','action':'core_uninstall','revision':'f'*64,'payload':{}}
                call(route,uninstall)
                _,reply=call('/api/management-agent/v1/poll',{'snapshot':snapshot,'result':None},credential=credential,anonymous=True)
                assert reply['task']['request']==uninstall
                snapshot['modules']['pm'].update(installed=False,revision='0'*64)
                call('/api/management-agent/v1/poll',{'snapshot':snapshot,'result':{'id':uninstall['id'],'status':'succeeded','message':'fixture success'}},credential=credential,anonymous=True)
                _,state=call(f'/api/management/{node}')
                assert state['operations'][0]['status']=='succeeded' and state['operations'][0]['message']=='已卸载模块'
                large=maximum_snapshot()
                payload={'snapshot':large,'result':None}
                assert 256*1024 < len(json.dumps(payload).encode()) < 512*1024
                call('/api/management-agent/v1/poll',payload,credential=credential,anonymous=True)
                large_op={'id':uuid.uuid4().hex,'module':'fw','action':'check','revision':'e'*64,'payload':{'port':30000}}
                call(f'/api/management/{node}/operations',large_op)
                _,reply=call('/api/management-agent/v1/poll',payload,credential=credential,anonymous=True)
                assert reply['task']['request']['id']==large_op['id']
                payload['result']={'id':large_op['id'],'status':'checked','message':'checked'}
                call('/api/management-agent/v1/poll',payload,credential=credential,anonymous=True)
                _,state=call(f'/api/management/{node}')
                assert state['online'] and state['operations'][0]['status']=='checked'
                for route,size in (('/api/management-agent/v1/poll',512*1024),('/api/management-agent/v1/enroll',64*1024)):
                    # These routes reject a declared oversized body before reading it.
                    connection=http.client.HTTPConnection('127.0.0.1',19913,timeout=5)
                    try:
                        connection.putrequest('POST',route)
                        connection.putheader('Authorization','Bearer '+credential)
                        connection.putheader('Content-Type','application/json')
                        connection.putheader('Content-Length',str(size+1))
                        connection.endheaders()
                        response=connection.getresponse()
                        assert response.status==413, (route,response.status)
                        response.read()
                    finally: connection.close()
                call(f'/api/management/{node}',method='DELETE')
                try: call('/api/management-agent/v1/poll',{'snapshot':snapshot,'result':None},credential=credential,anonymous=True);raise AssertionError('revoked accepted')
                except urllib.error.HTTPError as e: assert e.code==401 and e.headers.get('X-Node-Manager-Binding')=='revoked'
                call('/api/settings',{'github_allowed_users':'alice,bob'},'PUT')
                r,_=call('/api/auth/login',{'password':password},anonymous=True)
                other_cookie=r.headers['Set-Cookie'].split(';')[0]
                prior_cookie=cookie
                slow=http.client.HTTPConnection('127.0.0.1',19913,timeout=5)
                late_body=json.dumps({'github_allowed_users':'alice,bob'}).encode()
                slow.putrequest('PUT','/api/settings')
                for key,value in {'Cookie':prior_cookie,'Content-Type':'application/json','Content-Length':str(len(late_body)),'Origin':'https://panel.example.com','Sec-Fetch-Site':'same-origin'}.items():
                    slow.putheader(key,value)
                slow.endheaders()
                time.sleep(.1)
                r,_=call('/api/settings',{'github_allowed_users':'bob'},'PUT')
                cookie=r.headers['Set-Cookie'].split(';')[0]
                try:
                    slow.send(late_body)
                    rejected=slow.getresponse()
                    assert rejected.status==401, 'delayed revoked request restored authority'
                    rejected.read()
                finally:slow.close()
                _,saved=call('/api/settings');assert saved['github_allowed_users']=='bob'
                for stale in (prior_cookie,other_cookie):
                    try:call('/api/settings',extra_headers={'Cookie':stale});raise AssertionError('removed access retained session')
                    except urllib.error.HTTPError as e:assert e.code==401
                call('/api/settings')
                original_cookie=cookie
                r,_=call('/api/settings',{'admin_password':'replacement-test-password'},'PUT')
                cookie=r.headers['Set-Cookie'].split(';')[0]
                try:call('/api/settings',extra_headers={'Cookie':original_cookie});raise AssertionError('old session survived password change')
                except urllib.error.HTTPError as e:assert e.code==401
                call('/api/auth/logout',{})
                try:call('/api/settings');raise AssertionError('logout did not revoke session')
                except urllib.error.HTTPError as e:assert e.code==401
                print('PASS: login/logout/password rotation and delayed revocation, OAuth failure safety, CSRF, auth, maximum module snapshot/receipt, size limits, protected exports, retired subscriptions, core lifecycle permissions and revoke')
            finally:
                process.terminate()
                try:process.wait(timeout=10)
                except subprocess.TimeoutExpired:process.kill();process.wait()

if __name__=='__main__': main()
