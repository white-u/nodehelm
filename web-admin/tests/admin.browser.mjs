// Full-page module wiring against a disposable local Hub; no VPS is contacted.
import { spawn } from 'node:child_process'
import { mkdtemp, rm } from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
import { createRequire } from 'node:module'
import assert from 'node:assert/strict'
const require=createRequire(process.env.PLAYWRIGHT_PACKAGE??import.meta.url)
const {chromium}=require('playwright')
const binary=process.argv[2]
if(!binary)throw new Error('Usage: node admin.browser.mjs /path/to/monitor-hub')
const directory=await mkdtemp(path.join(os.tmpdir(),'nm-admin-browser-')),base='http://127.0.0.1:19918'
const hub=spawn(binary,['--listen','127.0.0.1:19918','--db',directory+'/hub.db','--themes',directory+'/themes','--site','https://panel.example.com'])
let logs='',browser
hub.stdout.on('data',data=>logs+=data);hub.stderr.on('data',data=>logs+=data)
try {
  let password
  for(let i=0;i<100;i++){password=logs.match(/Emergency password: ([^\r\n]+)/)?.[1].trim();if(password)break;if(hub.exitCode!==null)throw new Error('Test Hub exited');await new Promise(resolve=>setTimeout(resolve,100))}
  assert(password)
  browser=await chromium.launch({headless:true,...(process.env.CHROME_BINARY?{executablePath:process.env.CHROME_BINARY}:{})})
  const context=await browser.newContext({viewport:{width:1280,height:900}}),page=await context.newPage(),errors=[]
  page.on('pageerror',error=>errors.push(error.message))
  // Version discovery is external to page wiring; all CRUD below uses real APIs.
  await page.route('**/api/version',route=>route.fulfill({status:200,contentType:'application/json',body:JSON.stringify({hub:'1.1.1',hub_latest:'',agent_latest:'',notice:false})}))
  // Existing admin Field labels are display labels; scope the input to that field.
  function field(scope,label){return scope.locator('label').filter({hasText:new RegExp('^'+label.replace(/[.*+?^${}()|[\]\\]/g,'\\$&')+'$')}).locator('..').locator('input').first()}
  async function api(route){const response=await context.request.get(base+route);assert(response.ok(),route);return response.json()}
  async function section(label,route){await page.locator('main nav').getByRole('button',{name:label,exact:true}).click();await page.waitForURL(base+route);await page.locator('main nav').getByRole('button',{name:label,exact:true}).getAttribute('aria-current').then(value=>assert.equal(value,'page'))}
  await page.goto(base+'/admin/nodes');await page.getByLabel('应急密码').fill(password)
  await page.getByRole('button',{name:'登录',exact:true}).click();await page.waitForURL(base+'/admin/nodes')
  await page.getByRole('button',{name:'添加节点',exact:true}).click()
  let dialog=page.getByRole('dialog');await field(dialog,'名称').fill('模块迁移节点')
  await dialog.getByRole('button',{name:'添加',exact:true}).click()
  await dialog.waitFor({state:'detached'})
  const createdRow=page.getByRole('row').filter({hasText:'模块迁移节点'})
  await createdRow.waitFor();await createdRow.getByRole('button',{name:'安装 Agent',exact:true}).click()
  await page.getByRole('dialog').getByRole('heading',{name:'模块迁移节点',exact:true}).waitFor()
  await page.keyboard.press('Escape')
  await page.getByRole('row').filter({hasText:'模块迁移节点'}).getByRole('button',{name:'编辑节点',exact:true}).click()
  dialog=page.getByRole('dialog');await field(dialog,'名称').fill('编辑后的节点')
  await dialog.getByRole('button',{name:'保存',exact:true}).click();await dialog.waitFor({state:'detached'})
  let nodes=(await api('/api/nodes')).nodes;assert.equal(nodes.length,1);assert.equal(nodes[0].name,'编辑后的节点')
  await page.getByLabel('搜索节点',{exact:true}).fill('不存在的名称')
  assert.equal(await page.getByRole('row').filter({hasText:'编辑后的节点'}).count(),0)
  await page.getByLabel('搜索节点',{exact:true}).fill('')
  await page.getByRole('row').filter({hasText:'编辑后的节点'}).getByRole('button',{name:'续费设置',exact:true}).click()
  await field(page.getByRole('dialog'),'价格').waitFor();await page.keyboard.press('Escape')
  await section('延迟','/admin/ping');await page.getByRole('button',{name:'添加监控',exact:true}).click()
  dialog=page.getByRole('dialog');await field(dialog,'名称').fill('模块探测')
  await field(dialog,'目标地址').fill('example.com:443')
  await dialog.getByRole('button',{name:'保存',exact:true}).click();await dialog.waitFor({state:'detached'})
  const probes=(await api('/api/ping-tasks')).tasks;assert.equal(probes.length,1);assert.equal(probes[0].name,'模块探测');assert(probes[0].nodes.includes(nodes[0].id))
  await page.getByRole('button',{name:'按节点',exact:true}).click();await page.getByRole('row').filter({hasText:'编辑后的节点'}).waitFor()
  await section('通知','/admin/notify');await page.getByRole('heading',{name:'通知渠道',exact:true}).waitFor()
  assert.equal(await field(page,'PM 流量阈值（%）').inputValue(),'50,80,100')
  await field(page,'离线宽限期（分钟）').fill('5');await field(page,'PM 流量阈值（%）').fill('90, 60')
  await page.getByRole('button',{name:'保存事件设置',exact:true}).click()
  await page.waitForFunction(async()=>{const s=await(await fetch('/api/settings')).json();return Number(s.notify_grace)===5&&s.notify_pm_thresholds==='60,90,100'})
  await section('设置','/admin/settings');await field(page,'站点名称').fill('模块化后台')
  await page.getByRole('button',{name:'保存站点设置',exact:true}).click()
  await page.waitForFunction(async()=>(await(await fetch('/api/settings')).json()).site_name==='模块化后台')
  await section('安全','/admin/security');await page.getByRole('heading',{name:'GitHub 单点登录',exact:true}).waitFor()
  await page.getByRole('heading',{name:'登录会话',exact:true}).waitFor();await page.getByText('当前设备',{exact:true}).waitFor()
  await section('数据','/admin/data');await page.getByRole('heading',{name:'数据库',exact:true}).waitFor()
  await page.getByRole('heading',{name:'备份',exact:true}).waitFor()
  await section('主题','/admin/themes');await page.getByRole('heading',{name:'安装主题',exact:true}).waitFor()
  // A bound node on an older component is listed with the command to run on it.
  const managementRoute=route=>route.fulfill({status:200,contentType:'application/json',body:JSON.stringify({enabled:true,site:'https://panel.example.com',bound:1,outdated:[nodes[0].id],offline:[nodes[0].id]})})
  await page.route('**/api/management',managementRoute)
  await section('更新','/admin/update');await page.getByRole('heading',{name:'后台',exact:true}).waitFor()
  // A development hub carries no agent, and says so instead of offering a command that would fail.
  await page.getByText('此后台构建不含 agent',{exact:true}).waitFor()
  await page.getByRole('heading',{name:'管理组件',exact:true}).waitFor();await page.getByText('1 台待升级',{exact:true}).waitFor();await page.locator('[title="管理组件未连接"]').waitFor()
  await page.getByText('sudo python3 /opt/node-manager/install.py upgrade',{exact:true}).waitFor();await page.getByText(nodes[0].name,{exact:true}).waitFor()
  await page.unroute('**/api/management',managementRoute)
  await section('服务管理','/admin/manage');await page.getByRole('heading',{name:'节点服务管理',exact:true}).waitFor()
  await page.getByTestId(`node-card-${nodes[0].id}`).waitFor()
  await section('节点','/admin/nodes');await page.reload()
  await page.getByRole('row').filter({hasText:'编辑后的节点'}).waitFor()
  await page.getByRole('row').filter({hasText:'编辑后的节点'}).getByRole('button',{name:'删除节点',exact:true}).click()
  await page.getByRole('dialog').getByRole('button',{name:'删除节点',exact:true}).click()
  await page.waitForFunction(async()=>(await(await fetch('/api/nodes')).json()).nodes.length===0)
  await page.getByTitle('退出登录',{exact:true}).click();await page.waitForURL(base+'/')
  assert.equal((await api('/api/me')).authed,false)
  await page.goto(base+'/admin/nodes');await page.getByLabel('应急密码').waitFor()
  assert.deepEqual(errors,[])
  console.log('PASS admin modules: real login/logout, node create/edit/search/billing/delete, probe creation/node view, notification and general settings saves, security/session/data/themes/update/service routes and node-page reload; no browser errors')
} finally {
  if(browser)await browser.close()
  hub.kill('SIGTERM');if(hub.exitCode===null)await new Promise(resolve=>hub.once('exit',resolve))
  await rm(directory,{recursive:true,force:true})
}
