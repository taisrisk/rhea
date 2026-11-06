# Third-party designs and data

Rhea's memory system (`src/memory/`) is original code written for Rhea, but it follows ideas and algorithms from these MIT-licensed projects, and the credit belongs to them:

- **Palace RS** by AncientiCe, https://github.com/AncientiCe/palace-rs (MIT): the hybrid BM25 plus vector ranker and its constants (recency half-life, rescaled cosine), time-phrase boosts, question-intent boosts, the temporal entity/triple fact graph, and the conversation/memory chunk model.
- **OmniMem** by richarvey, https://github.com/richarvey/OmniMem (MIT): experience weighting, the dead-end list ("graveyard"), contradiction handling and memory lifecycle ideas.
- **all-MiniLM-L6-v2** by sentence-transformers (Apache-2.0) and **Candle** by Hugging Face (MIT/Apache-2.0): the optional 90 MB sentence-embedding model, downloaded on request and run in pure Rust.
- **SQLite** (public domain) through `rusqlite` (MIT).

Other data and libraries:

- **en_US Hunspell dictionary** (SCOWL), `assets/dictionaries/LICENSE-en_US.txt`, read by the `spellbook` crate (MIT): spell check and autocorrect.
- **egui** (MIT/Apache-2.0), vendored in `vendor/egui` with one change: labels that are scrolled out of view still take part in an active text selection, so a selection no longer disappears when scrolling and everything selected is copied.
- **Phosphor icons** (MIT) through `egui-pop-phosphor`.
- **python-build-standalone** (MPL-2.0 and others) and the **Laya** model (`convaiinnovations/laya`, Apache-2.0): downloaded on request for the local model; not bundled in the repository.
- Skills adapted from **obra/superpowers** (MIT), **taste-skill** (MIT) and **unslop** (CC BY 4.0).
