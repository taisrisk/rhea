# Chrome extension

The extension lets Rhea read and act on Chrome tabs. It is a browser bridge, not an MCP server. Rhea acts only on tabs the extension can see and never reads saved passwords or cookies.

## One-click install (Settings > Browser)

If the extension is missing, the Connections page says *Chrome extension not installed · install it?* and the Browser page has **Install extension**. One click:

1. downloads the `chrome-extension` folder from the GitHub repository (and falls back to the copy built into the app if GitHub is unreachable) into `~/.rhea/chrome-extension`, on Windows and macOS alike,
2. registers the native messaging host that lets the extension talk to Rhea,
3. copies the folder path to the clipboard and opens `chrome://extensions`.

Chrome does not allow an unpacked extension to be added silently (it removed the command-line switch for branded Chrome, and an extension written into a profile's files is rejected by its tamper protection), so one step stays with you. Rhea opens `chrome://extensions` straight in the right profile, the one that already has the extension or the one you used last, so Chrome's "Who's using Chrome?" picker does not get in the way, and the **GitHub** button opens the extension's source page. The step that stays with you: turn on **Developer mode**, choose **Load unpacked** and paste the folder path. Rhea then checks every few seconds and shows three live steps: *Extension files*, *Added to Chrome* (read from Chrome's profile data) and *Connected to Rhea*.

Status messages worth knowing:

| Message | Meaning |
| --- | --- |
| Not installed | no files and Chrome does not know the extension |
| Downloaded, not added to Chrome yet | do the Load unpacked step |
| Chrome loads a different copy | Chrome points at another folder (an old build folder); remove that one and load `~/.rhea/chrome-extension` |
| Turned off in Chrome | switch it on in `chrome://extensions` |
| Added to Chrome | installed, waiting for the connection: open Chrome or reload the extension |
| Working | connected |

## Why the folder never moves

Chrome remembers an unpacked extension by its folder and drops it when the folder disappears. Rhea therefore always uses `~/.rhea/chrome-extension`, never its own install folder or a build directory, and updates the files in place (never delete and recreate) when the app ships a newer extension version.

The extension's ID is fixed by the `key` in its manifest, so reinstalling gives the same ID and the native host registration keeps working.
