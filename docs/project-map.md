# Project map

| Path | Purpose |
| --- | --- |
| `src/app.rs`, `src/app/` | The window: chat, history, settings pages (`chrome_ui`, `laya_ui`, `archive_ui`, `team_ui`), motion helpers (`motion`) |
| `src/chat.rs` | Model catalog, streamed responses, tool-call loop, context handling |
| `src/opencode.rs` | OpenCode as a second provider: API key auth, model catalog, Chat Completions wire translation |
| `src/chat_store.rs` | Per-chat files, the open-file cache, archives, migration |
| `src/jev.rs` | Local tool routing and bounded desktop plans |
| `src/usage.rs` | Lifetime usage record (`usage.json`): daily totals, streaks, activity grid, token breakdown |
| `src/ledger.rs` | The per-chat run ledger (`ledger.jsonl`) and its summaries |
| `src/research.rs` | Research evidence log, the completion gate and the blocker reply |
| `src/replay.rs` | Replay tests: the chat loop against a scripted model server |
| `src/desktop.rs` | Agent Desktop adapter, snapshots, actions |
| `src/macos.rs`, `src/uia.rs` | Native macOS input and window layer; Windows UI Automation |
| `src/mcp.rs` | MCP configuration, connections, tool dispatch |
| `src/browser.rs`, `src/chrome_ext.rs`, `chrome-extension/` | Chrome bridge, its one-click install, the extension |
| `src/laya.rs` | The local decision model: private runtime, server, `laya_decide` |
| `src/skills.rs`, `src/hooks.rs` | Skills (discovery, install, dedupe) and hooks (including skill hooks) |
| `src/team.rs`, `src/tools.rs`, `src/compose.rs` | Subagents, tool dispatch, Compose mode |
| `src/memory/` | Long-term memory: store, embeddings, ranking, extraction, briefing, tools |
| `src/spelling.rs` | Spell check and autocorrect |
| `src/installer.rs` | Self-install and updates |
| `src/auth.rs` | ChatGPT sign-in and credential storage |
| `src/system_prompt.md`, `src/system_prompt_macos.md`, `src/toolset.json` | The operating manual (two editions) and the model-facing tool schemas |
| `packaging/macos/`, `.github/workflows/macos.yml`, `build.ps1` | Packaging and release |
| `assets/` | Icons and the spelling dictionary |
| `docs/` | These pages and the screenshots |
| `RHEA VAULT/` | Obsidian project notes |
