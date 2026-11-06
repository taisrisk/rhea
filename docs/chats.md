# Chats and archives

- **New chat** (the `+` buttons) does not create a history card. It just shows the empty chat, and a card (and a folder on disk) appears when you send the first message. Pressing it on an already empty chat does nothing, and leaving an empty chat discards it, so empty cards can never pile up.
- **The `…` menu** on each history card offers **Rename**, **Archive chat** and **Delete chat** (a second click confirms the delete; the button eases to red).
- **Archive** does not delete anything: the chat's folder moves from `~/.rhea/chats/<id>` to `~/.rhea/chats/Archives/<id>`.
- **Settings > Archived chats** lists every archived chat. **Preview** opens a small window with its messages, **Restore** moves it back into the history, and **Delete** removes it for good (two clicks).

## Files

Each chat folder holds `metadata.json`, `history.json`, `toolcalls.json`, and, once the model has a transcript, `model.json` and `agent-<id>.json`. The two transcript kinds are cached while a chat is in use and dropped five seconds after you leave it, and they are saved atomically. Restoring or archiving a chat never replaces a chat that already has the same ID: if the destination exists, nothing is moved and you are told.
