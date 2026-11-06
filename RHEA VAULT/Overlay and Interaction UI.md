---
title: Overlay and Interaction UI
type: ui-design
status: proposed
tags: [rhea, overlay, ui]
---

# Overlay and Interaction UI

## Ghost-outline overlay

The overlay visualizes boxes emitted by [[Custom Screen Pipeline]]. It is a local view of the detector, not an AI-generated layer.

### Elements shown

- Outer frame, app window, or panel bounds when detected.
- Nested cards and controls with distinct outlines.
- Small element ID and predicted type in inspect mode.
- Confidence in inspect/debug mode, not necessarily in the clean default overlay.
- A separate highlight for the action target currently selected by the broker.

### Requirements

- Transparent and click-through except while the user is inspecting/selecting.
- Keep exactly aligned with the matching capture after DPI, resolution, and monitor changes.
- Avoid covering controls with large labels; place labels outside the target box where possible.
- Provide toggles for overlay, IDs, confidence, and debug candidates.
- Clicking an outline in inspect mode selects a target for explanation, not immediate execution.

## Main application surface

The first UI should make these states clear: ready, listening, locally detecting, model planning, awaiting confirmation, acting, verifying, paused, and error. The user can stop the current task and see the proposed next step.

## Visual reference

The attached reference shows a line-art wireframe: large desktop bounds, a nested application window, column/panel divisions, several card-like rectangles, and small controls. Rhea should produce this kind of nested spatial map while also retaining IDs and detector confidence in its structured output.

## Related notes

- [[Custom Screen Pipeline]]
- [[Screen Map JSON Schema]]
- [[Product Requirements]]
