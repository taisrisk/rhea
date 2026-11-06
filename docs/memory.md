# Memory

Rhea remembers across every chat, without anyone saying "remember this". One SQLite file, `~/.rhea/memory/memory.db`, is shared by all chats and is yours to read, correct and delete in **Settings > Memory**.

## What it remembers

| Kind | From | Example |
| --- | --- | --- |
| `preference`, `instruction` | what you write | "I prefer dark themes", "Always reply in short sentences" |
| `fact` | what you write | "The user's name is Dana" (a newer answer replaces the older one) |
| `decision` | what you write | "We decided to use SQLite for storage" |
| `outcome` | tool results, verified | "Ran `cargo test` successfully", "Wrote C:\app\notes.md" |
| `failure` | tool results, verified | "`cargo build --release` failed (exit 1): linker not found" |
| `workflow` | a failure followed by a step that worked | "Problem: … Fixed by: …" |
| `summary` | the end of each turn | "Asked: … Result: … Tools: run_command x2" |
| `task` | what you or the answer left open | "Unfinished: the Mac signing step is not yet configured" |

What happened is recorded from **tool results** (a command's exit code, a file actually written, a desktop plan's status), never from what the model said it did. Routine reads (`ls`, `git status`, reading a file) are not remembered. Obvious secrets (tokens, passwords, keys) are hidden before anything is stored. A quoted value with spaces (`--password "two words"`, `PASSWORD='a b'`, a JSON body) is hidden whole, not just its first word.

Memories are scoped: **global** (you) or **project** (the nearest folder with a `.git`, `Cargo.toml`, `package.json` … that a command or file touched). The current project's memories rank higher. Unfinished tasks and dead ends are never carried across projects: a briefing includes the global ones and those of the project being worked on, and when no project is detected only the global ones.

## How it comes back

- **Before every message** a short *Memory* note is added to the model's context: standing preferences and instructions, facts about you, the memories that match the message, unfinished work from other chats, and dead ends that match what is being attempted. It is labeled as possibly out of date, and the model is told that what you say now wins.
- **The model can also** call `memory_search`, `memory_remember` and `memory_forget`.
- **Writes never block work.** Recording happens on a background thread; nothing runs for each mouse move or keystroke.

## How it finds things

Hybrid retrieval, adapted from Palace RS: SQLite full-text search (BM25, with word stemming) combined with vector similarity, then adjusted for recency (half-life about 35 days), importance, whether the outcome was verified, an experience weight (failures that repeat count more, solved problems count more), what kind of memory the question's wording points at ("why did we choose…" finds decisions, "what failed last time" finds failures), time phrases ("yesterday", "last week", "3 days ago") and the current project. Near-identical memories are collapsed, and nothing is returned unless it is actually about the question.

**Embeddings:** a small built-in matcher works from the first message with nothing to download. **Settings > Memory > Download model** adds sentence-transformers `all-MiniLM-L6-v2` (about 90 MB), a real sentence encoder run on the CPU in pure Rust with Candle (no ONNX runtime, no Python). It understands meaning ("cheap flight" = "inexpensive plane ticket") and stored vectors are re-made automatically when it arrives.

**Related %.** Every recalled memory carries a *related* figure: the model's similarity rescaled between "unrelated" and "clearly the same thing", lifted a little by shared keywords, and an *importance* figure of its own (how much it matters apart from the question). Memories that are **80% related or more are brought along automatically** (up to 10), weaker matches from 45% only fill the remaining room, and for each strong match Rhea also adds what else happened in the same chat around that time (the failure before the fix, the decision behind the outcome), so the model gets the full picture rather than one line. The percentages appear in the Memory note and in `memory_search` results.

## Facts that change

Facts with a single answer (name, where you live, where you work) go through a small graph with validity dates: a new answer ends the old one instead of sitting beside it as a contradiction, and the history is kept. The same idea retires a superseded memory (it is marked *superseded*, not deleted).

## Your control

Memories are saved once per event: indexing chats you already have, or indexing twice, does not create copies of what was remembered live. Settings > Memory switches memory off, lists everything with filters and search, edits a memory (an edited memory counts as verified by you, and the facts it states are re-read: correcting "The user's name is Dana" to "…Alex" ends the old name in the fact graph too, so the briefing cannot contradict the edit), deletes one, erases all, exports JSON to Downloads, and **Index my chats** builds memory from the chats you already have. Nothing is sent anywhere; the model sees only the note placed in your conversation.

## Where it came from

The retrieval design and the temporal fact graph follow [Palace RS](https://github.com/AncientiCe/palace-rs); dead-end tracking, experience weighting and contradiction handling follow [OmniMem](https://github.com/richarvey/OmniMem); the optional embedding model is [all-MiniLM-L6-v2](https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2) (Apache-2.0) run with [Candle](https://github.com/huggingface/candle) (MIT/Apache-2.0). Palace RS and OmniMem are MIT. The code is written for Rhea's own types and tool loop; see `THIRD_PARTY.md`.
