# Desktop Understanding Pipeline

## Current implementation decision

JARVUS uses [Vercel agent-desktop](https://github.com/vercel-labs/agent-desktop) for screen understanding and interaction. It reads Windows UI Automation (UIA) metadata: control role, accessible name/value, measured bounds, available actions, window identity, and short-lived refs. This is the desktop engine; JARVUS does not classify controls from pixel shapes or run OCR itself.

The Rust UI calls the installed native CLI and displays its structured JSON envelope. A snapshot is fresh and refs are bound to that snapshot. After the UI changes, take a new snapshot and use its new refs. The app supports an app/process filter and surfaces such as focused/window/desktop/start-menu/taskbar.

## Local visual-debug output

The optional **SNAPSHOT + LOCAL VISUAL MAP** command asks agent-desktop for its HTML debug visualization. It is written under `JARVUS/captures/agent-desktop/` and stays on this PC. agent-desktop's debug renderer currently requires the `window` surface; JARVUS selects that surface automatically for this command. The normal snapshot respects the selected surface.

## When UIA does not describe a screen

Some applications expose incomplete or no accessibility metadata. JARVUS should report the limitation and offer a separate, explicit visual fallback only when needed. Any future fallback can use a local image/vision pipeline or a vision-capable AI request with clear user control. Never fabricate button/card labels from geometry. OCR and shape detection are not the current source of truth.

## Why

The prior local rectangle/OCR prototype produced many noisy text boxes on a labeled test window and required heavyweight inference. The live agent-desktop bridge exposes the actual control tree, text labels, positions, and actionable references without sending screenshots to an AI service. See [[Decision Log]] and [[Screen Map JSON Schema]].

## Current test evidence

- `agent-desktop status` reported `platform: windows` and `accessibility: granted`.
- With AccessKit enabled in eframe, an agent-desktop snapshot of JARVUS returned the JARVUS controls and measured bounds.
- Pressing the local visual-map button through an agent-desktop ref wrote an HTML debug artifact under `captures/agent-desktop/`.
- A direct agent-desktop screenshot command wrote a PNG preview under the same folder.

This verifies the JARVUS-to-agent-desktop slice on the current Windows desktop. It does not verify an AI planning loop or voice input; neither is implemented yet.
