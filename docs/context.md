# Context

## The window

Every model in your ChatGPT account's catalog reports two sizes: the size it recommends working in (272K for most, 500K for the 6.1 models) and the most this account can send it (`max_context_window`, 872K today). Rhea uses the **maximum** as the model's context window, so the meter and the Context panel show the full size. If the catalog reports no maximum, it falls back to its recommended size, and then to the size published on OpenAI's model pages.

The panel and Settings > Chat & model also show what OpenAI publishes for the model: the **maximum reply** (128K for every current model) and the **API window** (1.05M for the GPT-6, GPT-5.6, GPT-5.5 and GPT-5.4 families, 400K for the mini/nano and older GPT-5 models). The API window is larger than the ChatGPT route allows, so it is shown for reference and not used for the meter or auto-compact. The published figures come from https://developers.openai.com/api/docs/models and are kept in `documented_limits` in `src/chat.rs`.

The meter beside the model picker and the Context panel always use the selected model's window. When you change the model, the window, the ring and every number animate to the new values. Inside a subagent's chat the panel shows that agent's own context and its own model's window.

The panel splits the used share into system prompt, tools & MCP, **memory** (lime), and messages & results. The lime Memory row and ring segment show what long-term memory added to the latest request: its tokens, its share of the window, and how many memories it carried. It is empty for a message that recalled nothing, and lists the latest request's input and output, cache hits and writes, and running totals. Service-reported numbers are labeled; local estimates are kept separate. Each chat keeps its own numbers across new messages and restarts.

## How a chat continues

The ChatGPT Codex backend does not store conversations (`store:true` is refused), so each request sends the whole transcript again, exactly as the Codex CLI does, together with a stable per-chat session id so the prompt cache hits. Rhea keeps the model's own transcript of each chat (messages, tool calls and their results, and which tools were loaded) in the chat's folder as `model.json`, and subagents keep theirs as `agent-<id>.json`. A message therefore continues from what the model actually saw, with no re-loading of tools.

These files are read once when a chat is used and kept in memory so a message does not reparse them. Every save writes a temporary file and swaps it into place, so a crash or a full disk leaves the previous intact file, and a failed save is shown in the status line instead of being ignored. Five seconds after you leave a chat the cached copy is dropped. If you edit or regenerate a message, the transcript no longer matches and is rebuilt from the visible turns.

## Auto-compact and `/compact`

`/compact` shows up in the chat like a message: your `/compact` on the right, and on Rhea's side a live line, "Compacting… it's been 0m 12s", that counts up while the summary is written. When it finishes, that line becomes "Compacted 48.2K tokens", with a note of how large the summary is and that the last exchange stays word for word. If it fails, the line says so with the reason, and if there is nothing older than the last exchange it says "Nothing to compact yet." These lines are part of the chat's saved history but are not messages to the model, so they never change what it sees.

When a request would fill more than the **auto-compact** share of the window (Settings > Chat & model, 15–95%, default 80%), older turns are summarized first; the last two turns stay verbatim. `/compact` does it on demand, and the transcript shows where it happened.
