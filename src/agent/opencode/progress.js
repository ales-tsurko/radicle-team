// Tells the runtime what this member's agent is doing.
//
// The runtime stops an agent which has shown nothing for fifteen minutes, and from outside the
// process a working agent and a stuck one look the same. OpenCode knows the difference, so this
// plugin writes what it knows to a file the adapter reads. The path is written in when the adapter
// installs this file, and every line is one event.
//
// The names below come from `@opencode-ai/plugin`: `event` carries everything OpenCode announces,
// including `session.idle`, `session.error` and `message.part.updated`, while the tool hooks fire
// around a tool call and `permission.ask` fires when a run wants an answer nobody is there to give.
//
// A member's own harness folder is the only thing this touches. It never fails a run: an agent
// which cannot report is still an agent doing its work.

import { appendFileSync } from "node:fs"

const REPORT = "__REPORT__"

const note = (kind, detail) => {
  try {
    appendFileSync(REPORT, JSON.stringify({ kind, at: Date.now(), detail }) + "\n")
  } catch {
    // The runtime reads silence as a stall, which is the right answer when reporting is broken.
  }
}

export const Progress = async () => {
  note("started")

  return {
    event: async ({ event }) => note(event?.type),
    "tool.execute.before": async (input) => note("tool.before", input?.tool),
    "tool.execute.after": async (input) => note("tool.after", input?.tool),
    "permission.ask": async (_input, output) => note("permission.ask", output?.status),
  }
}
