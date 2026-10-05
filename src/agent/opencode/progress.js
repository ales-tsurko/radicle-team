// OpenCode 2 plugins export a definition and subscribe to live events during setup. The adapter
// fills in the report path, which remains inside this role's harness.
import { appendFileSync } from "node:fs"

const REPORT = "__REPORT__"

const note = (kind, tool, detail) => {
  try {
    appendFileSync(REPORT, JSON.stringify({ kind, tool, detail, at: Date.now() }) + "\n")
  } catch {
    // Missing reports let the runtime detect a stalled or broken harness.
  }
}

export default {
  id: "radicle-team-progress",
  setup: async ({ event, model, rpc }) => {
    // RPC requests wait for all plugins to activate, so this sees the complete model registry.
    // Return only the requested model and the fields Rust needs to keep CLI output bounded.
    await rpc.register({
      id: "radicle-team",
      methods: {
        models: {
          input: {
            type: "object",
            properties: { model: { type: "string" } },
            required: ["model"],
          },
          output: { type: "array", items: { type: "object" } },
        },
      },
      events: {},
    }, {
      models: async (input) => {
        const { data } = await model.list()
        return data
          .filter((item) => `${item.providerID}/${item.id}` === input.model)
          .map(({ id, providerID, enabled, variants }) => ({
            id, providerID, enabled, variants: variants.map(({ id }) => ({ id })),
          }))
      },
    })

    note("started")
    const stop = new AbortController()
    const reports = (async () => {
      const observed = new Set([
        "session.step.started", "session.reasoning.started", "session.reasoning.delta",
        "session.text.started", "session.text.delta", "session.tool.called",
        "session.tool.progress", "session.tool.success", "session.tool.failed",
        "session.tool.input.started", "session.tool.input.delta", "session.retry.scheduled",
        "session.compaction.started", "session.compaction.delta", "session.step.failed",
      ])
      let session
      let deltaAt = 0
      const tools = new Map()
      const progress = new Map()
      for await (const update of event.subscribe({ signal: stop.signal })) {
        if (!observed.has(update.type)) continue
        const directory = update.location?.directory
        if (directory && directory !== process.cwd()) continue
        const data = update.data ?? update.properties ?? {}
        const id = data.sessionID ?? data.sessionId
        if (session && id && session !== id) continue
        session ??= id
        if (update.type.endsWith(".delta")) {
          const now = Date.now()
          if (now - deltaAt < 1000) continue
          deltaAt = now
        }
        if (update.type === "session.tool.input.started") tools.set(data.id, data.name)
        if (update.type === "session.tool.progress") {
          const metadata = JSON.stringify(data.metadata)
          if (progress.get(data.id) === metadata) continue
          progress.set(data.id, metadata)
        }
        const tool = tools.get(data.id)
        const detail = data.error?.message
        note(update.type, typeof tool === "string" ? tool.slice(0, 100) : undefined,
          typeof detail === "string" ? detail.slice(0, 400) : undefined)
        if (update.type === "session.tool.success" || update.type === "session.tool.failed") {
          tools.delete(data.id)
          progress.delete(data.id)
        }
      }
    })().catch(() => {
      // Plugin shutdown or a broken stream must not fail the agent's delivery.
    })

    return async () => {
      stop.abort()
      await reports
    }
  },
}
