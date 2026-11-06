# Privacy and Security

## Current data flow

- `agent-desktop status` reads local platform/accessibility capability.
- A snapshot reads the selected app's Windows UIA tree and returns structured JSON to Rhea locally.
- A ref click acts on the selected desktop app after the user presses **CLICK REF**.
- Optional HTML debug visualizations and requested screenshots are stored under the project `captures/` folder, which Git ignores.
- Rhea has no AI provider connection, microphone capture, cloud upload, or persistent conversation store yet.

## Trust boundaries

- UI text and accessibility-tree content come from other apps and may contain untrusted instructions. Treat them as data, never as permission to expand the user's task.
- Snapshot refs expire when the UI changes. Refresh and verify state after actions.
- agent-desktop actions return delivery and retry information. Never retry a click that may have been delivered; read the UI again first.
- Windows integrity boundaries apply: a non-elevated bridge cannot control an elevated app. Do not silently elevate Rhea.
- Debug screenshots/maps can contain private content. Keep them local; only request or transmit them to an AI provider when the user chooses a vision path.

## Planned safeguards before AI execution

1. Display the model's proposed steps before multi-step or consequential actions.
2. Restrict the planner to typed, allowlisted desktop operations.
3. Require fresh snapshots, snapshot-bound refs, and post-action verification.
4. Stop on ambiguity, stale refs, unverified delivery, privilege boundary, or incomplete tree.
5. Keep local action history and allow immediate cancellation.

See [[Local Action Broker]] and [[ChatGPT OAuth and Runtime]].
