/// <reference types="node" />
import assert from "node:assert/strict"
import { requestObserved, serviceFeedbackClass, pmTrafficBars, editable, instancePayload, newPmDraft, newResource, pmBlocked, pmDraft, pmPayload, pmSummary, pmAlarmChange, pmWarnChange, ipThresholds, type PmPolicy, type RootSnapshot } from "./model.ts"

const draft = newResource("snell", "example.com")
assert.equal(draft.name, "")
assert.deepEqual(instancePayload("snell", { ...draft, name: "  ", port: 23456, public_port: 23456 }), {
  name: "23456", port: 23456, host: "example.com", public_port: 23456, mode: "default", psk: "",
})
assert.equal(draft.name, "", "normalizing the request must not overwrite the draft")
const old = { name: "Original", port: 12345, host: "old.example.com", public_port: 54321, mode: "unshaped", psk: "server-secret" }
const edited = instancePayload("snell", { ...editable("snell", old), name: "" })
assert.equal(edited.host, "old.example.com")
assert.equal(edited.public_port, 54321, "hidden existing NAT port must be preserved")
assert.equal(edited.mode, "unshaped")
assert.equal(edited.psk, "", "editing preserves the node secret instead of resubmitting it")
assert.equal(edited.name, "12345")
const sb = newResource("sb", "example.org")
assert.equal(sb.sni, "", "SNI is a visible user field, not a hidden preset")
assert.equal(instancePayload("sb", { ...sb, name: "  入口  ", sni: "  www.microsoft.com " }).sni, "www.microsoft.com")
assert.equal(instancePayload("sb", { ...sb, name: "  入口  " }).name, "入口")
assert.equal(instancePayload("sb", { ...sb, name: "", protocol: "ss2022", sni: "old.example.com" }).sni, "", "protocol switch must not submit the previous Reality SNI")
assert.equal(instancePayload("sb", { ...sb, name: "", port: 34567 }).name, "34567")
assert.equal(instancePayload("fw", { name: "", port: 12345 }).name, "", "other module validation remains unchanged")
console.log("service name defaults, hidden configuration preservation and SB field switching passed")

const pm: PmPolicy = { port: 23456, name: "PM", quota_gb: 0, quota_mode: "in_out", reset_day: 0, expiry_date: "", egress_mbps: 0 }
assert.deepEqual(pmPayload(pmDraft(pm)), pm, "a rule without an IP limit keeps its earlier shape")
// The form asks for the count that warns; the rule keeps the original PM's limit below it.
const limitedDraft = { ...pmWarnChange(pmDraft(pm), 3), whitelist: " 203.0.113.7,2001:db8::1\n\n198.51.100.2，" }
assert.deepEqual(pmPayload(limitedDraft).ip_limit, { max_ips: 3, cooldown_min: 30, whitelist: ["203.0.113.7", "2001:db8::1", "198.51.100.2"] })
assert.deepEqual(pmPayload(pmDraft(pmPayload(limitedDraft))), pmPayload(limitedDraft))
assert.deepEqual(pmPayload({ ...limitedDraft, block: true }).ip_limit?.block, true)
assert.deepEqual(pmPayload(pmDraft(pmPayload({ ...limitedDraft, block: true }))), pmPayload({ ...limitedDraft, block: true }))
assert.equal("block" in pmPayload(limitedDraft).ip_limit!, false, "off is left out")
assert.equal("ip_limit" in pmPayload({ ...limitedDraft, max_ips: 0 }), false, "0 turns the check off")
assert.throws(() => pmPayload({ ...limitedDraft, max_ips: 4096 }), /1–4095/)
const fresh = newPmDraft(40001, "新实例")
assert.deepEqual([fresh.max_ips, fresh.block, fresh.quota_gb], [3, true, 0], "new rules warn over 3, alarm over 5 and block over 6")
assert.deepEqual(pmPayload(fresh).ip_limit, { max_ips: 3, cooldown_min: 30, whitelist: [], block: true })
assert.deepEqual([fresh.alarm_ips, fresh.block_ips], [5, 6])
const raisedWarn = pmWarnChange(fresh, 10)
assert.deepEqual([raisedWarn.alarm_ips, raisedWarn.block_ips], [10, 11], "a warning count past the others raises them")
assert.deepEqual([pmWarnChange(fresh, 2).alarm_ips, pmWarnChange(fresh, 2).block_ips], [5, 6], "a lower one leaves them")
assert.deepEqual([pmDraft(pm).alarm_ips, pmDraft(pm).block_ips], [5, 6], "a rule without a limit offers the defaults")
const customSteps = { ...fresh, max_ips: 4, alarm_ips: 8, block_ips: 12 }
assert.deepEqual(pmPayload(customSteps).ip_limit, { max_ips: 4, alarm_ips: 8, block_ips: 12, cooldown_min: 30, whitelist: [], block: true })
assert.deepEqual(pmDraft({ ...pm, ip_limit: pmPayload(customSteps).ip_limit }).block_ips, 12, "chosen counts come back to the form")
const steppedWarn = [5, 6, 7].reduce(pmWarnChange, customSteps)
assert.deepEqual([steppedWarn.alarm_ips, steppedWarn.block_ips], [8, 12], "chosen counts stay however the warning count is stepped")
assert.equal("block_ips" in pmPayload({ ...customSteps, block: false }).ip_limit!, false, "no block count without blocking")
assert.deepEqual(pmAlarmChange(customSteps, 15).block_ips, 16, "an alarm count past the block count raises it")
assert.deepEqual(pmAlarmChange(customSteps, 9).block_ips, 12, "a lower one leaves it")
assert.equal(pmDraft({ ...pm, ip_limit: { max_ips: 2, alarm_ips: 7, cooldown_min: 30, whitelist: [] } }).block_ips, 8, "turning blocking on offers a count above the alarm")
assert.throws(() => pmPayload({ ...customSteps, alarm_ips: 3 }), /报警阈值/)
assert.throws(() => pmPayload({ ...customSteps, block_ips: 7 }), /阻断阈值/)
assert.throws(() => pmPayload({ ...customSteps, block_ips: 8 }), /阻断阈值须大于报警阈值/)
assert.throws(() => pmPayload({ ...customSteps, block_ips: 4096 }), /最大 4095/)
assert.equal(ipThresholds({ max_ips: 4, alarm_ips: 8, block_ips: 12, block: true }), ">4 告警 · >8 报警 · >12 阻断新增 IP")
assert.equal(ipThresholds({ max_ips: 3, block: true }), ">3 告警 · >5 报警 · >6 阻断新增 IP")
assert.doesNotThrow(() => pmPayload({ ...limitedDraft, whitelist: "203.0.113.0/24 2001:db8::/64" }))
assert.throws(() => pmPayload({ ...limitedDraft, whitelist: "fe80::1%eth0" }))
assert.throws(() => pmPayload({ ...limitedDraft, whitelist: "256.1.1.1" }))
assert.throws(() => pmPayload({ ...limitedDraft, whitelist: Array.from({ length: 33 }, (_, n) => `192.0.2.${n}`).join(",") }), /32/)
assert.doesNotThrow(() => pmPayload({ ...limitedDraft, whitelist: "::ffff:192.0.2.1 2001:DB8::1" }))
assert.equal(pmSummary({ ...pm, quota_gb: 100, reset_day: 1, ip_limit: { max_ips: 3, cooldown_min: 30, whitelist: [] } }), "配额 100 GiB／每月 1 日重置／IP >3 告警 · >5 报警")
assert.equal(pmSummary({ ...pm, ip_limit: { max_ips: 2, cooldown_min: 30, whitelist: [], block: true } }), "无限额／IP >2 告警 · >4 报警 · >5 阻断新增 IP")
const empty = { installed: true, blocked: false, revision: "", resources: [] }
const root = (rules: number[] = []): RootSnapshot => ({ ...empty, modules: { pm: { ...empty, resources: rules.map(port => ({ ...pm, port })) } } })
assert.equal(pmBlocked(root(), 23456), "")
assert.match(pmBlocked(root(), 443), /1024/)
assert.match(pmBlocked(root([23456]), 23456), /已有/)
assert.match(pmBlocked({ ...empty }, 23456), /启用流量统计/)
console.log("PM drafts, whitelist checks and eligibility passed")

for (const whitelist of ["203.0.113.0/33", "2001:db8::/129", "203.0.113.0/", "203.0.113.0/24/1"]) assert.throws(() => pmPayload({ ...limitedDraft, whitelist }))

// History is UTC, bounded to seven days, and never turns missing samples into zero.
const trafficNow = Date.parse('2026-11-07T10:30:00Z') / 1000
const trafficHours: import('./model.ts').PmTrafficHour[] = [
  [Date.parse('2026-11-06T23:00:00Z') / 1000, 12, 40],
  [Date.parse('2026-11-07T00:00:00Z') / 1000, 3, 9],
  [Date.parse('2026-11-07T10:00:00Z') / 1000, 0, 0],
]
const trafficDays = pmTrafficBars(trafficHours, trafficNow)
assert.equal(trafficDays.length, 7)
assert.equal(trafficDays[0].key, '2026-11-01')
assert.equal(trafficDays[0].partial, false, 'today and six earlier UTC dates fit within rolling seven-day retention')
assert.equal(trafficDays[5].rx, 12, 'UTC midnight keeps adjacent dates separate')
assert.equal(trafficDays[6].tx, 9)
const trafficDay = pmTrafficBars(trafficHours, trafficNow, '2026-11-07')
assert.equal(trafficDay.length, 24)
assert.equal(trafficDay[0].state, 'recorded')
assert.equal(trafficDay[1].state, 'missing')
assert.equal(trafficDay[10].state, 'recorded', 'explicit zero readings remain distinct from missing rows')
assert.equal(trafficDay[10].partial, true)
assert.equal(trafficDay[11].state, 'future')
assert.equal(trafficDay[23].state, 'future')
assert.equal(pmTrafficBars(trafficHours, trafficNow, '2026-11-06')[23].tx, 40)
console.log('PM UTC history, midnight boundaries, retention and missing/future hours passed')

assert.equal(serviceFeedbackClass("rejected", "delete"), "text-destructive", "failure takes precedence over destructive action")
assert.match(serviceFeedbackClass("succeeded", "delete"), /text-orange-/)
assert.match(serviceFeedbackClass("succeeded", "apply"), /text-emerald-/)
assert.match(serviceFeedbackClass("delivered", "apply"), /text-blue-/, "submission is not confirmed success")
assert.match(serviceFeedbackClass("interrupted", "uninstall"), /text-amber-/)
console.log("Service feedback outcome precedence passed")

// A resume request is confirmed by a new attempt, not merely by the old job id.
const pausedJob = { id: "job", scope: "all" as const, status: "paused", stage: "preflight", message: "", attempt: 1 }
const resumeRequest = { id: "job", body: "{}", kind: "lifecycle" as const, attempt: 1 }
assert.equal(requestObserved({ bound: true, lifecycle: pausedJob }, resumeRequest), false)
assert.equal(requestObserved({ bound: true, lifecycle: { ...pausedJob, attempt: 2, status: "running" } }, resumeRequest), true)
assert.equal(requestObserved({ bound: true, lifecycle: { ...pausedJob, status: "succeeded" } }, resumeRequest), true)
assert.equal(requestObserved({ bound: true, lifecycle: { ...pausedJob, id: "other", attempt: 2 } }, resumeRequest), false)
assert.equal(requestObserved({ bound: true, lifecycle: pausedJob }, { ...resumeRequest, attempt: undefined }), true)
console.log("Lifecycle recovery distinguishes a queued resume from its prior attempt")
