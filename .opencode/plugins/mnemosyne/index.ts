import { Plugin } from "@opencode/plugin"
import { execFile } from "node:child_process"
import { readFile, unlink } from "node:fs/promises"
import { homedir, tmpdir } from "node:os"
import { join } from "node:path"

/**
 * Project-scoped Mnemosyne auto-recall.
 *
 * The bank name is deliberately fixed per repository. This keeps the
 * long-term memory of this project isolated from every other project that
 * uses the same local Mnemosyne installation.
 */
const BANK = "tasklerq"
const MNEMOSYNE_BIN =
  process.env.MNEMOSYNE_BIN || `${homedir()}/.local/bin/mnemosyne`
const TOP_K = 12
const MAX_CONTEXT_CHARS = 5000

type RecallResult = {
  id?: string
  content?: string
  score?: number
}

function runMnemosyne(args: string[]): Promise<string> {
  return new Promise((resolve, reject) => {
    execFile(
      MNEMOSYNE_BIN,
      args,
      {
        env: {
          ...process.env,
          MNEMOSYNE_BANK: BANK,
          MNEMOSYNE_MCP_BANK: BANK,
        },
        timeout: 20_000,
        maxBuffer: 8 * 1024 * 1024,
      },
      (error, stdout) => {
        if (error) {
          reject(error)
          return
        }
        resolve(stdout)
      },
    )
  })
}

function messageText(message: any): string {
  if (!message) return ""

  const content = message.parts ?? message.content ?? message.message?.content
  if (typeof content === "string") return content

  if (Array.isArray(content)) {
    return content
      .filter(
        (part: any) =>
          part &&
          typeof part.text === "string" &&
          (part.type === "text" ||
            part.type === "input_text" ||
            part.type === undefined),
      )
      .map((part: any) => part.text)
      .join("\n")
  }

  return typeof message.text === "string" ? message.text : ""
}

function roleOf(message: any): string {
  return message?.role ?? message?.info?.role ?? ""
}

function lastUserText(messages: any[]): string {
  for (let i = messages.length - 1; i >= 0; i--) {
    if (roleOf(messages[i]) !== "user") continue
    const text = messageText(messages[i]).trim()
    if (text) return text
  }
  return ""
}

function isUsable(row: any, today: string): boolean {
  if (!row || typeof row.content !== "string" || !row.content.trim()) return false
  if (row.superseded_by) return false
  if (row.valid_until && String(row.valid_until).slice(0, 10) < today) return false
  // Only durable, project-wide memories. Session-scoped notes belong to the
  // session that created them and must not leak into another conversation.
  return (row.scope ?? "global") === "global"
}

/**
 * Load the curated project memory baseline.
 *
 * Query-based recall is intentionally not used for the automatic bootstrap:
 * Mnemosyne's semantic search is sensitive to morphology and imperative
 * phrasing ("назови ..." can return nothing for a memory stored as
 * "финальная фраза ..."). A small, curated project bank is better injected by
 * importance, while targeted lookups remain available through mnemosyne_recall.
 */
async function loadBootstrap(): Promise<RecallResult[]> {
  const exportPath = join(
    tmpdir(),
    `mnemosyne-${BANK}-${process.pid}-${Date.now()}.json`,
  )

  try {
    await runMnemosyne(["export", exportPath])
    const payload = JSON.parse(await readFile(exportPath, "utf8"))
    const rows: any[] = Array.isArray(payload.working_memory)
      ? payload.working_memory
      : []
    const today = new Date().toISOString().slice(0, 10)
    const seen = new Set<string>()

    return rows
      .filter((row) => {
        if (!isUsable(row, today)) return false
        const key = String(row.id ?? row.content)
        if (seen.has(key)) return false
        seen.add(key)
        return true
      })
      .sort((a, b) => {
        const importance =
          (Number(b.importance) || 0) - (Number(a.importance) || 0)
        if (importance !== 0) return importance
        return String(b.timestamp ?? "").localeCompare(String(a.timestamp ?? ""))
      })
      .slice(0, TOP_K)
      .map((row) => ({
        id: String(row.id ?? ""),
        content: String(row.content),
        score: Number(row.importance) || 0,
      }))
  } finally {
    await unlink(exportPath).catch(() => {})
  }
}

function recallByQuery(query: string): Promise<RecallResult[]> {
  return runMnemosyne([
    "recall",
    query.slice(0, 1500),
    String(TOP_K),
    "--json",
  ]).then((stdout) => {
    const payload = JSON.parse(stdout)
    return Array.isArray(payload.results) ? payload.results : []
  })
}

function render(results: RecallResult[]): string {
  const lines: string[] = []

  for (const result of results) {
    const content = String(result.content ?? "").trim()
    if (!content) continue

    const id = result.id ? ` [id=${result.id}]` : ""
    const score =
      typeof result.score === "number" ? ` (importance ${result.score.toFixed(2)})` : ""
    lines.push(`- ${content}${id}${score}`)
  }

  let body = lines.join("\n")
  if (body.length > MAX_CONTEXT_CHARS) {
    body = `${body.slice(0, MAX_CONTEXT_CHARS)}\n… (обрезано)`
  }

  return [
    `<project_memory bank="${BANK}" source="mnemosyne">`,
    `Автоматически загруженная долговременная память проекта (bank=${BANK}).`,
    `Она относится только к текущему репозиторию. Не смешивай её с памятью других проектов.`,
    `Устаревшие факты обновляй или инвалидируй через инструменты mnemosyne; новые устойчивые факты и решения сохраняй в bank=${BANK} со scope=global.`,
    "",
    body,
    "</project_memory>",
  ].join("\n")
}

export default Plugin.define({
  id: "tasklerq.mnemosyne",
  async setup(ctx) {
    // One automatic bootstrap per session. Targeted lookups during the
    // session use the mnemosyne MCP tools.
    const injected = new Set<string>()

    const registration = await ctx.session.hook("context", async (event) => {
      if (event.kind && event.kind !== "primary") return

      const sessionID = event.sessionID
      if (!sessionID || injected.has(sessionID)) return

      // Mark before awaiting so parallel/retried model calls cannot duplicate
      // the same bootstrap.
      injected.add(sessionID)

      try {
        let results = await loadBootstrap()

        if (results.length === 0) {
          const query = lastUserText(event.messages as any[])
          if (query) results = await recallByQuery(query)
        }

        if (results.length === 0) return
        event.system.push({ type: "text", text: render(results) })
      } catch (error) {
        console.warn(`[mnemosyne] bootstrap failed for bank ${BANK}:`, error)
      }
    })

    return () => registration.dispose()
  },
})
