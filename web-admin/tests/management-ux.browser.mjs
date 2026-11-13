// Browser regression paths with a disposable Hub and simulated node reports.
import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { mkdtemp, rm } from 'node:fs/promises'
import { createRequire } from 'node:module'
import os from 'node:os'
import path from 'node:path'

const { chromium } = createRequire(process.env.PLAYWRIGHT_PACKAGE ?? import.meta.url)('playwright')
const binary = process.argv[2]
if (!binary) throw new Error('Usage: node management-ux.browser.mjs /path/to/host-native/monitor-hub')
const address = '127.0.0.1:19919'
const base = 'http://' + address
const directory = await mkdtemp(path.join(os.tmpdir(), 'nm-ux-browser-'))
const hub = spawn(binary, ['--listen', address, '--db', directory + '/hub.db', '--themes', directory + '/themes', '--site', 'https://panel.example.com'])
let logs = ''
hub.stdout.on('data', data => logs += data)
hub.stderr.on('data', data => logs += data)
let browser
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
  page.on('pageerror', error => errors.push(error.message))
  page.on('dialog', async dialog => {
    errors.push('Unexpected native dialog: ' + dialog.type())
    await dialog.dismiss()
  })
  page.setDefaultTimeout(12_000)

  async function api(route, data, method = data === undefined ? 'GET' : 'POST') {
    const response = await context.request.fetch(base + route, { method, data, headers: { Origin: base, 'Sec-Fetch-Site': 'same-origin' } })
    assert(response.ok(), `${route}: HTTP ${response.status()} ${await response.text()}`)
    return response.json()
  }

  await page.goto(base + '/admin/manage')
  await page.getByLabel('应急密码').fill(password)
  await page.getByRole('button', { name: '登录', exact: true }).click()
  await page.waitForURL('**/admin/nodes')
  const { id } = await api('/api/nodes', { name: '操作验收', public: false })
  await api(`/api/nodes/${id}`, { ipv4_pin: '203.0.113.42' }, 'PUT')
  await api('/api/management', { enabled: true })
  await page.goto(base + '/admin/manage')
  const card = page.getByTestId(`node-card-${id}`)
  const enrollment = page.getByTestId('node-management-panel')
  const button = (scope, name) => scope.getByRole('button', { name, exact: true })
  const confirm = label => button(page.getByRole('region', { name: '操作确认' }), label).click()
  const refresh = () => button(page, '刷新此节点').click()
  const select = module => page.getByTestId(`node-module-${id}-${module}`).click()
  const detail = page.getByTestId('node-detail')
  const instance = page.getByTestId('service-instance-editor')
  const policy = page.getByTestId('pm-policy-editor')
  const name = instance.getByLabel('名称（可选）', { exact: true })
  const port = instance.getByLabel('监听端口', { exact: true })
  const quota = scope => scope.getByLabel('配额（GiB，0 关闭）', { exact: true })
  const withPm = instance.getByRole('checkbox', { name: /同时启用流量统计/ })
  const save = scope => button(scope, '保存并应用')
  const first = detail.getByTestId('instance-34567')
  const pm = { installed: false, resources: [], revision: 'a'.repeat(64), blocked: false, runtime: {}, core: { candidates: [] } }
  const snell = { name: '现有实例', port: 34567, public_port: 34567, host: 'client.example.com', mode: 'default', psk: 's'.repeat(32) }
  const snapshot = {
    installed: true, resources: [snell, { ...snell, name: '另一个实例', port: 45678, public_port: 45678 }],
    revision: 'b'.repeat(64), blocked: false, runtime: { 34567: 'active', 45678: 'active' },
    core: { version: '6.0.0', sha256: 'a'.repeat(64), candidates: [] }, modules: { pm },
  }
  const credential = 'c'.repeat(64)
  async function poll(result = null, nodeCredential = credential) {
    const response = await context.request.post(base + '/api/management-agent/v1/poll', {
      data: { snapshot, result }, headers: { Authorization: 'Bearer ' + nodeCredential, 'X-Node-Manager-Protocol': '1' },
    })
    assert(response.ok(), `poll: HTTP ${response.status()} ${await response.text()}`)
    return response.json()
  }
  async function submit(trigger, label) {
    const posted = page.waitForResponse(response => response.request().method() === 'POST' && response.url().endsWith('/operations'))
    await trigger.click()
    await confirm(label)
    const response = await posted
    assert(response.ok(), await response.text())
    const { task } = await poll()
    assert(task, 'the submitted request reaches the node')
    return { task, request: response.request().postDataJSON() }
  }
  async function report(task, status, code) {
    await poll({ id: task.request.id, status, code, message: 'fixture result' })
    await refresh()
  }

  await button(card, '接入管理').click()
  await button(enrollment, '安装并接入此节点').click()
  const command = await enrollment.locator('pre').textContent()
  const token = command.match(/[a-f\d]{64}$/)?.[0]
  assert(token, 'the UI provides an enrollment code')
  await api('/api/management-agent/v1/enroll', { enrollment: token, credential, boot_id: '' })
  await enrollment.getByText('上次读取离线', { exact: true }).waitFor()
  assert.equal(await enrollment.getByText('管理组件与后台版本不一致，请先升级组件。', { exact: true }).count(), 0)
  assert.equal(await enrollment.getByTestId('component-outdated').count(), 0)
  await poll()
  await enrollment.getByText('上次读取在线', { exact: true }).waitFor()
  await button(enrollment, '收起').click()
  await select('snell')

  // Existing instances lead to the module even before traffic statistics are enabled.
  await first.getByRole('button', { name: /流量统计/ }).click()
  await button(detail, '启用流量统计').waitFor()
  await select('snell')

  // Installing the dependency in the creation form preserves the unsaved instance.
  await button(detail, '新增实例').click()
  await name.fill('端口测试草稿')
  await port.fill('23456')
  let submitted = await submit(button(instance, '启用流量统计'), '启用流量统计')
  assert.equal(submitted.task.request.module, 'pm')
  assert.equal(submitted.task.request.action, 'install')
  pm.installed = true
  pm.revision = 'c'.repeat(64)
  await report(submitted.task, 'succeeded')
  await withPm.check()
  assert.equal(await name.inputValue(), '端口测试草稿')
  assert.equal(await port.inputValue(), '23456')
  await quota(instance).fill('19')

  // An ineligible port may not silently drop an already selected rule.
  await port.fill('443')
  assert(await withPm.isChecked())
  assert.equal(await quota(instance).inputValue(), '19')
  assert(await save(instance).isDisabled())
  await withPm.uncheck()
  assert(await save(instance).isEnabled())
  await port.fill('23456')
  await withPm.check()
  await quota(instance).fill('19')
  await port.fill('443')
  await port.fill('23456')

  submitted = await submit(save(instance), '确认应用')
  assert.equal(submitted.request.then.payload.quota_gb, 19)
  assert(await name.isDisabled(), 'the draft stays visible and frozen while the node runs the task')
  await report(submitted.task, 'rejected', 'port_in_use')
  await detail.getByRole('alert').filter({ hasText: /监听端口已被占用/ }).waitFor()
  assert.equal(await name.inputValue(), '端口测试草稿')
  assert.equal(await port.inputValue(), '23456')
  assert(await withPm.isChecked())
  assert.equal(await quota(instance).inputValue(), '19')
  if (process.env.MANAGEMENT_UX_SCREENSHOTS) {
    await instance.screenshot({ path: '/tmp/node-manager-draft-recovery-desktop.png' })
    await page.setViewportSize({ width: 390, height: 844 })
    assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1))
    await instance.screenshot({ path: '/tmp/node-manager-draft-recovery-mobile.png' })
    await page.setViewportSize({ width: 1280, height: 900 })
  }
  await port.fill('23457')
  await withPm.uncheck()
  submitted = await submit(save(instance), '确认应用')
  assert.equal(submitted.task.request.payload.port, 23457)
  snapshot.resources.push({ ...submitted.task.request.payload, psk: 'r'.repeat(32) })
  snapshot.runtime[23457] = 'active'
  snapshot.revision = 'd'.repeat(64)
  await report(submitted.task, 'succeeded')
  await instance.waitFor({ state: 'detached' })

  // Copying the applied configuration and editing the client's address are independent.
  await button(first, '编辑').click()
  await name.fill('尚未应用的名称')
  await instance.locator('summary').click()
  const host = instance.getByLabel('客户端访问地址', { exact: true })
  assert.equal(await host.inputValue(), 'client.example.com')
  await button(instance, '采用当前节点地址').click()
  assert.equal(await host.inputValue(), '203.0.113.42')
  await button(first, '显示配置').click()
  const config = first.getByLabel('客户端配置内容')
  assert.match(await config.textContent(), /client\.example\.com/)
  assert.equal(await name.inputValue(), '尚未应用的名称')
  assert.equal(await host.inputValue(), '203.0.113.42')
  assert.equal(await page.getByRole('region', { name: '操作确认' }).count(), 0)
  submitted = await submit(save(instance), '确认应用')
  assert.equal(submitted.task.request.payload.host, '203.0.113.42')
  assert.equal(submitted.task.request.payload.port, 34567)
  assert.equal(submitted.task.request.payload.psk, '', 'the existing key is retained')
  snapshot.resources[0] = { ...snell, ...submitted.task.request.payload, psk: snell.psk }
  snapshot.revision = 'e'.repeat(64)
  await report(submitted.task, 'succeeded')
  await instance.waitFor({ state: 'detached' })
  await button(first, '显示配置').click()
  assert.match(await config.textContent(), /203\.0\.113\.42/)

  // Loss of runtime eligibility does not choose another port on the user's behalf.
  await select('pm')
  await button(detail, '新增流量统计规则').click()
  const candidate = policy.getByLabel('业务端口', { exact: true })
  const remark = policy.getByLabel('名称／备注', { exact: true })
  await candidate.selectOption('34567')
  await remark.fill('保留计量策略')
  await quota(policy).fill('27')
  snapshot.runtime[34567] = 'inactive'
  await poll()
  await refresh()
  await candidate.locator('option[value="34567"]:disabled').waitFor({ state: 'attached' })
  assert.equal(await candidate.inputValue(), '34567')
  assert(await save(policy).isDisabled())
  await candidate.selectOption('45678')
  assert.equal(await remark.inputValue(), '保留计量策略')
  assert.equal(await quota(policy).inputValue(), '27')
  submitted = await submit(save(policy), '应用规则')
  assert(await remark.isDisabled())
  await report(submitted.task, 'rolled_back', 'pm_nft')
  await detail.getByRole('alert').filter({ hasText: /规则检查失败/ }).waitFor()
  assert.equal(await candidate.inputValue(), '45678')
  assert.equal(await remark.inputValue(), '保留计量策略')
  assert.equal(await quota(policy).inputValue(), '27')
  await quota(policy).fill('28')
  submitted = await submit(save(policy), '应用规则')
  assert.equal(submitted.task.request.payload.quota_gb, 28)
  pm.resources = [submitted.task.request.payload]
  pm.revision = 'f'.repeat(64)
  await report(submitted.task, 'succeeded')
  await policy.waitFor({ state: 'detached' })
  await detail.getByTestId('pm-rule-45678').waitFor()

  // A read error during the second step pauses automatic checks without losing the original policy.
  await select('snell')
  await button(detail, '新增实例').click()
  await port.fill('56789')
  await withPm.check()
  await quota(instance).fill('91')
  submitted = await submit(save(instance), '确认应用')
  snapshot.resources.push({ ...submitted.task.request.payload, psk: 't'.repeat(32) })
  snapshot.runtime[56789] = 'active'
  snapshot.revision = '1'.repeat(64)
  const { task: followUp } = await poll({ id: submitted.task.request.id, status: 'succeeded', message: 'fixture applied' })
  assert.equal(followUp.request.module, 'pm')
  const progress = detail.getByTestId('chain-progress')
  await progress.filter({ hasText: /^2\/2/ }).waitFor()
  await instance.waitFor({ state: 'detached' })
  const statusRoute = `**/api/management/${id}`
  await page.route(statusRoute, route => route.abort('failed'))
  await detail.getByText(/读取失败，已停止自动确认/).waitFor()
  await page.unroute(statusRoute)
  await report(followUp, 'rejected', 'pm_nft')
  await progress.filter({ hasText: /应用流量统计规则未成功.*已保留填写的参数/ }).waitFor()
  await button(detail.getByTestId('instance-56789'), '添加流量统计').click()
  await policy.waitFor()
  assert.equal(await candidate.inputValue(), '56789')
  assert.equal(await quota(policy).inputValue(), '91')
  await button(policy, '取消').click()
  await confirm('放弃修改')
  await policy.waitFor({ state: 'detached' })

  // A command can expire while the panel stays open.
  const another = await api('/api/nodes', { name: '接入命令验收', public: false })
  await page.reload()
  await page.getByRole('group', { name: '按接入状态筛选' }).getByRole('button', { name: /^未接入/ }).click()
  await button(page.getByTestId(`node-card-${another.id}`), '接入管理').click()
  await page.clock.install()
  await button(enrollment, '安装并接入此节点').click()
  const original = await enrollment.locator('pre').textContent()
  await button(enrollment, '复制命令').waitFor()
  await page.clock.fastForward(601_000)
  const regenerate = enrollment.getByRole('button', { name: /重新生成.*命令/ })
  await regenerate.waitFor()
  assert.equal(await button(enrollment, '复制命令').count(), 0)
  assert.equal(await enrollment.locator('pre').count(), 0)
  await regenerate.click()
  await button(enrollment, '复制命令').waitFor()
  const replacement = await enrollment.locator('pre').textContent()
  assert.notEqual(replacement, original)
  const secondToken = replacement.match(/[a-f\d]{64}$/)?.[0]
  assert(secondToken)

  // With no default address the field opens and stays open as the user types.
  const secondCredential = 'd'.repeat(64)
  await api('/api/management-agent/v1/enroll', { enrollment: secondToken, credential: secondCredential, boot_id: '' })
  await poll(null, secondCredential)
  await button(enrollment, '刷新此节点').click()
  await enrollment.getByText('上次读取在线', { exact: true }).waitFor()
  await button(enrollment, '收起').click()
  await page.getByRole('group', { name: '按接入状态筛选' }).getByRole('button', { name: /^已接入/ }).click()
  await page.getByTestId(`node-module-${another.id}-snell`).click()
  await button(detail, '新增实例').click()
  assert.equal(await host.inputValue(), '')
  assert(await host.isVisible())
  await host.pressSequentially('new-client.example.com')
  assert(await host.isVisible())
  assert.equal(await host.inputValue(), 'new-client.example.com')
  assert(await save(instance).isEnabled())

  assert.deepEqual(errors, [])
  console.log('PASS management UX: explicit PM selection and enablement, failure recovery, applied config and address change, unavailable ports, first report and command expiration')
} finally {
  if (browser) await browser.close()
  hub.kill('SIGTERM')
  if (hub.exitCode === null) await new Promise(resolve => hub.once('exit', resolve))
  await rm(directory, { recursive: true, force: true })
}
