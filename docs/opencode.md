# OpenCode as a second provider

Settings > Connections has an **OpenCode** row next to ChatGPT. Paste an API key from
[opencode.ai/console](https://opencode.ai/console) (OpenCode's own site calls it "Console", not the
older "Zen" name some third-party guides still use), click **Connect**, and OpenCode's models join
the model picker under their own divider, below ChatGPT's. The context window shown next to each
model's name, in muted gray, is read from OpenCode's own catalog the same way it is for ChatGPT.

**Switching models never loses context.** Rhea keeps a chat's whole transcript locally - never on
either provider's servers - and resends it with every request (see [Context](context.md)). Picking
an OpenCode model after a run of ChatGPT turns, or the other way around, just means the next
request is translated into whichever provider's wire format that model needs; nothing about the
conversation itself depends on which provider answered an earlier turn.

## What's unverified

This integration was built from OpenCode's own published docs, not tested against a live account
or API key - the same honesty bar Rhea holds fast mode to. Specifically unverified:

- **The exact model-list response shape.** The docs show the endpoint (`GET
  /inference/v1/models`) but not its JSON schema. Rhea reads `models`, `data`, or a bare array, and
  each entry's `id`/`slug` and `name`/`display_name`; if OpenCode's real response uses different
  keys, the catalog comes back empty with a clear error rather than silently wrong data.
- **Tool-calling against OpenCode's actual gateway.** The translation between Rhea's internal tool
  schema and Chat Completions' `tool_calls` format is tested against a scripted server that mimics
  the documented wire shape (see `src/replay.rs`), not against OpenCode itself.
- **Go/Pro plan behavior.** OpenCode's docs describe a "Go" subscription with included usage; a
  "Pro" tier was not found on the page read while building this. Both likely affect which models
  are available and how usage is billed, not the wire format Rhea speaks to.

If a request to an OpenCode model fails in a way that looks like a wire-format mismatch (not a
plain auth or network error), that is the first thing to suspect.

## What this does not do

- Fast mode (priority processing) is a ChatGPT-only concept here; there is no verified equivalent
  on OpenCode's side, so the fast-mode toggle has no effect on OpenCode models.
- Subagents inherit the main chat's provider, not a per-agent choice - a team of subagents all use
  whichever provider the main chat's selected model belongs to.
