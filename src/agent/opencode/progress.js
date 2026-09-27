// OpenCode 2 plugins export a definition and subscribe to live events during setup. The adapter
// fills in the report path, which remains inside this role's harness.
import { appendFileSync } from "node:fs"

const REPORT = "__REPORT__"

const note = (kind) => {
  try {
    appendFileSync(REPORT, JSON.stringify({ kind, at: Date.now() }) + "\n")
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
      for await (const update of event.subscribe({ signal: stop.signal })) {
        note(update.type)
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
