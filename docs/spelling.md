# Spell check and autocorrect

The chat box underlines misspelled words in **red** as you type. Right-click a word for suggestions, **Add to dictionary** and **Ignore for this message**. The dictionary is Hunspell's en_US (SCOWL, license in `assets/dictionaries`) read by a pure-Rust library, so it is the same on Windows and macOS and nothing is sent anywhere. Slash commands, code in backticks, links, paths, numbers, acronyms and camelCase are never checked.

## Automatically AutoCorrect (Settings > Chat & model, off by default)

With it on, when you finish a misspelled word (by typing a space or punctuation) it is replaced with the first suggestion, keeping your capitalization.

- If you delete the correction and write the same word again, Rhea takes that to mean you intend it: it is not corrected again in this message and is underlined in **blue** instead of red.
- Each time that happens counts. After you undo the autocorrection of the same word **three times** it is never autocorrected again (it stays blue). The counts are kept in `~/.rhea/spelling.json`, with the words you added.
- Right-clicking a corrected word offers **Change back to "…"**, which counts the same way.
