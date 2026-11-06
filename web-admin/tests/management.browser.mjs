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
  const instances=page.getByTestId('instance-config'),detail=page.getByTestId('node-detail'),dialog=page.getByRole('dialog')
  let reads=0
  page.on('request',request=>{if(request.method()==='GET'&&/^\/api\/management(?:\/\d+)?$/.test(new URL(request.url()).pathname))reads++})
  const refresh=()=>page.getByRole('button',{name:'刷新此节点',exact:true}).click()
  async function select(module,node=id){
    await page.getByTestId(`node-module-${node}-${module}`).click()
    await page.waitForFunction(({node,module})=>document.querySelector(`[data-testid="node-module-${node}-${module}"]`)?.getAttribute('aria-pressed')==='true',{node,module})
  }
  async function close(){await dialog.getByRole('button',{name:'关闭',exact:true}).click();await dialog.waitFor({state:'detached'})}
  async function confirmSubmit(button){
    page.once('dialog',prompt=>prompt.accept())
    const posted=page.waitForResponse(response=>response.request().method()==='POST'&&response.url().endsWith('/operations'))
    await button.click();const response=await posted;assert(response.ok(),await response.text());return response
  }
  async function createSubmit(){
    const posted=page.waitForResponse(response=>response.request().method()==='POST'&&response.url().endsWith('/operations'))
    await dialog.getByRole('button',{name:'保存并应用',exact:true}).click();const response=await posted;assert(response.ok(),await response.text());await dialog.waitFor({state:'detached'})
    return (await poll()).task
  }
  async function finish(task){await poll({id:task.request.id,status:'succeeded',message:'fixture applied'});await refresh()}
  async function assertFits(){
    assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth+1),'page has horizontal overflow')
    if(await dialog.count())for(const control of [dialog,dialog.getByRole('button',{name:await dialog.getByRole('button',{name:'关闭面板',exact:true}).count()?'关闭面板':'关闭',exact:true}),...(await dialog.getByRole('button',{name:'保存并应用',exact:true}).count()?[dialog.getByRole('button',{name:'保存并应用',exact:true})]:[])]){
      const box=await control.boundingBox(),viewport=page.viewportSize();assert(box&&box.x>=-1&&box.y>=-1&&box.x+box.width<=viewport.width+1&&box.y+box.height<=viewport.height+1,'drawer or its fixed actions exceeded viewport')
    }
    for(const row of await page.getByTestId(`node-card-${id}`).locator('[data-testid^="node-module-"]').all()){
      const aligned=await row.evaluate(element=>{const [name,state]=element.querySelectorAll('span');const a=name.getBoundingClientRect(),b=state.getBoundingClientRect();return Math.abs(a.y+a.height/2-b.y-b.height/2)<2});assert(aligned,'module label and state wrapped onto separate lines')
    }
  }
  await instances.getByRole('button',{name:'新增实例',exact:true}).waitFor()
  assert.equal(await page.getByRole('tab').count(),0,'configuration/management tabs remain')
  await detail.getByRole('button',{name:'模块维护',exact:true}).click()
  await dialog.getByTestId('core-maintenance').waitFor()
  assert.equal(await dialog.getByRole('button',{name:'全部卸载此节点',exact:true}).count(),0)
  assert.equal(await dialog.getByText('紧急撤销授权',{exact:true}).count(),0,'node authorization controls leaked into module maintenance')
  await close()
  await page.getByTestId(`node-manage-${id}`).click()
  await dialog.getByTestId('node-maintenance').waitFor()
  assert.equal(await dialog.getByTestId('core-maintenance').count(),0,'core controls leaked into node management')
  await dialog.locator('summary').filter({hasText:'紧急撤销授权'}).click()
  await dialog.getByRole('button',{name:'撤销管理绑定',exact:true}).waitFor()
  await close()

  // An abandoned drawer preserves dirty data when dismissal is rejected.
  const add=instances.getByRole('button',{name:'新增实例',exact:true})
  await add.click();await dialog.getByLabel('监听端口',{exact:true}).fill('12345')
  assert.equal(await dialog.locator('form input:visible').count(),2)
  assert.equal(await dialog.getByTestId('advanced-config').getAttribute('open'),null)
  assert.equal(await dialog.getByLabel('客户端访问端口',{exact:true}).inputValue(),'12345')
  page.once('dialog',prompt=>prompt.dismiss());await page.keyboard.press('Escape')
  assert(await dialog.isVisible());assert.equal(await dialog.getByLabel('监听端口',{exact:true}).inputValue(),'12345')
  await assertFits()
  page.once('dialog',prompt=>prompt.accept());await page.keyboard.press('Escape');await dialog.waitFor({state:'detached'})
  await page.waitForFunction(element=>document.activeElement===element,await add.elementHandle())

  await add.click();await dialog.getByLabel('监听端口',{exact:true}).fill('12345')
  const advanced=dialog.getByTestId('advanced-config')
  await advanced.locator('summary').click();await dialog.getByLabel('客户端访问域名或 IP',{exact:true}).fill('');await advanced.locator('summary').click()
  await dialog.getByRole('button',{name:'保存并应用',exact:true}).click()
  assert.equal(await advanced.getAttribute('open'),'','invalid hidden advanced field stayed collapsed')
  await dialog.getByLabel('客户端访问域名或 IP',{exact:true}).fill('203.0.113.42')
  let task=await createSubmit()
  assert.equal(task.request.intent,'create');assert.equal(task.request.payload.port,12345);assert.equal(task.request.payload.public_port,12345);assert.equal(task.request.payload.psk,'')
  const savedSnell={...task.request.payload,psk:'s'.repeat(32)}
  snapshot.resources=[savedSnell];snapshot.runtime={'12345':'active'};snapshot.revision='b'.repeat(64)
  await finish(task);await instances.getByTestId('instance-12345').waitFor()
  snapshot.resources.push({...savedSnell,name:'Other Snell',port:12346,public_port:12346,host:'other-snell.example.com',psk:'o'.repeat(32)})
  snapshot.runtime['12346']='active';snapshot.revision='c'.repeat(64);await poll();await refresh()
  // Per-instance export stays protected, does not download, and returns focus.
  const show=instances.getByTestId('instance-12345').getByRole('button',{name:'显示配置',exact:true})
  await show.click();await page.getByTestId('client-configuration').waitFor()
  const configuration=dialog.getByLabel('客户端配置内容',{exact:true})
  assert.match(await configuration.textContent(),/203\.0\.113\.42/)
  assert.equal((await configuration.textContent()).includes('other-snell.example.com'),false,'export included another instance')
  await page.setViewportSize({width:375,height:812});await assertFits()
  await page.keyboard.press('Escape');await dialog.waitFor({state:'detached'})
  await page.waitForFunction(element=>document.activeElement===element,await show.elementHandle())
  await page.setViewportSize({width:1280,height:900})
  // A delayed export may not appear after changing module.
  let releaseExport,exportStarted
  const exportGate=new Promise(resolve=>releaseExport=resolve),started=new Promise(resolve=>exportStarted=resolve)
  const exportRoute=`**/api/management/${id}/export?*`
  await page.route(exportRoute,async route=>{const response=await route.fetch();exportStarted();await exportGate;await route.fulfill({response})})
  await show.click();await started;await select('sb');releaseExport();await page.unroute(exportRoute,{behavior:'wait'})
  assert.equal(await page.getByTestId('client-configuration').count(),0,'late export leaked across modules')

  await instances.getByRole('button',{name:'新增实例',exact:true}).click()
  assert.equal(await dialog.getByLabel('协议',{exact:true}).inputValue(),'reality')
  assert.equal(await dialog.getByLabel('监听端口',{exact:true}).inputValue(),'443')
  await dialog.getByLabel('协议',{exact:true}).selectOption('ss2022')
  await dialog.getByLabel('监听端口',{exact:true}).fill('34567')
  task=await createSubmit();assert.equal(task.request.module,'sb');assert.equal(task.request.payload.protocol,'ss2022')
  snapshot.modules.sb.resources=[{...task.request.payload,psk:'AQEBAQEBAQEBAQEBAQEBAQ=='}];snapshot.modules.sb.runtime={'34567':'active'}
  await finish(task)

  await select('fw');await instances.getByRole('button',{name:'新增转发规则',exact:true}).click()
  await dialog.getByLabel('监听端口',{exact:true}).fill('23456')
  assert.equal(await dialog.getByLabel('目标端口',{exact:true}).inputValue(),'23456')
  await dialog.getByLabel('目标 IPv4 地址',{exact:true}).fill('invalid-target')
  const rejected=page.waitForResponse(response=>response.request().method()==='POST'&&response.url().endsWith('/operations'))
  await dialog.getByRole('button',{name:'保存并应用',exact:true}).click();assert.equal((await rejected).status(),400)
  await dialog.getByRole('alert').waitFor()
  assert.equal(await dialog.getByLabel('目标 IPv4 地址',{exact:true}).inputValue(),'invalid-target')
  await dialog.getByLabel('目标 IPv4 地址',{exact:true}).fill('192.0.2.1')
  task=await createSubmit();assert.equal(task.request.module,'fw');assert.equal(task.request.payload.target_port,23456)
  snapshot.modules.fw.resources=[task.request.payload];snapshot.modules.fw.runtime={'23456':'active'}
  await finish(task)

  snapshot.modules.pm.installed=false;await poll();await select('pm');await refresh()
  await confirmSubmit(instances.getByRole('button',{name:'启用 PM',exact:true}))
  task=(await poll()).task;assert.equal(task.request.module,'pm');assert.equal(task.request.action,'install')
  snapshot.modules.pm.installed=true;await finish(task)
  await instances.getByRole('button',{name:'新增 PM 规则',exact:true}).click()
  await dialog.getByLabel('业务端口',{exact:true}).selectOption('23456')
  await dialog.getByLabel('配额（GiB，0 关闭）',{exact:true}).fill('10')
  await dialog.getByLabel('每月重置日（0 关闭）',{exact:true}).fill('31')
  await dialog.getByLabel('计费方向',{exact:true}).selectOption('out_only')
  await confirmSubmit(dialog.getByRole('button',{name:'保存并应用',exact:true}));await dialog.waitFor({state:'detached'})
  task=(await poll()).task
  assert.deepEqual(task.request.payload,{port:23456,name:'端口监控',quota_gb:10,quota_mode:'out_only',reset_day:31,expiry_date:'',egress_mbps:0})
  snapshot.modules.pm.resources=[task.request.payload];snapshot.modules.pm.revision='b'.repeat(64)
  snapshot.modules.pm.usage={'23456':{rx:12,tx:9*1073741824,sampled_at:1760000000,period_start:'2026-10-31',alert:'quota_warning'}}
  await finish(task);await instances.getByText('用量达到配额的 80%',{exact:true}).waitFor()
  await instances.getByRole('button',{name:'编辑',exact:true}).click()
  await dialog.getByLabel('配额（GiB，0 关闭）',{exact:true}).fill('20')
  snapshot.modules.pm.revision='c'.repeat(64);snapshot.modules.pm.resources[0]={...snapshot.modules.pm.resources[0],quota_gb:30}
  await poll();await refresh();await dialog.getByRole('button',{name:'重新载入最新配置',exact:true}).waitFor()
  assert.equal(await dialog.getByLabel('配额（GiB，0 关闭）',{exact:true}).inputValue(),'20')
  assert(await dialog.getByRole('button',{name:'保存并应用',exact:true}).isDisabled())
  page.once('dialog',prompt=>prompt.accept());await dialog.getByRole('button',{name:'重新载入最新配置',exact:true}).click()
  assert.equal(await dialog.getByLabel('配额（GiB，0 关闭）',{exact:true}).inputValue(),'30')
  await dialog.getByRole('button',{name:'取消',exact:true}).click();await dialog.waitFor({state:'detached'})
  await confirmSubmit(instances.getByRole('button',{name:'重置用量',exact:true}))
  task=(await poll()).task;assert.equal(task.request.action,'reset');assert.deepEqual(task.request.payload,{port:23456});await finish(task)

  await select('snell');await detail.getByRole('button',{name:'模块维护',exact:true}).click()
  await confirmSubmit(dialog.getByRole('button',{name:'升级核心',exact:true}))
  task=(await poll()).task;assert.equal(task.request.action,'core_upgrade');assert.deepEqual(task.request.payload,{version:'6.1.0'})
  await poll({id:task.request.id,status:'rolled_back',message:'private binary path',code:'core_runtime'});await refresh()
  await dialog.getByRole('status').filter({hasText:/核心或业务实例健康检查失败/}).waitFor()
  assert.equal(await page.getByText(/private binary path/).count(),0)
  await close()

  // Node management has an independent target and never changes selection.
  const second=await api('/api/nodes',{name:'东京服务节点',public:false})
  const enrollment2=await api(`/api/management/${second.id}/enrollment`,{})
  const credential2='d'.repeat(64)
  await api('/api/management-agent/v1/enroll',{node:second.id,enrollment:enrollment2.enrollment,credential:credential2})
  const secondPoll=await context.request.post(base+'/api/management-agent/v1/poll',{data:{snapshot,result:null},headers:{Authorization:'Bearer '+credential2,'X-Node-Manager-Protocol':'1'}})
  assert(secondPoll.ok());await page.reload();await instances.getByTestId('instance-12345').waitFor()
  await page.getByTestId(`node-manage-${second.id}`).click()
  assert.equal(await detail.getAttribute('data-node-id'),String(id),'opening node management changed current configuration selection')
  assert.equal(await dialog.getByTestId('node-maintenance').getAttribute('data-node-id'),String(second.id))
  await dialog.locator('summary').filter({hasText:'紧急撤销授权'}).click()
  page.once('dialog',prompt=>{assert(prompt.message().includes('东京服务节点'));return prompt.accept()})
  const revocation=page.waitForResponse(response=>response.request().method()==='DELETE'&&new URL(response.url()).pathname===`/api/management/${second.id}`)
  await dialog.getByRole('button',{name:'撤销管理绑定',exact:true}).click();assert((await revocation).ok())
  await dialog.getByRole('button',{name:'安装并接入此节点',exact:true}).waitFor()
  assert.equal((await api(`/api/management/${id}`)).bound,true,'another node lost authorization')
  await dialog.getByRole('button',{name:'安装并接入此节点',exact:true}).click()
  await dialog.getByRole('button',{name:'复制命令',exact:true}).waitFor()
  await close()
  await page.getByLabel('搜索服务器',{exact:true}).fill('东京')
  assert.equal(await page.getByTestId(`node-card-${id}`).count(),0)
  await page.getByLabel('搜索服务器',{exact:true}).fill('')
  await page.setViewportSize({width:375,height:812});await assertFits()
  await detail.getByRole('button',{name:'模块维护',exact:true}).click();await assertFits();await close()
  await add.click();await assertFits()
  await page.screenshot({path:'/tmp/node-manager-service-form-mobile.png'})
  await dialog.getByRole('button',{name:'取消',exact:true}).click()
  await page.getByTitle('切换主题',{exact:true}).click();await page.waitForFunction(()=>document.documentElement.classList.contains('dark'))
  await assertFits();await page.screenshot({path:'/tmp/node-manager-service-overview-mobile-dark.png',fullPage:true})
  await page.setViewportSize({width:1280,height:900});await page.screenshot({path:'/tmp/node-manager-service-overview-desktop-dark.png',fullPage:true})
  await page.waitForTimeout(300);const idleReads=reads;await page.waitForTimeout(6200);assert.equal(reads,idleReads,'idle service page issued background reads')
  assert.deepEqual(errors,[]);assert.deepEqual(downloads,[])
  console.log('PASS service browser: node/module action separation; independent node target; emergency revocation and enrollment; single-line module rows; guarded form drawers and focus restoration; real Snell/SB/FW/PM requests; stale PM draft protection; per-instance secret dialog isolation; core failure redaction; scoped refresh and idle polling; light/dark and mobile layouts')
} finally {
  if(browser)await browser.close()
  hub.kill('SIGTERM');if(hub.exitCode===null)await new Promise(resolve=>hub.once('exit',resolve))
  await rm(directory,{recursive:true,force:true})
}
