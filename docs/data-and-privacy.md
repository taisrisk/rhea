# Data and privacy

Rhea keeps application data in the current user's `.rhea` directory:

```text
~/.rhea/
  settings.json            settings (model, auto-compact, autocorrect, …)
  mcp-servers.json         MCP server configuration (secrets are not in here)
  hooks.json               hooks
  spelling.json            words you added, autocorrect decisions
  usage.json               lifetime usage totals (daily tokens, time worked, counters; no chat content)
  chats/
    <chat-id>/             metadata.json, history.json, toolcalls.json, model.json, agent-<id>.json, ledger.jsonl
    Archives/<chat-id>/    archived chats
  skills/                  skills installed by Rhea
  chrome-extension/        the extension Chrome loads
  memory/                  memory.db (long-term memory) and the optional embedding model
  laya/                    the local model (private Python, packages, weights)
  captures/                screenshots and debug maps
```

**Coming from an older version?** Earlier versions called this folder `~/.jarvus`. The first time a newer Rhea starts it moves everything in it to `~/.rhea` (nothing already in `~/.rhea` is overwritten, and it is safe to repeat). The Chrome extension folder is copied rather than moved, because Chrome still loads the old one; the Browser page tells you to load the new folder when convenient.

Credentials move too: a ChatGPT login or MCP secret saved under the old `JARVUS` credential-store name is copied to the new `Rhea` name the first time it is needed and the old entry is removed, so you stay signed in. The Chrome native host is registered under `com.rhea.browser`, and also under the old `com.jarvus.browser` so an extension Chrome loaded earlier keeps working until you reload it from the new folder.

On Windows `~` is the user's profile directory (for example `C:\Users\Alice\.rhea`). The app migrates legacy conversation and settings files when it finds them. Credentials (the ChatGPT session and MCP secrets) are in the operating system's credential store: the Windows Credential Manager, or the macOS Keychain. Do not attach `~/.rhea` to bug reports; it can contain private chat content and connection configuration.

Rhea sends prompts and the tool results needed for an answer to the connected ChatGPT service. Local screenshots, full accessibility trees, the local model's inputs and unrelated files stay on your computer unless you share their contents. The only other network use is downloading things you asked for: skills, the Chrome extension, the local model, updates.

Tool calls are visible in the conversation. A successful sequence of local steps is not, by itself, proof that a longer goal is complete, and consequential actions such as sending messages should only happen when your request clearly authorizes them.
