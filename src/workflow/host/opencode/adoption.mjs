import { spawn } from "node:child_process"
import { createHash } from "node:crypto"
import { constants } from "node:fs"
import { open, unlink } from "node:fs/promises"
import { dirname, isAbsolute, join } from "node:path"
import { fileURLToPath } from "node:url"

function matchesText(part, digest) {
  return part?.type === "text" && !part.synthetic && !part.ignored &&
    typeof part.text === "string" &&
    createHash("sha256").update(part.text).digest("hex") === digest
}

export default async function adoption({ directory }, { briefSha256, cwd } = {}) {
  const { WFC_STATE, WFC_DISPATCH, WFC_ATTEMPT, WFC_ROUND, WFC_BIN,
    WFC_WORKER_CALLBACK, OPENCODE_DB } = process.env
  const database = fileURLToPath(new URL("opencode.db", import.meta.url))
  if (!WFC_STATE || !isAbsolute(WFC_STATE) || !WFC_BIN || !isAbsolute(WFC_BIN) ||
      !/^d-[a-f0-9]{32}$/.test(WFC_DISPATCH) || !/^a-[a-f0-9]{32}$/.test(WFC_ATTEMPT) ||
      !/^[a-f0-9]{64}$/.test(briefSha256) || !WFC_WORKER_CALLBACK || WFC_ROUND !== "1" ||
      typeof directory !== "string" || directory !== cwd || !isAbsolute(directory) ||
      database !== OPENCODE_DB ||
      dirname(database) !== join(WFC_STATE, `opencode-${WFC_DISPATCH}-${WFC_ATTEMPT}`)) {
    return {}
  }

  // Only the plugin instance for this worktree claims the startup capability.
  // Keep it in this closure and pass it only to the private receiver.
  const capabilityFile = new URL("wfc-adoption.capability", import.meta.url)
  let capability
  try {
    const file = await open(capabilityFile, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK)
    try {
      const info = await file.stat()
      if (!info.isFile() || info.nlink !== 1 || (info.mode & 0o777) !== 0o600 ||
          info.uid !== process.geteuid() || info.size !== 64) return {}
      capability = await file.readFile("utf8")
    } finally {
      await file.close()
    }
    if (!/^[a-f0-9]{64}$/.test(capability)) return {}
    await unlink(capabilityFile)
  } catch {
    return {}
  }

  let opening
  let submitted = false
  return {
    "chat.message"(input, output) {
      const message = output.message
      if (opening || message?.role !== "user" || message.sessionID !== input.sessionID ||
          !/^ses_[a-zA-Z0-9_-]+$/.test(message.sessionID) ||
          !/^msg_[a-zA-Z0-9_-]+$/.test(message.id) ||
          (input.messageID !== undefined && input.messageID !== message.id)) return
      const parts = output.parts.filter((part) => part.type === "text" && !part.synthetic && !part.ignored)
      if (parts.length !== 1 || !matchesText(parts[0], briefSha256) ||
          typeof parts[0].id !== "string") return
      opening = { session: message.sessionID, firstMessage: message.id, part: parts[0].id }
    },
    event({ event }) {
      // The 1.19.5 v1 event is published after its durable text part is committed.
      if (!opening || submitted || event.type !== "message.part.updated") return
      const { sessionID, part } = event.properties
      if (sessionID !== opening.session || part?.sessionID !== opening.session ||
          part.messageID !== opening.firstMessage || part.id !== opening.part ||
          !matchesText(part, briefSha256)) return
      submitted = true
      const child = spawn(WFC_BIN, ["--opencode-adopt"], {
        cwd: directory,
        env: { WFC_STATE, WFC_DISPATCH, WFC_ATTEMPT, WFC_ROUND, WFC_WORKER_CALLBACK, OPENCODE_DB,
          WFC_OPENCODE_ADOPTION: capability },
        stdio: ["pipe", "ignore", "ignore"],
      })
      child.on("error", () => {})
      child.on("close", (code) => { submitted = code === 0 })
      child.stdin.on("error", () => {})
      child.stdin.end(JSON.stringify({
        dispatch: WFC_DISPATCH, attempt: WFC_ATTEMPT, cwd: directory, database,
        session: opening.session, firstMessage: opening.firstMessage,
      }))
    },
  }
}
