// Service management recovery against a disposable Hub and simulated node reports.
import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { mkdtemp, rm } from 'node:fs/promises'
import { createRequire } from 'node:module'
import os from 'node:os'
import path from 'node:path'

const { chromium } = createRequire(process.env.PLAYWRIGHT_PACKAGE ?? import.meta.url)('playwright')
const binary = process.argv[2]
if (!binary) throw new Error('Usage: node management-recovery.browser.mjs /path/to/monitor-hub')
const address = '127.0.0.1:19923'
const base = 'http://' + address
const directory = await mkdtemp(path.join(os.tmpdir(), 'nm-recovery-browser-'))
const hub = spawn(binary, ['--listen', address, '--db', directory + '/hub.db', '--themes', directory + '/themes', '--site', 'https://panel.example.com'])
let logs = ''
let browser
hub.stdout.on('data', data => logs += data)
hub.stderr.on('data', data => logs += data)

try {
  let password
  for (let i = 0; i < 100; i++) {
    password = logs.match(/Emergency password: ([^\r\n]+)/)?.[1].trim()
    if (password) break
    if (hub.exitCode !== null) throw new Error('Test Hub exited')
    await new Promise(resolve => setTimeout(resolve, 100))
  }
  assert(password)
  browser = await chromium.launch({ headless: true, ...(process.env.CHROME_BINARY ? { executablePath: process.env.CHROME_BINARY } : {}) })
  const context = await browser.newContext({ viewport: { width: 1280, height: 900 } })
  const page = await context.newPage()
  const errors = []
  page.setDefaultTimeout(12_000)
  page.on('pageerror', error => errors.push(error.message))

  const button = (scope, name) => scope.getByRole('button', { name, exact: true })
  const card = id => page.getByTestId(`node-card-${id}`)
  const nodePath = id => `/api/management/${id}`
  const panel = page.getByTestId('node-management-panel')
  const detail = page.getByTestId('node-detail')
  const confirmation = page.getByRole('region', { name: '操作确认' })
  const retry = button(page, '按原请求重试')
  const confirm = label => button(confirmation, label).click()
  const refresh = scope => button(scope, '刷新此节点').click()
  const request = (route, data, method = data === undefined ? 'GET' : 'POST') => context.request.fetch(base + route, {
    method, data, headers: { Origin: base, 'Sec-Fetch-Site': 'same-origin' },
  })
  async function api(route, data) {
    const response = await request(route, data)
    assert(response.ok(), `${route}: HTTP ${response.status()} ${await response.text()}`)
    return response.json()
  }
  function responseFor(route, method) {
    return page.waitForResponse(response => new URL(response.url()).pathname === route && response.request().method() === method)
  }
  async function inspectLayout(name) {
    await page.setViewportSize({ width: 390, height: 844 })
    assert(await detail.evaluate(element => element.scrollWidth <= element.clientWidth + 1), `${name} overflows on mobile`)
    if (process.env.MANAGEMENT_RECOVERY_SCREENSHOTS) await detail.screenshot({ path: path.join(os.tmpdir(), `nodehelm-${name}-mobile.png`) })
    await page.setViewportSize({ width: 1280, height: 900 })
    if (process.env.MANAGEMENT_RECOVERY_SCREENSHOTS) await detail.screenshot({ path: path.join(os.tmpdir(), `nodehelm-${name}-desktop.png`) })
  }

  await page.goto(base + '/admin/manage')
  await page.getByLabel('应急密码').fill(password)
  await button(page, '登录').click()
  await page.waitForURL('**/admin/nodes')
  await page.goto(base + '/admin/manage')

  // Keep the response pending to observe the button lock, then show the Hub error on the overview.
  let enablePosts = 0
  const failed = responseFor('/api/management', 'POST')
  const enable = button(page, '启用服务管理')
  const enabling = button(page, /正在启用|启用中/)
  let releaseEnable
  const enableGate = new Promise(resolve => { releaseEnable = resolve })
  await page.route('**/api/management', async route => {
    if (route.request().method() !== 'POST') return route.continue()
    enablePosts++
    await enableGate
    await route.fulfill({ status: 400, contentType: 'text/plain; charset=utf-8', body: '请先配置 HTTPS 公网地址' })
  })
  await enable.click()
  try {
    await enabling.waitFor()
    assert(await enabling.isDisabled())
    await enabling.evaluate(element => element.click())
  } finally {
    releaseEnable()
  }
  assert.equal((await failed).status(), 400)
  await page.getByRole('alert').filter({ hasText: '请先配置 HTTPS 公网地址' }).waitFor()
  assert.equal(enablePosts, 1)
  await page.unroute('**/api/management')
  await enable.click()
  await page.getByText('服务管理尚未启用，现有监控不受影响。', { exact: true }).waitFor({ state: 'detached' })

  const fresh = await api('/api/nodes', { name: '首次接入验收', public: false })
  await page.reload()
  await button(card(fresh.id), '接入管理').click()
  await button(panel, '安装并接入此节点').click()
  const command = await panel.locator('pre').textContent()
  const enrollment = command.match(/[a-f\d]{64}$/)?.[0]
  assert(enrollment)
  await api('/api/management-agent/v1/enroll', { enrollment, credential: 'a'.repeat(64), boot_id: '' })
  await refresh(panel)
  await panel.getByText('已绑定', { exact: true }).waitFor()
  const bound = await api(nodePath(fresh.id))
  assert.equal(bound.last_seen, 0)
  assert.equal(bound.abandonable, false)
  assert.equal(await panel.getByTestId('abandon-node').count(), 0)
  assert.equal((await request(nodePath(fresh.id) + '/abandon', { name: '首次接入验收' })).status(), 400)
  assert.equal((await api(nodePath(fresh.id))).bound, true)

  // An unsent revocation leaves the binding in place and reads its state before reporting.
  await panel.getByText('紧急撤销授权', { exact: true }).click()
  const revoke = () => button(panel, '撤销管理绑定')
  let deletePosts = 0
  let delivered = false
  const revokeRoute = '**' + nodePath(fresh.id)
  await page.route(revokeRoute, async route => {
    if (route.request().method() !== 'DELETE') return route.continue()
    deletePosts++
    if (delivered) assert((await route.fetch()).ok())
    await route.abort('connectionreset')
  })
  const checkedBinding = responseFor(nodePath(fresh.id), 'GET')
  await revoke().click()
  await confirm('确认操作')
  assert.equal((await (await checkedBinding).json()).bound, true)
  await panel.getByText(/撤销.*(?:未确认|待确认|仍绑定|仍有管理绑定)/).waitFor()
  assert.equal(deletePosts, 1)
  assert(await panel.getByText('已绑定', { exact: true }).isVisible())
  await refresh(panel)
  await panel.getByText('配置已刷新', { exact: true }).waitFor()
  assert.equal(deletePosts, 1)

  // The same missing response after a committed revocation is recovered without another DELETE.
  delivered = true
  await revoke().click()
  await confirm('确认操作')
  await panel.waitFor({ state: 'detached' })
  await button(card(fresh.id), '接入管理').waitFor()
  assert.equal((await api(nodePath(fresh.id))).bound, false)
  assert.equal(deletePosts, 2)
  await page.unroute(revokeRoute)

  // A submitted revocation must refresh the card even if the user closes its panel in flight.
  const replacement = (await api(nodePath(fresh.id) + '/enrollment', {})).enrollment
  await api('/api/management-agent/v1/enroll', { enrollment: replacement, credential: 'a'.repeat(64), boot_id: '' })
  await page.reload()
  const manageFresh = card(fresh.id).getByTestId(`node-manage-${fresh.id}`)
  await manageFresh.click()
  await panel.getByText('已绑定', { exact: true }).waitFor()
  await panel.getByText('紧急撤销授权', { exact: true }).click()
  let releaseDelete
  const deleteGate = new Promise(resolve => { releaseDelete = resolve })
  await page.route(revokeRoute, async route => {
    if (route.request().method() !== 'DELETE') return route.continue()
    await deleteGate
    assert((await route.fetch()).ok())
    await route.abort('connectionreset')
  })
  await revoke().click()
  const sent = page.waitForRequest(request => request.method() === 'DELETE' && new URL(request.url()).pathname === nodePath(fresh.id))
  await confirm('确认操作')
  await sent
  try {
    await manageFresh.click()
    await panel.waitFor({ state: 'detached' })
  } finally {
    releaseDelete()
  }
  await button(card(fresh.id), '接入管理').waitFor()
  assert.equal((await api(nodePath(fresh.id))).bound, false)
  await page.unroute(revokeRoute)

  const core = { version: '6.0.0', sha256: 'a'.repeat(64), previous_version: null, candidates: [] }
  const snapshot = {
    installed: true, blocked: false, revision: 'a'.repeat(64), core, resources: [], runtime: {}, modules: {
      sb: { installed: false, blocked: false, revision: 'b'.repeat(64), resources: [], runtime: {} },
      fw: {
        installed: true, blocked: false, revision: 'c'.repeat(64), core,
        resources: [
          { name: '第一条转发', port: 23456, target_host: '203.0.113.10', target_port: 443 },
          { name: '第二条转发', port: 23457, target_host: '203.0.113.11', target_port: 443 },
        ],
        runtime: { 23456: 'active', 23457: 'active' },
      },
      pm: {
        installed: true, blocked: false, revision: 'd'.repeat(64), core: { version: null, sha256: null, previous_version: null, candidates: [] },
        resources: [{ name: '流量验收', port: 23456, quota_gb: 1, quota_mode: 'in_out', reset_day: 0, expiry_date: '', egress_mbps: 0 }],
        usage: { 23456: { rx: 536870912, tx: 0, sampled_at: Math.floor(Date.now() / 1000), period_start: '', alert: 'normal', ip_count: 4, ip_level: 0 } },
        runtime: {},
      },
    },
  }
  const managed = await api('/api/nodes', { name: '服务操作验收', public: false })
  const credential = 'b'.repeat(64)
  const token = (await api(nodePath(managed.id) + '/enrollment', {})).enrollment
  await api('/api/management-agent/v1/enroll', { enrollment: token, credential, boot_id: '' })
  async function poll(result = null, lifecycleResult, identity = credential) {
    const response = await context.request.post(base + '/api/management-agent/v1/poll', {
      data: { snapshot, result, ...(lifecycleResult ? { lifecycle_result: lifecycleResult } : {}) },
      headers: { Authorization: 'Bearer ' + identity, 'X-Node-Manager-Protocol': '1' },
    })
    assert(response.ok(), `agent poll: HTTP ${response.status()} ${await response.text()}`)
    return response.json()
  }
  await poll()
  await page.reload()
  const select = module => card(managed.id).getByTestId(`node-module-${managed.id}-${module}`).click()

  // Change just the connectivity of a real report to exercise an offline refresh with existing usage.
  await select('pm')
  const pmRule = detail.getByTestId('pm-rule-23456')
  await pmRule.getByText('TCP 在线 IP 4 个', { exact: true }).waitFor()
  await pmRule.getByText('正常', { exact: true }).waitFor()
  const usage = pmRule.getByRole('progressbar')
  const usageText = await usage.getAttribute('aria-valuetext')
  const statusRoute = '**' + nodePath(managed.id)
  await page.route(statusRoute, async route => {
    const response = await route.fetch()
    await route.fulfill({ response, json: { ...await response.json(), online: false } })
  })
  await refresh(detail)
  await detail.getByText(/管理组件离线，配置只读/).waitFor()
  await pmRule.getByText('待确认', { exact: true }).waitFor()
  assert.equal(await pmRule.getByText('TCP 在线 IP 4 个', { exact: true }).count(), 0)
  assert.equal(await pmRule.getByText('正常', { exact: true }).count(), 0)
  assert.equal(await usage.getAttribute('aria-valuetext'), usageText)
  await inspectLayout('pm-offline')
  await page.unroute(statusRoute)
  await refresh(detail)
  await pmRule.getByText('TCP 在线 IP 4 个', { exact: true }).waitFor()

  // Refreshing the node still reports a settings read failure near its detail.
  await page.route('**/api/management', route => route.fulfill({
    status: 503, contentType: 'text/plain; charset=utf-8', body: '管理设置暂时不可读取',
  }))
  const refreshedNode = responseFor(nodePath(managed.id), 'GET')
  await refresh(detail)
  assert((await refreshedNode).ok())
  await detail.getByRole('alert').filter({ hasText: '管理设置暂时不可读取' }).waitFor()
  await page.unroute('**/api/management')
  await refresh(detail)
  await detail.getByRole('alert').filter({ hasText: '管理设置暂时不可读取' }).waitFor({ state: 'detached' })

  const operations = nodePath(managed.id) + '/operations'
  await select('fw')
  for (const port of [23456, 23457]) {
    const row = detail.getByTestId(`instance-${port}`)
    const sibling = detail.getByTestId(`instance-${port === 23456 ? 23457 : 23456}`)
    const posted = responseFor(operations, 'POST')
    await button(row, '测试目标 TCP').click()
    assert((await posted).ok())
    await button(row, /正在测试/).waitFor()
    assert.equal(await button(sibling, /正在测试/).count(), 0)
    const { task } = await poll()
    assert.equal(task.request.payload.port, port)
    await poll({ id: task.request.id, status: 'checked', message: 'fixture check result' })
    await refresh(detail)
    await row.getByText(/目标 TCP 可达/).waitFor()
  }
  await inspectLayout('fw-check')
  const history = detail.getByTestId('operation-history')
  await history.locator('summary').click()
  for (const port of [23456, 23457]) await history.getByText(new RegExp(`端口 ${port}`)).waitFor()
  // A fresh snapshot with a different destination invalidates only that row's previous result.
  snapshot.modules.fw.resources[0].target_host = '203.0.113.12'
  snapshot.modules.fw.revision = 'e'.repeat(64)
  await poll()
  await refresh(detail)
  const changed = detail.getByTestId('instance-23456')
  await changed.getByText(/203\.0\.113\.12:443/).waitFor()
  assert.equal(await changed.getByText(/目标 TCP 可达/).count(), 0)
  assert(await detail.getByTestId('instance-23457').getByText(/目标 TCP 可达/).isVisible())

  // A module uninstall clears an agreed-to-discard draft only once the original task is observed.
  const moduleToken = (await api(nodePath(fresh.id) + '/enrollment', {})).enrollment
  await api('/api/management-agent/v1/enroll', { enrollment: moduleToken, credential: 'a'.repeat(64), boot_id: '' })
  await poll(null, undefined, 'a'.repeat(64))
  await button(page, '刷新全部').click()
  await card(fresh.id).getByTestId(`node-module-${fresh.id}-snell`).click()
  await button(detail, '新增实例').click()
  const draft = detail.getByTestId('service-instance-editor').getByLabel('名称（可选）')
  await draft.fill('临时草稿')
  await button(detail, '模块维护').click()
  const moduleLifecycle = nodePath(fresh.id) + '/lifecycle'
  const moduleRoute = '**' + moduleLifecycle
  let moduleBody
  await page.route(moduleRoute, route => {
    moduleBody = route.request().postDataJSON()
    return route.abort('connectionreset')
  })
  await button(detail, '卸载 Snell').click()
  await confirm('放弃修改')
  await confirm('确认卸载')
  await retry.waitFor()
  assert.equal(await draft.inputValue(), '临时草稿')
  assert.equal((await api(nodePath(fresh.id))).lifecycle, null)
  assert.equal(moduleBody.scope, 'snell')
  await page.unroute(moduleRoute)
  const moduleAccepted = responseFor(moduleLifecycle, 'POST')
  await retry.click()
  const moduleResponse = await moduleAccepted
  assert(moduleResponse.ok())
  assert.deepEqual(moduleResponse.request().postDataJSON(), moduleBody)
  await draft.waitFor({ state: 'detached' })

  await card(managed.id).getByTestId(`node-manage-${managed.id}`).click()
  const lifecycle = nodePath(managed.id) + '/lifecycle'
  const lifecycleRoute = '**' + lifecycle
  const progress = panel.getByTestId('lifecycle-progress')
  const uninstall = button(panel, '全部卸载此节点')
  const resume = button(progress, '继续卸载')
  const posts = []
  let acceptLifecycle = false
  let loseResponse = true
  await page.route(lifecycleRoute, async route => {
    posts.push(route.request().postDataJSON())
    if (!acceptLifecycle) return route.abort('connectionreset')
    const response = await route.fetch()
    assert(response.ok(), `lifecycle: HTTP ${response.status()} ${await response.text()}`)
    if (loseResponse) return route.abort('connectionreset')
    await route.fulfill({ response })
  })
  async function retryLifecycle(original, previousPosts) {
    await retry.waitFor()
    assert.equal(posts.length, previousPosts)
    acceptLifecycle = true
    loseResponse = false
    const resent = responseFor(lifecycle, 'POST')
    await retry.click()
    assert((await resent).ok())
    assert.deepEqual(posts.at(-1), original)
    await retry.waitFor({ state: 'detached' })
    await progress.getByText('正在卸载 · 此节点全部受管服务', { exact: true }).waitFor()
  }
  await uninstall.click()
  await confirmation.getByRole('textbox').fill('服务操作验收')
  await confirm('确认卸载')
  await retry.waitFor()
  assert(await uninstall.isDisabled())
  assert.equal((await api(nodePath(managed.id))).lifecycle, null)
  const original = posts[0]
  assert(original.id)
  await retryLifecycle(original, 1)

  async function pause(attempt) {
    await poll(null, { id: original.id, attempt, status: 'paused', stage: 'preflight', message: 'lifecycle_preflight' })
    await refresh(panel)
    await progress.getByText('卸载已暂停 · 此节点全部受管服务', { exact: true }).waitFor()
  }
  await pause(1)
  acceptLifecycle = false
  await resume.click()
  await confirm('继续卸载')
  await retry.waitFor()
  assert(await resume.isDisabled(), `resume must stay locked during request recovery: ${await panel.innerText()}`)
  const resumed = posts.at(-1)
  assert.equal(resumed.id, original.id)
  assert.equal(resumed.attempt, 1)
  assert.equal(resumed.resume, true)
  await retryLifecycle(resumed, 3)
  assert.equal((await api(nodePath(managed.id))).lifecycle.attempt, 2)

  await pause(2)
  loseResponse = true
  const confirmedResume = responseFor(nodePath(managed.id), 'GET')
  await resume.click()
  await confirm('继续卸载')
  await confirmedResume
  await progress.getByText('正在卸载 · 此节点全部受管服务', { exact: true }).waitFor()
  assert.equal((await api(nodePath(managed.id))).lifecycle.attempt, 3)
  assert.equal(posts.length, 5)
  assert.equal(await retry.count(), 0)

  // A whole-node removal can revoke the binding before its final acknowledgement.
  // Losing the separate abandon request still leaves that work to verify.
  const finalizing = {
    ...await api(nodePath(managed.id)),
    bound: false, online: false, abandonable: true,
    lifecycle: { id: original.id, scope: 'all', status: 'finalizing', stage: 'cleanup', attempt: 3, message: '等待清理确认' },
  }
  await page.route(statusRoute, route => route.fulfill({ json: finalizing }))
  await page.route('**' + nodePath(managed.id) + '/abandon', route => route.abort('connectionreset'))
  await refresh(panel)
  await progress.getByText('正在确认清理结果 · 此节点全部受管服务', { exact: true }).waitFor()
  await panel.getByTestId('abandon-node').locator('summary').click()
  await button(panel, '确认节点已失效').click()
  await confirmation.getByRole('textbox').fill('服务操作验收')
  await confirm('确认操作')
  await panel.getByText(/仍有待确认的卸载任务/).waitFor()
  assert(await progress.isVisible())

  assert.deepEqual(errors, [])
  console.log('PASS recovery browser: enable failure, first report, lost revocation reply, uninstall/resume recovery, offline usage and two TCP check targets')
} finally {
  if (browser) await browser.close()
  hub.kill('SIGTERM')
  if (hub.exitCode === null) await new Promise(resolve => hub.once('exit', resolve))
  await rm(directory, { recursive: true, force: true })
}
