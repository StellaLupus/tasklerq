# Tasklerq — project memory

This repository has its own isolated Mnemosyne bank: `tasklerq`.

- The `mnemosyne` MCP server in `opencode.json` is pinned to `--bank tasklerq`
  through `mnemosyne-guard` (`tools/mnemosyne-guard`), a stdio proxy that forces
  `bank=tasklerq` on every tool call and hides `mnemosyne_shared_*` /
  `mnemosyne_sync_*`. Install it with
  `cargo install --path tools/mnemosyne-guard --root ~/.local`.
- Project memory is injected automatically at the start of a session.
- Use `mnemosyne_recall` for additional lookups during the session.
- Store durable project facts, decisions, conventions, and gotchas with
  `mnemosyne_remember` using `scope=global` and a `source` such as `project`,
  `decision`, or `convention`.
- Never read from or write to another project's bank. Do not use `default`,
  `ds`, or any other bank for this repository.
- Do not store ephemeral task state, secrets, tokens, or raw logs.
- Update or invalidate outdated memories instead of creating duplicates.
