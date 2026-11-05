---
title: JARVUS VAULT
type: map-of-content
status: active
tags: [jarvus, index]
---

# JARVUS VAULT

Project brain for **JARVUS**, a standalone Windows desktop assistant. JARVUS provides the chat UI and local executor; the signed-in ChatGPT model plans tool use, and JEV performs local desktop operations and returns compact results.

## Start here

- [[Welcome]] — vault entry point.
- [[Project Overview]] — purpose, boundaries, and present product state.
- [[Product Requirements]] — capabilities and acceptance criteria.
- [[Architecture Overview]] — components and data flow.
- [[Implementation - ChatGPT and JEV]] — current implementation and validation record.
- [[ChatGPT OAuth and Runtime]] — device authorization and Responses integration.
- [[Custom Screen Pipeline]] — screenshot-local mapping direction and current UIA-first implementation.
- [[Screen Map JSON Schema]] — structured screen-map contract.
- [[Local Action Broker]] — local action execution and confirmation boundaries.
- [[Privacy and Security]] — local-first handling and action safety.
- [[Voice Interface]] — planned voice interaction.
- [[Overlay and Interaction UI]] — planned visual overlays and interaction states.
- [[Roadmap]] — delivered work and remaining verification.
- [[Decision Log]] — durable product and architecture decisions.
- [[Project Structure]] — source and vault organization.
- [[Current Status and Known Issues]] — latest changes, open defects, and proof status.
- [[Runtime Integrations and Setup]] — MCP stdio configuration, Chrome native-messaging setup, title generation, and current verification limits.

## Current implementation status

| Area | State |
|---|---|
| Native Windows application | Implemented in Rust with eframe; current release build passed, while pixel-level visual and live drag/navigation checks remain open |
| ChatGPT sign-in | Codex device authorization flow is implemented; live account authorization requires the user |
| Chat and model catalog | ChatGPT Codex-backend Responses streaming and account model catalog are implemented; a live signed-in response remains unverified |
| JEV tool loop | Local process/app/UIA tools execute through JEV; tool-call traces are persisted and shown in chat |
| Agent Desktop | UIA snapshots, fresh-ref interactions, local map artifacts, and compact interactive-only snapshot option are implemented |
| Chat history | Locally persisted conversations and message history are implemented |
| Context display | Estimated and service-reported usage/cache fields are implemented; current clipping/placement repair is being verified |
| Async chat titles | One-shot asynchronous small-model title request with local fallback and persisted request state is implemented; live title generation remains unverified |
| MCP | Local stdio configuration, connect/disconnect/reload, tool discovery, and dispatch are implemented; no external MCP server has been installed or connected |
| Chrome extension | MV3 extension and native-host entry-point wiring are implemented; Chrome installation, site grants, and end-to-end browser actions remain unverified |
| Voice interface | Not implemented |
| Custom screenshot-to-layout engine | Local-only direction documented; UIA remains the standard control path |

## Current verification status

Current local checks: `cargo test --bin jarvus` passed 30/30; `cargo build --release --bin jarvus` passed; the built executable was confirmed as a GUI-subsystem PE. These checks do not establish live ChatGPT auth/inference, Chrome installation, MCP server connectivity, user-desktop actions, or pixel-level UI correctness. The native capture provider timed out during the current visual review. See [[Current Status and Known Issues]] and [[Runtime Integrations and Setup]].

## Working rule

Keep requirements, implemented behavior, and verified behavior distinct. Update the relevant notes and this index when source or runtime evidence changes. Never record a build as proof of a live ChatGPT request or a desktop action.
