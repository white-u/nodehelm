/// <reference types="node" />
import assert from "node:assert/strict"
import { editable, instancePayload, newResource, pmBlocked, pmDraft, pmPayload, pmSummary, type PmPolicy, type RootSnapshot } from "./model.ts"

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
const limitedDraft = { ...pmDraft(pm), max_ips: 3, whitelist: " 203.0.113.7,2001:db8::1\n\n198.51.100.2，" }
assert.deepEqual(pmPayload(limitedDraft).ip_limit, { max_ips: 3, cooldown_min: 30, whitelist: ["203.0.113.7", "2001:db8::1", "198.51.100.2"] })
assert.deepEqual(pmPayload(pmDraft(pmPayload(limitedDraft))), pmPayload(limitedDraft))
assert.equal("ip_limit" in pmPayload({ ...limitedDraft, max_ips: 0 }), false, "0 turns the check off")
assert.throws(() => pmPayload({ ...limitedDraft, whitelist: "203.0.113.0/24" }), /203\.0\.113\.0\/24/)
assert.throws(() => pmPayload({ ...limitedDraft, whitelist: "fe80::1%eth0" }))
assert.throws(() => pmPayload({ ...limitedDraft, whitelist: "256.1.1.1" }))
assert.throws(() => pmPayload({ ...limitedDraft, whitelist: Array.from({ length: 33 }, (_, n) => `192.0.2.${n}`).join(",") }), /32/)
assert.doesNotThrow(() => pmPayload({ ...limitedDraft, whitelist: "::ffff:192.0.2.1 2001:DB8::1" }))
assert.equal(pmSummary({ ...pm, quota_gb: 100, reset_day: 1, ip_limit: { max_ips: 3, cooldown_min: 30, whitelist: [] } }), "配额 100 GiB／每月 1 日重置／IP 告警 3 个")
const empty = { installed: true, blocked: false, revision: "", resources: [] }
const root = (rows: number[] = [], rules: number[] = []): RootSnapshot => ({ ...empty, modules: { pm: { ...empty, resources: rules.map(port => ({ ...pm, port })) } }, port_monitor: { status: "ready", rows: rows.map(port => ({ port, quota_gb: 1, quota_mode: "in_out", reset_day: 0, comment: "", expiry_date: "", burst_enabled: false, state: "ready" })) } })
assert.equal(pmBlocked(root(), 23456), "")
assert.match(pmBlocked(root(), 443), /1024/)
assert.match(pmBlocked(root([23456]), 23456), /原 PM/)
assert.match(pmBlocked(root([], [23456]), 23456), /已有/)
assert.match(pmBlocked({ ...empty }, 23456), /启用 PM/)
console.log("PM drafts, whitelist checks and eligibility passed")
