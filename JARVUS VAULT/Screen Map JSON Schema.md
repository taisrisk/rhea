# Desktop Snapshot Data

## Current source of truth

JARVUS passes through the versioned JSON envelope emitted by agent-desktop. It does not define a competing coordinate schema yet.

Typical fields:

- `version`, `ok`, `command` — envelope version, success flag, and operation name.
- `data.app`, `data.window` — target application and window identity.
- `data.snapshot_id` — namespace for refs in this UI state.
- `data.complete`, `data.ref_count` — traversal completeness and number of actionable refs.
- `data.tree` — recursively nested UIA nodes.
- Node `role`, `name`, `value`, `states` — semantic metadata exposed by the app.
- Node `bounds` — measured screen position and dimensions when `--include-bounds` is requested.
- Node `available_actions`, `ref_id` — supported operations and a snapshot-qualified target reference.
- `data.debug.path` — optional local HTML visualization path.

Illustrative node:

```json
{
  "role": "button",
  "name": "Open Settings",
  "bounds": { "x": 120, "y": 240, "width": 140, "height": 36 },
  "available_actions": ["Click"],
  "ref_id": "@snapshot-id:e12"
}
```

This is a format sketch. Real output fields vary by command, platform, target app, and agent-desktop version; preserve its envelope rather than assuming every field exists.

## Ref lifecycle

Refs are valid only for their snapshot. Re-snapshot after a UI change, and do not retry an action unless agent-desktop reports delivery as `not_delivered` and retry as `safe`. On Windows, identical controls may be withheld as ambiguous rather than receiving unsafe refs.

## Future normalization

If the AI planner needs a smaller stable view, add a versioned JARVUS projection that preserves the source ref, window, role, name/value, bounds, supported actions, and source snapshot ID. Do not discard the source envelope or invent semantic labels.
