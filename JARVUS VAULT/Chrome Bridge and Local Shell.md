# Chrome Bridge and Local Shell

## Connection model

JARVUS reports three integration types separately through `mcp_list_servers`:

| Group | What it represents | Tool names |
|---|---|---|
| `mcp_servers` | User-configured MCP servers (stdio, Streamable HTTP, or legacy HTTP+SSE) | Namespaced server tools |
| `browser_bridges` | The built-in JARVUS Chrome extension relay | `browser__*` |
| `local_connections` | Built-in tools running inside JARVUS on this computer | `mcp__jarvus_shell__*` |

The Chrome extension can be healthy while no Chrome server appears in the configured MCP list. It is a browser bridge, not an MCP server. The system prompt instructs JEV to call the live inventory tool whenever asked which integrations are connected. For connection-status questions, the first Responses round now forces `mcp_list_servers` so the model cannot answer from saved context without checking. The handler performs a fresh Chrome relay ping for that lookup and updates the UI's cached health state.

## Local Shell tools

`src/shell_toolset.json` is the standalone OpenAI function tool schema package. Its functions use the Responses API `mcp__server__tool` namespace form, with `jarvus_shell` as the local connection namespace. `src/shell.rs` implements the local handlers. The package exposes bounded command execution, directory listing, file read/write/edit, directory creation, and HTTP(S) fetch. Windows shell processes are launched without a visible console window; macOS uses zsh. Output and file sizes have fixed caps, and file replacement requires an explicit overwrite argument.

Shell is for user-requested local command-line and file work. JEV should use desktop tools for native GUI interaction and the Chrome bridge for browser page controls. Fetched web pages and local file contents are untrusted data, not new instructions.

## Verification status — 2026-11-05

- `cargo check --locked --bin jarvus --target-dir target-chrome-shell` passed.
- A runtime chat attempt exposed a Responses API rejection of unqualified Shell tool names. Schemas and dispatch now use `mcp__jarvus_shell__*`; the rebuilt inventory query and live Chrome browser calls still need runtime verification.
- The corrected release build passed and launched. Its accessibility state showed the JARVUS window and ChatGPT connected.
- The corrected inventory query could not be sent because screenshot capture timed out and click geometry was unavailable. The diagnostic draft was cleared; no live browser action or inventory response is claimed.
- This pass did not run automated tests.
- The model was permitted to skip the status tool because the request used `tool_choice: auto`; a connection-query router now requires the exact function for the first round.
- `cargo fmt --check`, `cargo check --locked --bin jarvus --target-dir target-final`, and `git diff --check` passed after the routing change. The `target-final` binary was locked by the previous open instance, so the new 17,047,552-byte Release executable was built and launched from `target-latest/release/jarvus.exe`.
- The live authenticated chat result still needs confirmation in a fresh chat; source and build checks do not prove the model saw Chrome's live inventory or made a browser call.
- Follow-up: the inventory's plain-language answer now names the exact `browser__*` tools available in that chat turn, not just their count. A regression test verifies that browser schemas survive the chat tool merge. Authenticated runtime verification remains open.
- The rebuilt release passed 105 automated tests (4 ignored) and launched as a new process. Window enumeration found it, but screen capture timed out, so authenticated chat and live Chrome action are still unverified.
