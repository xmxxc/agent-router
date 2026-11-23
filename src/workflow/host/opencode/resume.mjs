import { spawn } from "node:child_process"
import { createHash } from "node:crypto"
import { constants } from "node:fs"
import { open, unlink } from "node:fs/promises"
import { dirname, isAbsolute, join } from "node:path"
import { fileURLToPath } from "node:url"

async function readPrivate(url, limit) {
  const file = await open(url, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK)
  try {
    const info = await file.stat()
    if (!info.isFile() || (info.mode & 0o777) !== 0o600 || info.nlink !== 1 ||
        info.uid !== process.geteuid() || info.size > limit) throw new Error("Invalid resume file")
    return await file.readFile("utf8")
  } finally {
    await file.close()
  }
}

async function takeCapability(url) {
  const capability = await readPrivate(url, 64)
  if (!/^[a-f0-9]{64}$/.test(capability)) return
  await unlink(url)
  return capability
}

function focusedPrompt(api, launch) {
  const route = api.route.current
  const session = api.state.session.get(launch.session)
  const status = api.state.session.status(launch.session)
  const editor = api.renderer.currentFocusedRenderable
  if (route.name === "session" && route.params?.sessionID === launch.session &&
      api.state.ready && api.state.path.directory === launch.cwd &&
      session?.id === launch.session && session.directory === launch.cwd && !session.parentID &&
      !api.ui.dialog.open && (!status || status.type === "idle") &&
      api.state.session.permission(launch.session).length === 0 &&
      api.state.session.question(launch.session).length === 0 &&
      editor?.focused && !editor.isDestroyed && editor.plainText === "" &&
      typeof editor.setText === "function" &&
      editor.traits?.owner === "opencode" && editor.traits.role === "prompt" &&
      !editor.traits.status && !editor.traits.capture?.includes("submit")) {
    return editor
  }
}

function claim(bin, env, launch) {
  return new Promise((resolve) => {
    const child = spawn(bin, ["--opencode-resume-ready"], {
      cwd: launch.cwd, env, stdio: ["pipe", "ignore", "ignore"],
    })
    child.on("error", () => resolve(false))
    child.on("close", (code) => resolve(code === 0))
    child.stdin.on("error", () => {})
    child.stdin.end(JSON.stringify(launch))
  })
}

export default {
  id: "wfc-opencode-resume",
  async tui(api) {
    const { WFC_STATE, WFC_DISPATCH, WFC_ATTEMPT, WFC_ROUND, WFC_BIN,
      WFC_WORKER_CALLBACK, OPENCODE_DB } = process.env
    if (api.app.version !== "1.19.5" || !WFC_STATE || !WFC_BIN ||
        !isAbsolute(WFC_STATE) || !isAbsolute(WFC_BIN) || !WFC_WORKER_CALLBACK ||
        !/^d-[a-f0-9]{32}$/.test(WFC_DISPATCH) || !/^a-[a-f0-9]{32}$/.test(WFC_ATTEMPT) ||
        dirname(fileURLToPath(import.meta.url)) !==
          join(WFC_STATE, `opencode-${WFC_DISPATCH}-${WFC_ATTEMPT}`)) return
    let wire, plan
    try {
      wire = await readPrivate(new URL("resume.json", import.meta.url), 64 * 1024)
      plan = JSON.parse(wire)
    } catch {
      return
    }
    const { launch, brief } = plan
    const digest = createHash("sha256").update(wire).digest("hex")
    if (launch.dispatch !== WFC_DISPATCH || launch.attempt !== WFC_ATTEMPT ||
        launch.round !== WFC_ROUND || launch.database !== OPENCODE_DB || launch.cwd !== process.cwd() ||
        !/^ses_[a-zA-Z0-9_-]+$/.test(launch.session) || typeof brief !== "string" || !brief) return

    // The TUI continues startup while the controller verifies the tmux receipt.
    let busy = false
    // A missing approval can mean tmux's launch result was lost.
    const approvalDeadline = Date.now() + 30_000
    let permit
    const timer = setInterval(() => {
      if (Date.now() >= (permit?.expiresAt ?? approvalDeadline) ||
          api.lifecycle.signal.aborted) return clearInterval(timer)
      if (busy) return
      busy = true
      void (async () => {
        if (!permit) {
          const candidate = JSON.parse(await readPrivate(new URL("permitted", import.meta.url), 256))
          if (candidate.planSha256 !== digest || !Number.isSafeInteger(candidate.expiresAt)) return
          permit = candidate
        }
        if (Date.now() >= permit.expiresAt) return
        const editor = focusedPrompt(api, launch)
        if (!editor) return
        clearInterval(timer)
        const capability = await takeCapability(new URL("wfc-adoption.capability", import.meta.url))
        if (!capability || api.lifecycle.signal.aborted) return
        const claimed = await claim(WFC_BIN, {
          WFC_STATE, WFC_DISPATCH, WFC_ATTEMPT, WFC_ROUND, WFC_WORKER_CALLBACK, OPENCODE_DB,
          WFC_OPENCODE_ADOPTION: capability,
        }, launch)
        // The focus or draft may have changed while the claim was in flight.
        // No second attempt is allowed after the claim, even on a lost reply.
        if (!claimed || Date.now() >= permit.expiresAt ||
            api.lifecycle.signal.aborted || focusedPrompt(api, launch) !== editor) return
        editor.setText(brief)
        if (api.renderer.currentFocusedRenderable === editor && editor.plainText === brief) {
          await api.keymap.dispatchCommand("prompt.submit")
        }
      })().catch(() => {}).finally(() => { busy = false })
    }, 50)
    api.lifecycle.onDispose(() => clearInterval(timer))
  },
}
