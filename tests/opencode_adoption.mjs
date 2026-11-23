import assert from "node:assert/strict"
import { createHash } from "node:crypto"
import { access, copyFile, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { setTimeout } from "node:timers/promises"
import { pathToFileURL } from "node:url"
import test from "node:test"

test("OpenCode 1.19.5 hooks coexist and hand off only the committed opening", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "wfc-opencode-hooks-"))
  t.after(() => rm(root, { recursive: true, force: true }))
  const dispatch = "d-" + "3".repeat(32)
  const attempt = "a-" + "1".repeat(32)
  const directory = join(root, "checkout with space")
  const store = join(root, `opencode-${dispatch}-${attempt}`)
  await mkdir(directory)
  await mkdir(store, { mode: 0o700 })
  const database = join(store, "opencode.db")
  const plugin = join(store, "wfc-adoption.mjs")
  await copyFile(new URL("../src/workflow/host/opencode/adoption.mjs", import.meta.url), plugin)
  const bin = join(root, "fixture wfc")
  await writeFile(bin, `#!/bin/sh
[ "$#" = 1 ] && [ "$1" = --opencode-adopt ] || exit 1
[ -n "$WFC_WORKER_CALLBACK" ] && [ -z "$WFC_CONTROLLER_TOKEN" ] || exit 1
[ -n "$WFC_OPENCODE_ADOPTION" ] || exit 1
: > "$WFC_STATE/callback-began"
for value in 1 2 3 4 5 6 7 8 9 10; do
  [ -f "$WFC_STATE/release-callback" ] && break
  /bin/sleep .05
done
[ -f "$WFC_STATE/release-callback" ] || exit 1
receipt=$(/bin/mktemp "$WFC_STATE/callback.XXXXXX") || exit 1
/bin/cat > "$receipt" && /bin/mv "$receipt" "$receipt.ready"
`, { mode: 0o700 })
  const secret = "synthetic-token-only"
  const capability = "a".repeat(64)
  const capabilityFile = join(store, "wfc-adoption.capability")
  await writeFile(capabilityFile, capability, { mode: 0o600 })
  const env = {
    WFC_STATE: root, WFC_DISPATCH: dispatch, WFC_ATTEMPT: attempt,
    WFC_ROUND: "1", WFC_BIN: bin, WFC_WORKER_CALLBACK: secret, OPENCODE_DB: database,
    WFC_CONTROLLER_TOKEN: "synthetic-controller-token",
  }
  const previous = Object.fromEntries(Object.keys(env).map((key) => [key, process.env[key]]))
  t.after(() => {
    for (const [key, value] of Object.entries(previous)) {
      if (value === undefined) delete process.env[key]
      else process.env[key] = value
    }
  })
  Object.assign(process.env, env)
  const { default: pluginFactory } = await import(pathToFileURL(plugin))
  const text = "first brief, with 合成 text and an exact final space "
  const briefSha256 = createHash("sha256").update(text).digest("hex")
  const options = { briefSha256, cwd: directory }
  for (const [key, value] of Object.entries({
    WFC_WORKER_CALLBACK: "", WFC_ROUND: "2", WFC_ATTEMPT: "a-" + "2".repeat(32),
    OPENCODE_DB: join(store, "other.db"),
  })) {
    process.env[key] = value
    assert.deepEqual(await pluginFactory({ directory }, options), {})
    process.env[key] = env[key]
  }
  assert.deepEqual(await pluginFactory({ directory: root }, options), {})
  await access(capabilityFile)
  const hook = await pluginFactory({ directory }, options)
  await assert.rejects(access(capabilityFile), { code: "ENOENT" })
  assert.deepEqual(await pluginFactory({ directory }, options), {})
  const sessionID = "ses_original"
  const messageID = "msg_original"
  const part = { id: "prt_original", messageID, sessionID, type: "text", text }
  const opening = { message: { id: messageID, sessionID, role: "user" }, parts: [part] }
  const notice = (part, id = sessionID) =>
    ({ event: { type: "message.part.updated", properties: { sessionID: id, part } } })
  const order = []
  const plugins = [
    { "chat.message": () => order.push("configured hook"), event: () => order.push("configured event") },
    hook,
    { "chat.message": () => order.push("discovered hook"), event: () => order.push("discovered event") },
  ]
  const chat = async (input, output) => {
    for (const plugin of plugins) await plugin["chat.message"](input, output)
  }
  const event = (payload) => {
    for (const plugin of plugins) plugin.event(payload)
  }
  const receipts = async () => (await readdir(root)).filter((name) => name.startsWith("callback."))
  await chat({ sessionID }, { ...opening, parts: [{ ...part, synthetic: true }] })
  await chat({ sessionID, messageID: "msg_copy" }, opening)
  event(notice(part))
  assert.deepEqual(await receipts(), [])

  await chat({ sessionID }, opening)
  assert.deepEqual(opening.parts, [part])
  assert.deepEqual(await receipts(), [])
  event(notice({ ...part, id: "prt_copy" }))
  event(notice({ ...part, messageID: "msg_copy" }))
  event(notice({ ...part, text: text.trim() }))
  event(notice(part, "ses_copy"))
  assert.deepEqual(await receipts(), [])
  const copy = { ...part, id: "prt_copy", messageID: "msg_copy", sessionID: "ses_copy" }
  await chat({ sessionID: copy.sessionID }, {
    message: { id: copy.messageID, sessionID: copy.sessionID, role: "user" }, parts: [copy],
  })
  event(notice(copy, copy.sessionID))
  event(notice(part))
  event(notice(part))
  let began = false
  for (let retry = 0; retry < 50 && !began; retry++) {
    await setTimeout(10)
    began = await access(join(root, "callback-began")).then(() => true, () => false)
  }
  assert.ok(began)
  assert.deepEqual(await receipts(), [])
  await writeFile(join(root, "release-callback"), "")
  let files = []
  for (let retry = 0; retry < 100 && !files.some((name) => name.endsWith(".ready")); retry++) {
    await setTimeout(20)
    files = await receipts()
  }
  assert.equal(files.length, 1)
  assert.ok(files[0].endsWith(".ready"))
  const input = await readFile(join(root, files[0]), "utf8")
  assert.ok(!input.includes(secret))
  assert.ok(!input.includes(capability))
  assert.deepEqual(JSON.parse(input), { dispatch, attempt, cwd: directory, database,
    session: sessionID, firstMessage: messageID })
  assert.deepEqual(order.slice(0, 2), ["configured hook", "discovered hook"])
  assert.deepEqual(order.slice(-2), ["configured event", "discovered event"])
})
