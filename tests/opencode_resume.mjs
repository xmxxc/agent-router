import assert from "node:assert/strict"
import { createHash } from "node:crypto"
import { access, copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { setTimeout } from "node:timers/promises"
import { pathToFileURL } from "node:url"
import test from "node:test"

const CAPABILITY = "a".repeat(64)

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), "wfc-opencode-resume-"))
  const dispatch = "d-" + "3".repeat(32)
  const attempt = "a-" + "8".repeat(32)
  const dir = join(root, "opencode-" + dispatch + "-" + attempt)
  const cwd = join(root, "synthetic cwd")
  const bin = join(root, "fake-wfc")
  await mkdir(cwd)
  await mkdir(dir, { mode: 0o700 })
  const launch = { dispatch, attempt, round: "2", cwd, database: join(root, "synthetic.db"),
    session: "ses_original" }
  const brief = "[router] Resumed once 合成 with a private brief."
  const wire = JSON.stringify({ launch, brief })
  await writeFile(join(dir, "resume.json"), wire, { mode: 0o600 })
  await writeFile(join(dir, "wfc-adoption.capability"), CAPABILITY, { mode: 0o600 })
  const plugin = join(dir, "wfc-opencode-resume.mjs")
  await copyFile(new URL("../src/workflow/host/opencode/resume.mjs", import.meta.url), plugin)
  await writeFile(bin, [
    "#!/bin/sh",
    '[ "$#" = 1 ] && [ "$1" = --opencode-resume-ready ] || exit 1',
    '[ -z "$WFC_CONTROLLER_TOKEN" ] && [ -n "$WFC_WORKER_CALLBACK" ] && [ -n "$WFC_OPENCODE_ADOPTION" ] || exit 1',
    "set -C",
    '/bin/cat > "$WFC_STATE/claim" || exit 1',
    ': > "$WFC_STATE/claim-complete"',
    "for number in 1 2 3 4 5 6 7 8 9 10; do",
    '  [ -f "$WFC_STATE/release" ] && exit 0',
    "  /bin/sleep .05",
    "done",
    "exit 1",
    "",
  ].join("\n"), { mode: 0o700 })
  const env = { WFC_STATE: root, WFC_DISPATCH: dispatch, WFC_ATTEMPT: attempt,
    WFC_ROUND: launch.round, WFC_BIN: bin, WFC_WORKER_CALLBACK: "synthetic-fixture",
    WFC_CONTROLLER_TOKEN: "synthetic-controller", OPENCODE_DB: launch.database }
  const old = Object.fromEntries(Object.keys(env).map((key) => [key, process.env[key]]))
  const start = process.cwd()
  const controller = new AbortController()
  let dispose
  t.after(async () => {
    controller.abort()
    dispose?.()
    process.chdir(start)
    for (const [key, value] of Object.entries(old)) {
      if (value === undefined) delete process.env[key]
      else process.env[key] = value
    }
    await rm(root, { recursive: true, force: true })
  })
  process.chdir(cwd)
  Object.assign(process.env, env)
  const notes = []
  const editor = { focused: true, plainText: "", traits: { owner: "opencode", role: "prompt" },
    setText(text) { this.plainText = text } }
  const api = {
    app: { version: "1.19.5" },
    route: { current: { name: "session", params: { sessionID: launch.session } } },
    state: { ready: true, path: { directory: cwd }, session: {
      get: () => ({ id: launch.session, directory: cwd }),
      status: () => ({ type: "idle" }), permission: () => [], question: () => [],
    } },
    ui: { dialog: { open: false } },
    renderer: { currentFocusedRenderable: editor },
    keymap: { dispatchCommand(command) { notes.push([command, editor.plainText]) } },
    lifecycle: { signal: controller.signal, onDispose(fn) { dispose = fn } },
  }
  const { default: pluginModule } = await import(pathToFileURL(plugin))
  const approved = () => createHash("sha256").update(wire).digest("hex")
  return { launch, brief, editor, api, notes, pluginModule, root,
    approve: (hash = approved(), expiresAt = Date.now() + 5000) =>
      writeFile(join(dir, "permitted"), JSON.stringify({ planSha256: hash, expiresAt }), { mode: 0o600 }),
    release: () => writeFile(join(root, "release"), ""),
    claimed: () => access(join(root, "claim-complete")).then(() => true, () => false),
  }
}

async function eventually(predicate) {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (await predicate()) return
    await setTimeout(15)
  }
  assert.fail("Timed out waiting for synthetic resume handoff")
}

test("the pinned TUI submits once in the original focused, authorized prompt", async (t) => {
  const f = await fixture(t)
  await f.approve("0".repeat(64))
  f.api.app.version = "1.19.4"
  await f.pluginModule.tui(f.api)
  await setTimeout(80)
  assert.equal(await f.claimed(), false)
  f.api.app.version = "1.19.5"
  f.editor.focused = false
  await f.pluginModule.tui(f.api)
  await f.approve()
  await setTimeout(80)
  assert.equal(await f.claimed(), false)
  f.editor.focused = true
  f.api.route.current.params.sessionID = "ses_other"
  await setTimeout(80)
  assert.equal(await f.claimed(), false)
  f.api.route.current.params.sessionID = f.launch.session
  f.api.ui.dialog.open = true
  await setTimeout(80)
  assert.equal(await f.claimed(), false)
  f.api.ui.dialog.open = false
  await eventually(f.claimed)
  const receipt = await readFile(join(f.root, "claim"), "utf8")
  assert.ok(!receipt.includes(f.brief) && !receipt.includes(CAPABILITY))
  assert.deepEqual(JSON.parse(receipt), f.launch)
  assert.deepEqual(f.notes, [])
  await f.release()
  await eventually(() => f.notes.length === 1)
  assert.deepEqual(f.notes, [["prompt.submit", f.brief]])
  f.editor.plainText = ""
  await f.pluginModule.tui(f.api)
  await setTimeout(150)
  assert.equal(f.notes.length, 1)
})

test("a focus change during the claim leaves the brief unsent", async (t) => {
  const f = await fixture(t)
  await f.approve()
  await f.pluginModule.tui(f.api)
  await eventually(f.claimed)
  f.editor.focused = false
  await f.release()
  await setTimeout(120)
  f.editor.focused = true
  await setTimeout(100)
  assert.equal(f.editor.plainText, "")
  assert.deepEqual(f.notes, [])
})

test("the controller deadline fences a late focus change", async (t) => {
  const f = await fixture(t)
  f.editor.focused = false
  await f.approve(undefined, Date.now() + 150)
  await f.pluginModule.tui(f.api)
  await setTimeout(250)
  f.editor.focused = true
  await setTimeout(100)
  assert.equal(await f.claimed(), false)
  assert.deepEqual(f.notes, [])
})
