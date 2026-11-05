# JARVUS Browser Bridge (Chrome MV3)

Lets JEV read and operate your Chrome tabs through the DOM: fast, and without
screen reading. The extension connects to JARVUS by itself whenever Chrome is
running and reconnects automatically, so there is nothing to click.

## Install (one time)

1. Start JARVUS once. On every start it registers its native messaging host for
   your Windows user (`%LOCALAPPDATA%\JARVUS\com.jarvus.browser.json` plus the
   `HKCU\Software\Google\Chrome\NativeMessagingHosts\com.jarvus.browser` key),
   pointing at the running `jarvus.exe`.
2. In Chrome open `chrome://extensions`, turn on **Developer mode**, choose
   **Load unpacked**, and select this `chrome-extension` folder.
   If an older "JARVUS Browser Bridge" is listed, remove it first.

The manifest's `key` pins the extension ID to `eplppddfejfcbdllfbooenaplkobcpkk`,
which the host manifest already allows; no environment variable is needed.
Settings → Connections in JARVUS shows a green dot once the bridge answers.

## What JEV can do

Tab level (any window): list tabs, switch, open, close, navigate, back,
forward, reload. Page level (any http/https tab): snapshot, click (real
mouse-event sequence, double/right), hover, type (works with React and rich
editors, optional submit), select (native and custom dropdowns), press keys,
scroll, wait for text/selectors, and extract text, links, tables, headings,
images, form fields or arbitrary elements.

Elements are targeted by visible label the way a person says it ("search",
"sign in", "chat"): case, symbols, emoji and extra words are ignored and the
best visible match wins. A `controlId` from a snapshot or a CSS selector also
works.

## Privacy

- Password field values, cookies, storage and browsing history are never read.
- Only `http`/`https` pages are opened or read; `chrome://`, `file:`,
  `javascript:` and `data:` URLs are refused.
- Commands only come from the local JARVUS process over a loopback relay that
  requires a per-session 256-bit token.

## Remove

Remove the extension in `chrome://extensions`, then delete the registry key:

```bat
reg delete "HKCU\Software\Google\Chrome\NativeMessagingHosts\com.jarvus.browser" /f
```
