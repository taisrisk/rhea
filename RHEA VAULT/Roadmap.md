# Roadmap

## Foundation — native app, sign-in, chat, and local tools

- [x] Create the standalone Rust desktop app and Rhea vault.
- [x] Integrate Agent Desktop and UI Automation snapshots, local debug artifacts, and semantic refs.
- [x] Implement Codex device authorization and local credential storage.
- [x] Add account model catalog, streaming chat, local Rhea tool dispatch, and tool-result continuation.
- [x] Persist chat history, tool traces, and selected model.
- [x] Add context and cache usage display, labeled by estimate versus service-reported values.
- [x] Add an interactive-only Agent Desktop snapshot option; keep full useful output as the default.
- [x] Add optional local snapshot filters (`all`, `buttons`, `editables`, `links`, `checkables`, `text`, `containers`, `other`) and bounded depth/compact controls to the tool schema and Rhea contract.
- [x] Require focus confirmation before typing and stop rather than acting on stale refs.
- [x] Bound MCP stdio frames, requests, tool metadata pagination, aggregate schema, and environment configuration; restart legacy servers that reject the discovery probe.
- [x] Add MCP edit/enable/disable/test/remove/reload controls and masked secret environment editing through Credential Manager.
- [x] Add Settings connection actions and local conversation/top-level Downloads inventory with confirmed file removal.
- [x] Expand failed tool activity by default and keep its error details selectable.
- [x] Supervise research and multi-step plans by returning bounded Rhea evidence to ChatGPT; prevent plan promises or Rhea completion flags from ending measurable tasks.
- [x] Add Streamable HTTP and legacy HTTP+SSE MCP transports alongside stdio, with live settings reload and secret-safe display.
- [x] Split each conversation into `metadata.json`, `history.json`, and `toolcalls.json`; load transcripts on open and unload inactive transcripts after five seconds.
- [x] Add macOS Agent Desktop adapter, Chrome native-host paths, and Agent Desktop permission actions in Settings.
- [x] Add macOS x64/Apple Silicon GitHub Actions builds and a Windows version/build/release-staging script.
- [x] Hide account email in the titlebar connection chip; account identity remains available in Settings.

## Current repair — native UI and interaction reliability

- [x] Stabilize layout allocations for completed versus streaming messages.
- [x] Enable egui clipboard support and use its native copy path.
- [x] Add Enter-to-send with Shift+Enter for a new line and a green caret.
- [x] Extend resize hit-testing to the viewport edge.
- [x] Make context details scrollable and size them to the available panel.
- [x] Replace patterned glow with a radial fade and add a green-tinted panel shadow.
- [x] Remove redundant chat heading and bottom-right connection footer; move model selection into the composer area.
- [x] Configure the Windows GUI subsystem (`PE subsystem 2`) for the Rhea executable.
- [x] Build optimized Windows source into the ignored `target-final/release/rhea.exe`; the normal `target/release/rhea.exe` was locked by an existing process and could not be replaced.
- [ ] Launch the latest `target-final/release/rhea.exe` and confirm no console window appears; do not infer this from the GUI subsystem bit alone.
- [ ] Launch and inspect the latest binary in a fresh app window. Computer Use capture timed out twice and the second attempt was stopped with Escape on 2026-11-04.
- [ ] Verify resize, copy, text selection, Enter/Shift+Enter, panel clipping, and message alignment in the live app.
- [ ] Verify the top seam is absent on both restored and maximized window states.
- [ ] Confirm a real authorized ChatGPT response and a local Rhea tool action in the rebuilt app. No live model request was sent in the 2026-11-04 repair pass.
- [ ] Install and verify the Chrome extension/native host with the user's browser permission workflow.
- [x] Implement and mock-test remote MCP Streamable HTTP and legacy SSE; a real server connection remains unverified.
- [ ] Run the macOS GitHub Actions matrix and verify launch, Keychain sign-in, Accessibility permission, Chrome native messaging, and a harmless desktop action on an actual Mac.
- [ ] Add a real current-app screenshot to README after a fresh visual capture succeeds; keep the existing screenshot marker pending until then.
- [ ] Configure the GitHub origin before attempting a push or release upload. No remote exists in this checkout.
- [ ] Add project/file and generated-artifact inventory APIs before claiming full local file inventory or cleanup support.
- [ ] Visually resolve the top hairline using a fresh screenshot; source inspection cannot distinguish DWM framing from a client seam.

## Next

- [ ] Complete end-to-end desktop workflows with a fresh snapshot between every UI-changing step and explicit confirmation for consequential actions.
- [ ] Improve process/window targeting and app startup search without weakening target validation.
- [ ] Add voice input/output after the text and action loop is dependable.
- [ ] Add local screenshot-to-layout as a fallback for apps with insufficient UI Automation metadata.
- [ ] Validate cancellation, error recovery, context compaction, and cache accounting across long conversations.

## Release rule

Mark a feature done only after its stated acceptance check passes. Builds prove compilation; they do not prove live OAuth, model output, desktop input, or visual correctness.
