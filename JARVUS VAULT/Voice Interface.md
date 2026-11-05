---
title: Voice Interface
type: technical-design
status: proposed
tags: [jarvus, voice]
---

# Voice Interface

## Goal

Give JARVUS a conversational voice mode without coupling basic PC control to cloud audio tools.

## Initial interaction

- Push-to-talk is the first mode; a wake word is a later, opt-in decision.
- A visible indicator shows microphone capture and recognition state.
- Local speech recognition turns speech into text for the same planner used by typed input.
- Local speech output reads short assistant responses when enabled.
- Text remains available whenever microphone access is disabled or recognition fails.
- Consequential actions show the recognized request and confirmation before execution.

## Integration boundary

Audio capture, recognition, and playback are local in the first design. The documented ChatGPT plan-usage route currently excludes audio input/transcription tools, so voice does not rely on those hosted tools. The model receives the resulting task text and a minimized screen map.

## Open decisions

- Windows speech API versus bundled offline speech engine.
- Supported languages and voice choice.
- Whether recognized text is retained in task history.
- Push-to-talk hotkey and headset behavior.
- Accessibility controls and spoken confirmation wording.

## Related notes

- [[ChatGPT OAuth and Runtime]]
- [[Privacy and Security]]
- [[Product Requirements]]
