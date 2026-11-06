// Opt-in lifecycle UI acceptance against a disposable real hub. Status fixtures
// cover recovery/progress branches; capability negotiation and initial submissions
// use the actual API. Agent task execution is covered by native/agent tests.
import { spawn } from 'node:child_process'
import { mkdtemp, rm } from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
import { createRequire } from 'node:module'
import assert from 'node:assert/strict'
const require=createRequire(process.env.PLAYWRIGHT_PACKAGE??import.meta.url)
const {chromium}=require('playwright')
const binary=process.argv[2]
if(!binary)throw new Error('Usage: node management-lifecycle.browser.mjs /path/to/monitor-hub')
const directory=await mkdtemp(path.join(os.tmpdir(),'nm-lifecycle-browser-'))
const base='http://127.0.0.1:19917'
const hub=spawn(binary,['--listen','127.0.0.1:19917','--db',directory+'/hub.db','--themes',directory+'/themes','--site','https://panel.example.com'])
let logs='',browser
hub.stdout.on('data',data=>logs+=data);hub.stderr.on('data',data=>logs+=data)
try {
  let password
  for(let i=0;i<100;i++){password=logs.match(/Emergency password: ([^\r\n]+)/)?.[1].trim();if(password)break;if(hub.exitCode!==null)throw new Error('Test hub exited');await new Promise(resolve=>setTimeout(resolve,100))}
  assert(password)
  browser=await chromium.launch({headless:true,...(process.env.CHROME_BINARY?{executablePath:process.env.CHROME_BINARY}:{})})
  const context=await browser.newContext({viewport:{width:1280,height:900}}),page=await context.newPage(),errors=[]
  page.on('pageerror',error=>errors.push(error.message))
  await page.goto(base+'/admin/manage');await page.getByLabel('应急密码').fill(password)
  await page.getByRole('button',{name:'登录',exact:true}).click();await page.waitForURL('**/admin/nodes')
  async function api(route,data,method=data===undefined?'GET':'POST'){
    const response=await context.request.fetch(base+route,{method,data,headers:{Origin:base,'Sec-Fetch-Site':'same-origin'}})
    assert(response.ok(),`${route}: HTTP ${response.status()}`);return response.json()
  }
  const core={version:'6.0.0',sha256:'a'.repeat(64),previous_version:null,candidates:[]}
  const emptyCore={version:null,sha256:null,previous_version:null,candidates:[]}
  const policy=(port,name)=>({port,name,quota_gb:0,quota_mode:'in_out',reset_day:0,expiry_date:'',egress_mbps:0})
  const template={installed:true,blocked:false,revision:'a'.repeat(64),core,resources:[{name:'Lifecycle Snell',port:12345,public_port:12345,host:'203.0.113.60',mode:'default',psk:'s'.repeat(32)}],runtime:{'12345':'active'},modules:{
    sb:{installed:true,blocked:false,revision:'b'.repeat(64),core:{...core,version:'1.12.0'},resources:[{name:'SB retained',port:34567,public_port:34567,host:'203.0.113.60',protocol:'ss2022',sni:'',psk:'AQEBAQEBAQEBAQEBAQEBAQ=='}],runtime:{'34567':'active'}},
    fw:{installed:true,blocked:false,revision:'c'.repeat(64),core:{...core,version:'1.0.0'},resources:[{name:'FW retained',port:23456,target_host:'192.0.2.1',target_port:23456}],runtime:{'23456':'active'}},
    pm:{installed:true,blocked:false,revision:'d'.repeat(64),core:emptyCore,resources:[policy(12345,'Snell policy'),policy(23456,'FW policy')],runtime:{}}
  },port_monitor:{read_only:true,status:'not_installed',rows:[]}}
  await api('/api/management',{enabled:true})
  async function createManaged(name,credential,supported=true,empty=false){
    const {id}=await api('/api/nodes',{name,public:false})
    const enrollment=await api(`/api/management/${id}/enrollment`,{})
    await api('/api/management-agent/v1/enroll',{node:id,enrollment:enrollment.enrollment,credential})
    const snapshot=structuredClone(template)
    if(empty){snapshot.resources=[];snapshot.runtime={};for(const state of Object.values(snapshot.modules)){state.resources=[];state.runtime={}}}
    const node={id,name,credential,supported,snapshot}
    await poll(node);return node
  }
  async function poll(node,result=null,lifecycleResult){
    const response=await context.request.post(base+'/api/management-agent/v1/poll',{data:{snapshot:node.snapshot,result,...(lifecycleResult?{lifecycle_result:lifecycleResult}:{})},headers:{Authorization:'Bearer '+node.credential,...(node.supported?{'X-Node-Manager-Protocol':'1'}:{})}})
    if(!node.supported){assert.equal(response.status(),409,'another protocol must exchange nothing');return null}
    assert(response.ok(),`agent poll: HTTP ${response.status()}`);return response.json()
  }
  const single=await createManaged('单模块生命周期','a'.repeat(64)),all=await createManaged('整节点生命周期','b'.repeat(64)),legacy=await createManaged('旧组件兼容','c'.repeat(64),true,true),lost=await createManaged('请求回包丢失','d'.repeat(64))
  legacy.supported=false;await poll(legacy)
  assert.equal((await api(`/api/management/${single.id}`)).compatible,true)
  assert.equal((await api(`/api/management/${legacy.id}`)).compatible,false)
  const fixtures=new Map()
  await page.route(/\/api\/management\/\d+$/,route=>{
    const id=Number(new URL(route.request().url()).pathname.split('/').at(-1)),fixture=fixtures.get(id)
    return fixture?route.fulfill({status:200,contentType:'application/json',body:JSON.stringify(fixture)}):route.continue()
  })
  await page.goto(base+'/admin/manage')
  const maintenance=page.getByTestId('node-maintenance'),moduleMaintenance=page.getByTestId('module-maintenance'),instances=page.getByTestId('instance-config'),progress=maintenance.getByTestId('lifecycle-progress')
  const panel=page.getByRole('dialog')
  async function closePanel(){if(await panel.count()){await panel.getByRole('button',{name:'关闭',exact:true}).click();await panel.waitFor({state:'detached'})}}
  async function completedHistory(message){
    const history=maintenance.getByTestId('operation-history')
    await history.waitFor();await progress.waitFor({state:'detached'})
    assert.equal(await progress.count(),0,'completed uninstall remained in the current progress area')
    assert.equal(await history.evaluate(element=>element.open),false,'completed history was expanded by default')
    await history.locator('summary').click()
    await history.getByText(message,{exact:true}).waitFor()
    await history.locator('summary').click()
  }
  async function select(node,module='snell',kind='node'){
    await closePanel()
    const card=page.getByTestId(`node-card-${node.id}`),entry=card.getByTestId(`node-module-${node.id}-${module}`)
    if(await entry.count()){
      await entry.click()
      await page.waitForFunction(id=>document.querySelector('[data-testid="node-detail"]')?.getAttribute('data-node-id')===String(id),node.id)
    }
    if(kind==='module')await page.getByTestId('node-detail').getByRole('button',{name:'模块维护',exact:true}).click()
    else if(kind==='node')await page.getByTestId(`node-manage-${node.id}`).click()
  }
  async function refresh(){await page.getByRole('button',{name:'刷新此节点',exact:true}).click()}
  const revisions=snapshot=>({snell:snapshot.revision,sb:snapshot.modules.sb.revision,fw:snapshot.modules.fw.revision,pm:snapshot.modules.pm.revision})
  async function submit(button,node,scope,resume=false,snapshot=node.snapshot,attempt){
    page.once('dialog',async dialog=>{assert(dialog.message().includes(node.name),'lifecycle confirmation omitted target node');await dialog.accept()})
    const posted=page.waitForResponse(response=>response.request().method()==='POST'&&new URL(response.url()).pathname===`/api/management/${node.id}/lifecycle`)
    await button.click();const response=await posted;assert(response.ok(),`lifecycle submit: HTTP ${response.status()}`)
    const request=response.request().postDataJSON()
    assert(/^[0-9a-f]{32}$/.test(request.id));assert.equal(request.scope,scope);assert.equal(request.resume,resume)
    assert.deepEqual(request.revisions,revisions(snapshot))
    if(resume)assert.equal(request.attempt,attempt,'resume did not carry the current attempt')
    return request
  }
  let reads=0
  page.on('request',request=>{if(request.method()==='GET'&&/^\/api\/management(?:\/\d+)?$/.test(new URL(request.url()).pathname))reads++})
  async function assertIdle(message){await page.waitForTimeout(300);const previous=reads;await page.waitForTimeout(6200);assert.equal(reads,previous,message)}
  // A component on another protocol reports presence only: nothing can be submitted to it.
  await select(legacy)
  assert(await maintenance.getByRole('button',{name:'全部卸载此节点',exact:true}).isDisabled())
  assert(await page.getByRole('alert').filter({hasText:'管理组件与后台版本不一致'}).isVisible())
  await select(legacy,'snell','module');assert.equal(await moduleMaintenance.getByRole('button',{name:'卸载 Snell',exact:true}).isEnabled(),false)
  // The Hub accepts a destructive request but its response never reaches the
  // browser. Read back the real durable job rather than suggesting a fresh retry.
  await poll(lost);await select(lost)
  const lostRoute=`**/api/management/${lost.id}/lifecycle`
  let lostRequest
  await page.route(lostRoute,async route=>{
    lostRequest=route.request().postDataJSON()
    const accepted=await route.fetch();assert(accepted.ok())
    await route.abort('connectionreset')
  })
  page.once('dialog',dialog=>dialog.accept())
  await maintenance.getByRole('button',{name:'全部卸载此节点',exact:true}).click()
  await page.getByText(/卸载请求结果未确认，已尝试读取节点进度；请核实结果，勿重复提交/).waitFor()
  await progress.getByText('正在卸载 · 此节点全部受管服务',{exact:true}).waitFor()
  assert.equal((await api(`/api/management/${lost.id}`)).lifecycle.id,lostRequest.id)
  assert(await maintenance.getByRole('button',{name:'全部卸载此节点',exact:true}).isDisabled())
  await page.unroute(lostRoute)
  await poll(lost,null,{id:lostRequest.id,attempt:1,status:'paused',stage:'preflight',message:'lifecycle_preflight'})
  await refresh();await progress.getByText('卸载已暂停 · 此节点全部受管服务',{exact:true}).waitFor()
  // Non-empty module removal uses the real lifecycle API and all four revisions.
  await select(single,'snell','module')
  assert(await moduleMaintenance.getByRole('button',{name:'卸载 Snell',exact:true}).isEnabled(),'non-empty module could not use cascade lifecycle')
  await moduleMaintenance.getByText('卸载会先清理关联受管 PM 规则，再删除此模块的实例和核心；其他模块保留。',{exact:true}).waitFor()
  const job=await submit(moduleMaintenance.getByRole('button',{name:'卸载 Snell',exact:true}),single,'snell')
  await panel.getByRole('button',{name:'查看进度',exact:true}).click();await progress.waitFor();const actual=await api(`/api/management/${single.id}`);assert.equal(actual.lifecycle.id,job.id)
  const running={...actual,online:true,lifecycle:{...actual.lifecycle,status:'running',stage:'delete_related_pm',message:'fixture running',attempt:1}}
  fixtures.set(single.id,running);await refresh();await progress.getByText('fixture running',{exact:true}).waitFor()
  await closePanel()
  assert(await instances.getByRole('button',{name:'新增实例',exact:true}).isDisabled())
  assert(await instances.getByTestId('instance-12345').getByRole('button',{name:'显示配置',exact:true}).isDisabled())
  await page.getByTestId(`node-manage-${single.id}`).click()
  assert(await maintenance.getByRole('button',{name:'全部卸载此节点',exact:true}).isDisabled())
  const pausedSnapshot=structuredClone(actual.snapshot);pausedSnapshot.modules.pm.resources=pausedSnapshot.modules.pm.resources.filter(rule=>rule.port!==12345);pausedSnapshot.modules.pm.revision='e'.repeat(64)
  const paused={...running,snapshot:pausedSnapshot,lifecycle:{...running.lifecycle,status:'paused',stage:'delete_snell',message:'fixture paused after PM cleanup',attempt:3}}
  fixtures.set(single.id,paused);await refresh();await progress.getByText('fixture paused after PM cleanup',{exact:true}).waitFor()
  assert(await progress.getByRole('button',{name:'继续卸载',exact:true}).isEnabled());await assertIdle('paused lifecycle kept polling')
  fixtures.set(single.id,{...paused,online:false,lifecycle:{...paused.lifecycle,status:'interrupted',underlying_status:'running',message:'fixture offline while running'}})
  await refresh();await progress.getByText('fixture offline while running',{exact:true}).waitFor()
  assert.equal(await progress.getByRole('button',{name:'继续卸载',exact:true}).count(),0,'running interruption incorrectly offered resume')
  assert(await maintenance.getByRole('button',{name:'全部卸载此节点',exact:true}).isDisabled())
  await maintenance.getByText('节点离线，无法执行卸载；已有请求保持未确认，恢复在线后继续。',{exact:true}).waitFor()
  fixtures.set(single.id,{...paused,online:false,lifecycle:{...paused.lifecycle,status:'interrupted',underlying_status:'paused',message:'fixture offline while paused'}})
  await refresh();await progress.getByText('fixture offline while paused',{exact:true}).waitFor();assert(await progress.getByRole('button',{name:'继续卸载',exact:true}).isDisabled())
  fixtures.set(single.id,paused);await refresh();await progress.getByText('fixture paused after PM cleanup',{exact:true}).waitFor()
  // Resume state is a UI fixture; native tests exercise the underlying recovery.
  const resumeRoute=`**/api/management/${single.id}/lifecycle`
  await page.route(resumeRoute,route=>{fixtures.set(single.id,{...paused,lifecycle:{...paused.lifecycle,status:'running',attempt:4,message:'fixture resumed'}});return route.fulfill({status:200,contentType:'application/json',body:JSON.stringify({id:job.id})})})
  const resumed=await submit(progress.getByRole('button',{name:'继续卸载',exact:true}),single,'snell',true,pausedSnapshot,3)
  assert.equal(resumed.id,job.id);await page.unroute(resumeRoute);await progress.getByText('fixture resumed',{exact:true}).waitFor()
  const singleDone=structuredClone(pausedSnapshot);singleDone.installed=false;singleDone.resources=[];singleDone.runtime={};singleDone.core=structuredClone(emptyCore);singleDone.revision='f'.repeat(64)
  fixtures.set(single.id,{...running,snapshot:singleDone,lifecycle:{...running.lifecycle,status:'succeeded',stage:'completed',attempt:4,message:'fixture module completed'}})
  await refresh();await completedHistory('fixture module completed')
  await closePanel()
  assert.equal(await page.getByTestId('node-detail').getByTestId('lifecycle-summary').isVisible(),false,'old uninstall result leaked into configuration')
  await select(single,'sb');await closePanel();await instances.getByTestId('instance-34567').waitFor()
  await select(single,'fw');await closePanel();await instances.getByTestId('instance-23456').waitFor()
  // Whole-node cleanup stays locked even after credentials have been removed.
  await poll(all);await select(all);await page.clock.install()
  const whole=await submit(maintenance.getByRole('button',{name:'全部卸载此节点',exact:true}),all,'all')
  await progress.waitFor();const allActual=await api(`/api/management/${all.id}`);assert.equal(allActual.lifecycle.id,whole.id)
  const wholeRunning={...allActual,online:true,lifecycle:{...allActual.lifecycle,status:'running',stage:'delete_modules',message:'fixture whole-node running',attempt:1}}
  fixtures.set(all.id,wholeRunning);await refresh();await progress.getByText('fixture whole-node running',{exact:true}).waitFor()
  await page.clock.fastForward(121000)
  await panel.getByText('自动确认已达两分钟，结果尚未确认。请手动刷新配置查看结果，勿重复提交。',{exact:true}).waitFor()
  await assertIdle('unconfirmed lifecycle exceeded automatic confirmation limit')
  for(const stage of ['cleaning','finalizing']){
    fixtures.set(all.id,{...wholeRunning,bound:false,lifecycle:{...wholeRunning.lifecycle,status:stage,stage,message:`fixture ${stage}`}})
    await refresh();await progress.getByText(`fixture ${stage}`,{exact:true}).waitFor()
    assert(await maintenance.getByRole('button',{name:'安装并接入此节点',exact:true}).isDisabled(),`${stage} allowed re-enrollment`)
    assert.equal(await progress.getByRole('button',{name:'继续卸载',exact:true}).count(),0)
  }
  // Completion arrives through the existing confirmation watcher after ordinary
  // credentials are revoked. It must not be relabelled as an unfinished install.
  fixtures.set(all.id,{...wholeRunning,bound:false,online:false,lifecycle:{...wholeRunning.lifecycle,status:'succeeded',stage:'finished',message:'fixture watched completion'}})
  await page.clock.fastForward(3100)
  await completedHistory('fixture watched completion')
  assert.equal(await page.getByText('管理绑定尚未确认，请安装完成后手动刷新配置。',{exact:true}).count(),0,'successful uninstall displayed an unfinished-install warning')
  await assertIdle('confirmed whole-node cleanup kept polling')
  fixtures.set(all.id,{...wholeRunning,bound:false,online:false,lifecycle:{...wholeRunning.lifecycle,status:'revoked',stage:'revoked',message:'fixture authorization revoked'}})
  await refresh();await progress.getByText('fixture authorization revoked',{exact:true}).waitFor()
  await progress.getByText('恢复备份后原卸载授权已失效 · 此节点全部受管服务',{exact:true}).waitFor()
  assert.equal(await progress.getByText('卸载已确认完成 · 此节点全部受管服务',{exact:true}).count(),0,'revocation was displayed as successful cleanup')
  fixtures.set(all.id,{...wholeRunning,bound:false,online:false,lifecycle:{...wholeRunning.lifecycle,status:'succeeded',stage:'completed',message:'fixture whole-node completed'}})
  await refresh();await completedHistory('fixture whole-node completed')
  await maintenance.getByTestId('operation-history').locator('summary').waitFor()
  assert(await maintenance.getByRole('button',{name:'安装并接入此节点',exact:true}).isEnabled())
  await assertIdle('completed lifecycle kept polling')
  assert.deepEqual(errors,[])
  // A node that will never answer: the way out asks for its exact name.
  fixtures.set(all.id,{...wholeRunning,online:false,abandonable:true,component_current:false,lifecycle:{...wholeRunning.lifecycle,status:'interrupted',underlying_status:'running',message:'fixture dead node'}})
  await refresh();await page.getByTestId('component-outdated').waitFor()
  let abandoned
  await page.route(`**/api/management/${all.id}/abandon`,route=>{abandoned=route.request().postDataJSON();fixtures.delete(all.id);return route.fulfill({status:200,contentType:'application/json',body:'{"message":"ok"}'})})
  await maintenance.getByTestId('abandon-node').locator('summary').click()
  page.once('dialog',dialog=>dialog.dismiss())
  await page.getByRole('button',{name:'确认节点已失效'}).click();await page.waitForTimeout(100)
  assert.equal(abandoned,undefined)
  page.once('dialog',dialog=>dialog.accept(all.name))
  await page.getByRole('button',{name:'确认节点已失效'}).click()
  for(let i=0;i<50&&!abandoned;i++)await page.waitForTimeout(50)
  assert.deepEqual(abandoned,{name:all.name})
  console.log('PASS lifecycle browser: real protocol check and scope/revision submissions; other-protocol node blocked; fixture running/paused/offline/resume-attempt and cleaning/finalizing/revoked/succeeded UI; active write/export/enrollment guards; bounded polling; unrelated modules retained')
} finally {
  if(browser)await browser.close()
  hub.kill('SIGTERM');if(hub.exitCode===null)await new Promise(resolve=>hub.once('exit',resolve))
  await rm(directory,{recursive:true,force:true})
}
