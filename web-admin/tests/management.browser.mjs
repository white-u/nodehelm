// Opt-in browser acceptance against a disposable real hub. No test data enters live VPS databases.
import { spawn } from 'node:child_process'
import { mkdtemp, rm } from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
import { createRequire } from 'node:module'
import assert from 'node:assert/strict'
const require = createRequire(process.env.PLAYWRIGHT_PACKAGE ?? import.meta.url)
const { chromium } = require('playwright')
const binary=process.argv[2]
if (!binary) throw new Error('Usage: node management.browser.mjs /path/to/host-native/monitor-hub')
const directory=await mkdtemp(path.join(os.tmpdir(),'nm-browser-'))
const base='http://127.0.0.1:19917'
const hub=spawn(binary,['--listen','127.0.0.1:19917','--db',directory+'/hub.db','--themes',directory+'/themes','--site','https://panel.example.com'])
let logs='';hub.stdout.on('data',d=>logs+=d);hub.stderr.on('data',d=>logs+=d)
let browser
try {
  let password
  for(let i=0;i<100;i++) {password=logs.match(/Emergency password: ([^\r\n]+)/)?.[1].trim();if(password)break;if(hub.exitCode!==null)throw new Error('Test hub exited');await new Promise(r=>setTimeout(r,100))}
  assert(password)
  browser=await chromium.launch({headless:true,...(process.env.CHROME_BINARY?{executablePath:process.env.CHROME_BINARY}:{})})
  const context=await browser.newContext({viewport:{width:1280,height:900}})
  const page=await context.newPage();const downloads=[];page.on('download',download=>downloads.push(download.suggestedFilename()));const errors=[];page.on('pageerror',e=>errors.push(e.message));page.on('console',message=>{if(/invalid form control.*not focusable/i.test(message.text()))errors.push(message.text())})
  await page.goto(base+'/admin/manage')
  await page.getByLabel('应急密码').fill(password)
  await page.getByRole('button',{name:'登录',exact:true}).click()
  await page.waitForURL('**/admin/nodes')
  await page.goto(base+'/admin/manage')
  await page.getByRole('heading',{name:'节点服务管理'}).waitFor()
  async function api(route,data,method=data===undefined?'GET':'POST') {
    const response=await context.request.fetch(base+route,{method,data,headers:{Origin:base,'Sec-Fetch-Site':'same-origin'}})
    assert(response.ok(),`${route}: HTTP ${response.status()}`)
    return response.json()
  }
  const {id}=await api('/api/nodes',{name:'表单验收',public:false})
  await api(`/api/nodes/${id}`,{ipv4_pin:'203.0.113.42'},'PUT')
  await api('/api/management',{enabled:true})
  const enrollment=await api(`/api/management/${id}/enrollment`,{})
  const credential='c'.repeat(64)
  await api('/api/management-agent/v1/enroll',{node:id,enrollment:enrollment.enrollment,credential})
  const empty={installed:true,resources:[],revision:'a'.repeat(64),blocked:false,runtime:{},core:{version:'6.0.0',sha256:'a'.repeat(64),previous_version:'5.0.0',candidates:['6.1.0']}}
  const snapshot={...empty,modules:{sb:{...empty},fw:{...empty},pm:{...empty,core:{candidates:[]}}},port_monitor:{read_only:true,status:'not_installed',rows:[]}}
  async function poll(result=null) {
    const response=await context.request.post(base+'/api/management-agent/v1/poll',{data:{snapshot,result},headers:{Authorization:'Bearer '+credential,'X-Node-Manager-Protocol':'1'}})
    assert(response.ok(),`poll: HTTP ${response.status()} ${await response.text()}`);return response.json()
  }
  await poll();await page.reload()
  const instances=page.getByTestId('instance-config'),detail=page.getByTestId('node-detail')
  const editor=page.getByTestId('service-instance-editor'),pmEditor=page.getByTestId('pm-policy-editor')
  const nodePanel=page.getByTestId('node-management-panel')
  page.on('dialog',async dialog=>{errors.push('unexpected native dialog: '+dialog.type());await dialog.dismiss()})
  await context.grantPermissions(['clipboard-read','clipboard-write'])
  let reads=0
  page.on('request',request=>{if(request.method()==='GET'&&/^\/api\/management(?:\/\d+)?$/.test(new URL(request.url()).pathname))reads++})
  const refresh=()=>page.getByRole('button',{name:'刷新此节点',exact:true}).click()
  async function select(module,node=id){
    await page.getByTestId(`node-module-${node}-${module}`).click()
    await page.waitForFunction(({node,module})=>document.querySelector(`[data-testid="node-module-${node}-${module}"]`)?.getAttribute('aria-pressed')==='true',{node,module})
  }
  const decision=()=>page.getByRole('region',{name:'操作确认'})
  async function accept(label='确认操作'){await decision().getByRole('button',{name:label,exact:true}).click()}
  async function submit(button){
    const posted=page.waitForResponse(response=>response.request().method()==='POST'&&response.url().endsWith('/operations'))
    await button.click();await accept();const response=await posted;assert(response.ok(),await response.text());return (await poll()).task
  }
  async function finish(task){await poll({id:task.request.id,status:'succeeded',message:'fixture applied'});await refresh()}
  async function fits(){assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth+1),'page has horizontal overflow')}
  await instances.getByRole('button',{name:'新增实例',exact:true}).waitFor()
  assert.equal(await page.getByRole('dialog').count(),0)
  await page.getByTestId(`node-manage-${id}`).click()
  await nodePanel.waitFor();assert.equal(await nodePanel.getByTestId('core-maintenance').count(),0)
  await nodePanel.locator('summary').filter({hasText:'紧急撤销授权'}).click()
  await nodePanel.getByRole('button',{name:'撤销管理绑定',exact:true}).waitFor()
  await nodePanel.getByRole('button',{name:'收起',exact:true}).click();await nodePanel.waitFor({state:'detached'})

  const add=instances.getByRole('button',{name:'新增实例',exact:true})
  await add.click();await editor.getByLabel('监听端口',{exact:true}).fill('12345')
  assert.equal(await editor.locator('input:visible:not([type=checkbox])').count(),2,'Snell must only expose name and port; the client port stays folded')
  assert(!await editor.getByLabel(/同时启用 PM 统计/).isChecked(),'adding PM is off by default')
  await editor.getByLabel('监听端口',{exact:true}).fill('443');assert(await editor.getByLabel(/同时启用 PM 统计/).isDisabled());await editor.getByText('（PM 只统计 1024 以上的端口）').waitFor()
  await editor.getByLabel('监听端口',{exact:true}).fill('12345')
  assert.equal(await editor.getByText('高级配置',{exact:true}).count(),0)
  await editor.getByRole('button',{name:'取消',exact:true}).click();await decision().getByRole('button',{name:'取消操作',exact:true}).click()
  assert.equal(await editor.getByLabel('监听端口',{exact:true}).inputValue(),'12345')
  await editor.getByRole('button',{name:'取消',exact:true}).click();await accept('放弃修改');await editor.waitFor({state:'detached'})
  await add.click();await editor.getByLabel('监听端口',{exact:true}).fill('12345')
  let task=await submit(editor.getByRole('button',{name:'保存并应用',exact:true}))
  assert.equal(task.request.intent,'create');assert.equal(task.request.payload.name,'12345');assert.equal(task.request.payload.public_port,12345);assert.equal(task.request.payload.psk,'')
  const savedSnell={...task.request.payload,psk:'s'.repeat(32)}
  snapshot.resources=[savedSnell];snapshot.runtime={'12345':'active'};snapshot.revision='b'.repeat(64);await finish(task)
  snapshot.resources.push({...savedSnell,name:'Other Snell',port:12346,public_port:12346,host:'other-snell.example.com',psk:'o'.repeat(32)})
  snapshot.runtime['12346']='active';snapshot.revision='c'.repeat(64);await poll();await refresh()
  const first=instances.getByTestId('instance-12345'),secondSnell=instances.getByTestId('instance-12346')
  assert(await instances.getByTestId('core-maintenance').evaluate(element=>!!(element.compareDocumentPosition(document.querySelector('[data-testid="instance-12345"]'))&Node.DOCUMENT_POSITION_FOLLOWING)),'core maintenance must precede instance rows')
  await first.getByRole('button',{name:'编辑',exact:true}).click();assert.equal(await editor.getByLabel('监听端口',{exact:true}).inputValue(),'12345')
  await secondSnell.getByRole('button',{name:'编辑',exact:true}).click();assert.equal(await editor.getByLabel('名称（可选）',{exact:true}).inputValue(),'Other Snell')
  await secondSnell.getByRole('button',{name:'收起编辑',exact:true}).click();await editor.waitFor({state:'detached'})
  await first.getByRole('button',{name:'显示配置',exact:true}).click()
  const configuration=first.getByTestId('client-configuration')
  await configuration.waitFor();assert.equal(await configuration.getByRole('button').count(),1);assert.equal(await configuration.locator('p,h3,span').count(),0)
  assert.match(await configuration.getByLabel('客户端配置内容').textContent(),/203\.0\.113\.42/)
  assert.equal((await configuration.textContent()).includes('other-snell.example.com'),false)
  await configuration.getByRole('button',{name:'复制',exact:true}).click();await configuration.getByRole('button',{name:'已复制',exact:true}).waitFor()
  await first.getByRole('button',{name:'收起配置',exact:true}).click();await configuration.waitFor({state:'detached'})

  let releaseExport,exportStarted
  const exportGate=new Promise(resolve=>releaseExport=resolve),started=new Promise(resolve=>exportStarted=resolve)
  const exportRoute=`**/api/management/${id}/export?*`
  await page.route(exportRoute,async route=>{const response=await route.fetch();exportStarted();await exportGate;await route.fulfill({response})})
  await first.getByRole('button',{name:'显示配置',exact:true}).click();await started;await select('sb');releaseExport();await page.unroute(exportRoute,{behavior:'wait'})
  assert.equal(await page.getByTestId('client-configuration').count(),0,'late export leaked across modules')
  await instances.getByRole('button',{name:'新增实例',exact:true}).click()
  await editor.getByLabel('监听端口',{exact:true}).fill('34567');await editor.getByLabel('SNI 地址',{exact:true}).fill('www.microsoft.com')
  task=await submit(editor.getByRole('button',{name:'保存并应用',exact:true}));assert.equal(task.request.payload.name,'34567')
  const reality={...task.request.payload,uuid:'11111111-1111-4111-8111-111111111111',private_key:Buffer.alloc(32,2).toString('base64url'),public_key:Buffer.alloc(32,1).toString('base64url'),short_id:'1234abcd'}
  snapshot.modules.sb.resources=[reality];snapshot.modules.sb.runtime={'34567':'active'};snapshot.modules.sb.revision='b'.repeat(64);await finish(task)
  const realityRow=instances.getByTestId('instance-34567')
  await realityRow.getByText('运行中',{exact:true}).waitFor();assert.equal(await realityRow.getByText('www.microsoft.com',{exact:true}).count(),0)
  await realityRow.getByRole('button',{name:'显示配置',exact:true}).click();assert.match(await realityRow.getByLabel('客户端配置内容').textContent(),/^vless:\/\//)
  assert(!(await realityRow.getByLabel('客户端配置内容').textContent()).includes(reality.private_key))
  await instances.getByRole('button',{name:'新增实例',exact:true}).click()
  await editor.getByLabel('协议',{exact:true}).selectOption('ss2022');assert.equal(await editor.getByLabel('SNI 地址',{exact:true}).count(),0)
  await editor.getByLabel('监听端口',{exact:true}).fill('34568')
  // A NAT-mapped client port is folded away by default and follows the listen port until changed.
  await editor.getByText('客户端端口（NAT 端口映射时修改）',{exact:true}).click();assert.equal(await editor.getByLabel('客户端访问端口',{exact:true}).inputValue(),'34568')
  await editor.getByLabel('客户端访问端口',{exact:true}).fill('44568')
  task=await submit(editor.getByRole('button',{name:'保存并应用',exact:true}));assert.equal(task.request.payload.sni,'');assert.equal(task.request.payload.public_port,44568)
  snapshot.modules.sb.resources.push({...task.request.payload,psk:'AQEBAQEBAQEBAQEBAQEBAQ=='});snapshot.modules.sb.runtime['34568']='active';snapshot.modules.sb.revision='c'.repeat(64);await finish(task)
  const ssRow=instances.getByTestId('instance-34568')
  await ssRow.getByText(/^端口 34568 · 客户端端口 44568( · 添加 PM)?$/).waitFor()
  await realityRow.getByRole('button',{name:'编辑',exact:true}).click();assert.equal(await editor.getByLabel('SNI 地址',{exact:true}).inputValue(),'www.microsoft.com')
  await ssRow.getByRole('button',{name:'编辑',exact:true}).click();assert.equal(await editor.getByLabel('SNI 地址',{exact:true}).count(),0)
  await ssRow.getByRole('button',{name:'收起编辑',exact:true}).click();await ssRow.getByRole('button',{name:'显示配置',exact:true}).click()
  assert.match(await ssRow.getByLabel('客户端配置内容').textContent(),/^ss:\/\/2022-blake3-aes-128-gcm:/)

  await select('fw');await instances.getByRole('button',{name:'新增转发规则',exact:true}).click()
  await editor.getByLabel('监听端口',{exact:true}).fill('23456');await editor.getByLabel('目标 IPv4 地址',{exact:true}).fill('invalid-target')
  const rejected=page.waitForResponse(response=>response.request().method()==='POST'&&response.url().endsWith('/operations'))
  await editor.getByRole('button',{name:'保存并应用',exact:true}).click();await accept();assert.equal((await rejected).status(),400)
  assert.equal(await editor.getByLabel('目标 IPv4 地址',{exact:true}).inputValue(),'invalid-target')
  await editor.getByLabel('目标 IPv4 地址',{exact:true}).fill('192.0.2.1');task=await submit(editor.getByRole('button',{name:'保存并应用',exact:true}))
  snapshot.modules.fw.resources=[task.request.payload];snapshot.modules.fw.runtime={'23456':'active'};await finish(task)

  snapshot.modules.pm.installed=false;await poll();await select('pm');await refresh()
  task=await submit(instances.getByRole('button',{name:'启用 PM',exact:true}));assert.equal(task.request.action,'install')
  snapshot.modules.pm.installed=true;await finish(task)
  await instances.getByRole('button',{name:'新增 PM 规则',exact:true}).click();await pmEditor.getByLabel('业务端口',{exact:true}).selectOption('23456')
  await pmEditor.getByLabel('配额（GiB，0 关闭）',{exact:true}).fill('10');await pmEditor.getByLabel('每月重置日（0 关闭）',{exact:true}).fill('31')
  task=await submit(pmEditor.getByRole('button',{name:'保存并应用',exact:true}));assert(!('ip_limit' in task.request.payload),'an IP limit left at 0 is not sent')
  snapshot.modules.pm.resources=[task.request.payload];snapshot.modules.pm.revision='b'.repeat(64);await finish(task)
  await instances.getByRole('button',{name:'编辑',exact:true}).click();await pmEditor.getByLabel('配额（GiB，0 关闭）',{exact:true}).fill('20')
  snapshot.modules.pm.revision='c'.repeat(64);snapshot.modules.pm.resources[0]={...snapshot.modules.pm.resources[0],quota_gb:30}
  await poll();await refresh();await pmEditor.getByRole('button',{name:'重新载入最新配置',exact:true}).waitFor();assert.equal(await pmEditor.getByLabel('配额（GiB，0 关闭）',{exact:true}).inputValue(),'20')
  assert(await pmEditor.getByRole('button',{name:'保存并应用',exact:true}).isDisabled())
  await pmEditor.getByRole('button',{name:'重新载入最新配置',exact:true}).click();await accept()
  assert.equal(await pmEditor.getByLabel('配额（GiB，0 关闭）',{exact:true}).inputValue(),'30')
  await pmEditor.getByRole('button',{name:'取消',exact:true}).click();await pmEditor.waitFor({state:'detached'})
  task=await submit(instances.getByRole('button',{name:'重置用量',exact:true}));assert.equal(task.request.action,'reset');await finish(task)
  await instances.getByRole('button',{name:'编辑',exact:true}).click();await pmEditor.locator('summary').filter({hasText:'IP 告警'}).click()
  await pmEditor.getByLabel('同时在线 IP 上限（0 关闭）',{exact:true}).fill('2');await pmEditor.getByLabel('白名单 IP（可选，每行或逗号分隔，最多 32 个）',{exact:true}).fill('203.0.113.9, 2001:db8::9')
  task=await submit(pmEditor.getByRole('button',{name:'保存并应用',exact:true}))
  assert.deepEqual(task.request.payload.ip_limit,{max_ips:2,cooldown_min:30,whitelist:['203.0.113.9','2001:db8::9']})
  snapshot.modules.pm.resources=[task.request.payload];snapshot.modules.pm.revision='d'.repeat(64)
  snapshot.modules.pm.usage={'23456':{rx:0,tx:0,sampled_at:Math.floor(Date.now()/1000),period_start:'',alert:'normal',ip_count:4,ip_level:1,ip_alert:1,ip_list:['198.51.100.1','198.51.100.2','198.51.100.3','198.51.100.4']}}
  await finish(task);await instances.getByText('在线 IP 4 / 2 · 198.51.100.1、198.51.100.2、198.51.100.3、198.51.100.4',{exact:true}).waitFor()
  assert.match(await instances.getByText(/^在线 IP 4 \/ 2/).getAttribute('class'),/text-destructive/)

  // A new instance with PM: the rule waits for the instance to run, and writes stay locked in between.
  await select('snell');const progress=page.getByTestId('chain-progress')
  const posted=()=>page.waitForResponse(response=>response.request().method()==='POST'&&response.url().endsWith('/operations'))
  const snellRow=(port,running='active')=>({...savedSnell,name:String(port),port,public_port:port,psk:'p'.repeat(32),_running:running})
  const addSnell=row=>{const {_running,...resource}=row;snapshot.resources.push(resource);snapshot.runtime[String(row.port)]=_running;snapshot.revision=row.port.toString(16).padStart(64,'0')}
  await add.click();await editor.getByLabel('监听端口',{exact:true}).fill('40001');await editor.getByLabel(/同时启用 PM 统计/).check()
  await editor.getByLabel('配额（GiB，0 关闭）',{exact:true}).fill('100');await editor.locator('summary').filter({hasText:'IP 告警'}).click()
  await editor.getByLabel('同时在线 IP 上限（0 关闭）',{exact:true}).fill('3');await editor.getByLabel('白名单 IP（可选，每行或逗号分隔，最多 32 个）',{exact:true}).fill('203.0.113.0/24')
  await editor.getByRole('button',{name:'保存并应用',exact:true}).click();await editor.getByText(/203\.0\.113\.0\/24 不是有效的 IP 地址/).waitFor()
  assert.equal(await decision().count(),0,'an invalid PM field stops before anything is submitted')
  await editor.getByLabel('白名单 IP（可选，每行或逗号分隔，最多 32 个）',{exact:true}).fill('203.0.113.5')
  await editor.getByRole('button',{name:'保存并应用',exact:true}).click();assert.match(await decision().textContent(),/实例运行后应用 PM 规则（配额 100 GiB／IP 告警 3 个）/)
  let response=posted();await accept();await response;task=(await poll()).task
  assert.equal(task.request.module,undefined);assert.equal(task.request.payload.port,40001)
  addSnell(snellRow(40001,'activating'));await poll({id:task.request.id,status:'succeeded',message:'fixture applied'})
  await progress.filter({hasText:'1/2 创建实例'}).waitFor();assert(await add.isDisabled(),'writes stay locked between the two steps')
  response=posted();await page.waitForTimeout(3500);snapshot.runtime['40001']='active';await poll();await response
  task=(await poll()).task;assert.equal(task.request.module,'pm');assert.equal(task.request.intent,'create')
  assert.deepEqual(task.request.payload,{port:40001,name:'40001',quota_gb:100,quota_mode:'in_out',reset_day:0,expiry_date:'',egress_mbps:0,ip_limit:{max_ips:3,cooldown_min:30,whitelist:['203.0.113.5']}})
  await progress.filter({hasText:'2/2 应用 PM 规则'}).waitFor();assert(await add.isDisabled())
  snapshot.modules.pm.resources.push(task.request.payload);snapshot.modules.pm.revision='e'.repeat(64);await poll({id:task.request.id,status:'succeeded',message:'fixture applied'})
  await progress.filter({hasText:'实例和 PM 规则已完成。'}).waitFor();assert(!await add.isDisabled())
  await instances.getByTestId('instance-40001').getByRole('button',{name:'PM 已统计',exact:true}).click()
  await pmEditor.getByText('编辑 PM 规则',{exact:false}).waitFor();assert.equal(await pmEditor.getByLabel('配额（GiB，0 关闭）',{exact:true}).inputValue(),'100')
  await pmEditor.getByRole('button',{name:'取消',exact:true}).click();await pmEditor.waitFor({state:'detached'})

  // A failed instance submits no PM rule.
  await select('snell');await add.click();await editor.getByLabel('监听端口',{exact:true}).fill('40002');await editor.getByLabel(/同时启用 PM 统计/).check()
  task=await submit(editor.getByRole('button',{name:'保存并应用',exact:true}))
  let extra=0;const countPosts=request=>{if(request.method()==='POST'&&request.url().endsWith('/operations'))extra++};page.on('request',countPosts)
  await poll({id:task.request.id,status:'rolled_back',message:'fixture failed'});await progress.filter({hasText:'创建实例未成功，未应用 PM 规则。'}).waitFor()
  await page.waitForTimeout(3500);assert.equal(extra,0);page.off('request',countPosts)

  // A rejected PM step keeps what was filled in for "添加 PM".
  await add.click();await editor.getByLabel('监听端口',{exact:true}).fill('40003');await editor.getByLabel(/同时启用 PM 统计/).check();await editor.getByLabel('配额（GiB，0 关闭）',{exact:true}).fill('7')
  task=await submit(editor.getByRole('button',{name:'保存并应用',exact:true}));addSnell(snellRow(40003))
  response=posted();await poll({id:task.request.id,status:'succeeded',message:'fixture applied'});await response;task=(await poll()).task
  await poll({id:task.request.id,status:'rejected',message:'原 PM 正忙'});await progress.filter({hasText:/^应用 PM 规则未成功：请求被拒绝[^。]*。可在实例行点击“添加 PM”继续，已保留填写的参数。$/}).waitFor()
  await instances.getByTestId('instance-40003').getByRole('button',{name:'添加 PM',exact:true}).click()
  await pmEditor.getByText('新增 PM 规则',{exact:false}).waitFor();assert.equal(await pmEditor.getByLabel('配额（GiB，0 关闭）',{exact:true}).inputValue(),'7')
  await pmEditor.getByRole('button',{name:'取消',exact:true}).click();await pmEditor.waitFor({state:'detached'})

  // Deleting an instance with PM removes the rule first, then the instance.
  await select('snell');await instances.getByTestId('instance-40001').getByRole('button',{name:'删除',exact:true}).click()
  assert.match(await decision().textContent(),/将先删除该端口的 PM 规则及计数/)
  response=posted();await accept();await response;task=(await poll()).task
  assert.equal(task.request.module,'pm');assert.equal(task.request.action,'delete');assert.equal(task.request.payload.port,40001)
  snapshot.modules.pm.resources=snapshot.modules.pm.resources.filter(rule=>rule.port!==40001);snapshot.modules.pm.revision='f'.repeat(64)
  response=posted();await poll({id:task.request.id,status:'succeeded',message:'fixture applied'});await response;task=(await poll()).task
  assert.equal(task.request.module,undefined);assert.equal(task.request.action,'delete');assert.equal(task.request.payload.port,40001)
  snapshot.resources=snapshot.resources.filter(row=>row.port!==40001);delete snapshot.runtime['40001'];snapshot.revision='9'.repeat(64)
  await poll({id:task.request.id,status:'succeeded',message:'fixture applied'});await progress.filter({hasText:'PM 规则和实例已删除。'}).waitFor()

  await select('snell');task=await submit(instances.getByRole('button',{name:'升级核心',exact:true}));assert.equal(task.request.action,'core_upgrade')
  await poll({id:task.request.id,status:'rolled_back',message:'private binary path',code:'core_runtime'});await refresh()
  await instances.getByRole('status').filter({hasText:/核心或业务实例健康检查失败/}).waitFor();assert.equal(await page.getByText(/private binary path/).count(),0)
  const second=await api('/api/nodes',{name:'东京服务节点',public:false}),enrollment2=await api(`/api/management/${second.id}/enrollment`,{}),credential2='d'.repeat(64)
  await api('/api/management-agent/v1/enroll',{node:second.id,enrollment:enrollment2.enrollment,credential:credential2})
  const secondPoll=await context.request.post(base+'/api/management-agent/v1/poll',{data:{snapshot,result:null},headers:{Authorization:'Bearer '+credential2,'X-Node-Manager-Protocol':'1'}});assert(secondPoll.ok())
  await page.reload();await instances.getByTestId('instance-12345').waitFor();await page.getByTestId(`node-manage-${second.id}`).click()
  assert.equal(await detail.getAttribute('data-node-id'),String(id));assert.equal(await nodePanel.getByTestId('node-maintenance').getAttribute('data-node-id'),String(second.id))
  await nodePanel.locator('summary').filter({hasText:'紧急撤销授权'}).click();await nodePanel.getByRole('button',{name:'撤销管理绑定',exact:true}).click()
  assert((await decision().textContent()).includes('东京服务节点'))
  const revoked=page.waitForResponse(response=>response.request().method()==='DELETE'&&new URL(response.url()).pathname===`/api/management/${second.id}`)
  await accept();assert((await revoked).ok());assert.equal((await api(`/api/management/${id}`)).bound,true)
  await nodePanel.getByRole('button',{name:'安装并接入此节点',exact:true}).click();await nodePanel.getByRole('button',{name:'复制命令',exact:true}).waitFor()
  await nodePanel.getByRole('button',{name:'收起',exact:true}).click();await nodePanel.waitFor({state:'detached'})
  await page.setViewportSize({width:375,height:812});await fits();await add.click();await fits();await page.screenshot({path:'/tmp/node-manager-service-form-mobile.png'})
  await editor.getByRole('button',{name:'取消',exact:true}).click();await page.getByTitle('切换主题',{exact:true}).click()
  await fits();await page.screenshot({path:'/tmp/node-manager-service-overview-mobile-dark.png',fullPage:true})
  await page.waitForTimeout(300);const idleReads=reads;await page.waitForTimeout(6200);assert.equal(reads,idleReads)
  assert.equal(await page.getByRole('dialog').count(),0);assert.deepEqual(errors,[]);assert.deepEqual(downloads,[])
  console.log('PASS service browser: inline actions and confirmations; instance with PM and PM-first deletion; optional names; compact SB rows and share links; selected-instance configuration/copy; edit toggle and stale draft guards; independent node management; protocol tasks; core controls above instances; mobile layouts and idle reads')
} finally {
  if(browser)await browser.close()
  hub.kill('SIGTERM');if(hub.exitCode===null)await new Promise(resolve=>hub.once('exit',resolve))
  await rm(directory,{recursive:true,force:true})
}
