# MCP servers

Open **Settings > MCP servers** to import or manage servers. Rhea supports local stdio, Streamable HTTP and legacy HTTP+SSE transports. Use HTTPS for remote servers and keep bearer tokens in the app's credential storage (they are read once per server per run, so the OS does not keep asking for permission). Never paste secrets into screenshots or public issue reports.

The built-in Chrome extension is a **browser bridge**, not an MCP server ([details](chrome-extension.md)). The built-in **Shell** connection supplies bounded local command, file, directory and HTTP(S) fetch tools. When asked what is connected, Rhea queries live inventory and reports configured MCP servers, browser bridges and local connections as separate groups.
