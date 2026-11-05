# Project Structure

## Root

```text
JARVUS/
├── Cargo.toml
├── Cargo.lock
├── README.md
├── captures/                    # ignored local screenshots and debug maps
├── src/
│   ├── app.rs                   # native eframe shell and controls
│   ├── desktop.rs               # agent-desktop CLI adapter
│   └── main.rs                  # startup
└── JARVUS VAULT/                # Obsidian project notes
```

## Runtime locations

- Default CLI install: `%APPDATA%\npm\node_modules\agent-desktop\bin\agent-desktop-win32-x64.exe` (the adapter also checks the ARM64 package).
- Override: `JARVUS_AGENT_DESKTOP_BIN` points directly to the native executable.
- Local generated debug files: `captures\agent-desktop\`.
- The `.gitignore` excludes captures and Cargo build output.

## Current boundary

The desktop adapter is intentionally narrow. It does not run shell commands from model output, build arbitrary command strings, or accept unreviewed AI action plans. Add new operations behind typed Rust functions that pass arguments separately and preserve agent-desktop's delivery/error envelope.
