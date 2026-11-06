/// <reference types="node" />
import assert from "node:assert/strict"
import { editable, instancePayload, newResource } from "./model.ts"

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
