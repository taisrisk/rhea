# Screenshots

The main window: a new chat with the Context panel open.

![Main window](screenshots/main.png)

## Settings, one page each

| | |
| --- | --- |
| **Chat & model** ![Chat and model](screenshots/settings-chat.png) | **Usage** ![Usage](screenshots/settings-usage.png) |
| **Connections** ![Connections](screenshots/settings-connections.png) | **MCP servers** ![MCP servers](screenshots/settings-mcp.png) |
| **Browser** ![Browser](screenshots/settings-browser.png) | **Agents, skills & hooks** ![Agents](screenshots/settings-agents.png) |
| **Memory** ![Memory](screenshots/settings-memory.png) | |
| **Local model** ![Local model](screenshots/settings-local.png) | **Archived chats** ![Archived chats](screenshots/settings-archived.png) |
| **Storage** ![Storage](screenshots/settings-storage.png) | |

## Compose mode and subagents

![Compose mode plan panel](screenshots/compose.png)

![Subagent panel](screenshots/subagents.png)

## How these were made

They are captured from a clean profile (no chats, no personal paths) with the demo switches `RHEA_DEMO=1` (hides the signed-in address), `RHEA_DEMO_SETTINGS=<chat|usage|connections|mcp|browser|agents|local|archived|storage>` (opens that settings page), `RHEA_DEMO_CONTEXT=1` (opens the Context panel) and `RHEA_DEMO_DRAFT=<text>`, with `USERPROFILE`, `APPDATA` and `LOCALAPPDATA` pointed at an empty folder.
