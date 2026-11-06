# Runs, evidence and testing

## The run ledger

Every message you send is one *run*, and Rhea keeps a record of it in the chat's own folder, `ledger.jsonl`, next to the transcript. Archiving or deleting the chat takes the ledger with it. Nothing in it leaves your computer.

One line is written per event: the request (first 300 characters), each model round (time and tokens), each tool call (name, succeeded or failed, time), each piece of research evidence, each decision of the research gate, and how the run ended (`completed`, `failed`, `cancelled`). Subagents write to the same file under their own id. A run that has a start but no end was interrupted, for example by closing the app, and is shown that way.

**Settings > Usage > Recent runs in this chat** lists the latest runs of the open chat, newest first: how it ended, model rounds, tools used and how many failed, evidence found, whether the research gate sent it back, and the time. The file is cut back to its newest half when it passes 512 KB.

## Research evidence

A request that needs current data or sources ("search the web for games with more than 1K active players") is not finished until the run has *evidence*: a successful search, fetch or extract result that holds data meeting the request. A plan status, a page navigation or the model's own promise never counts. Each piece of evidence records the tool, the site it came from and how many results qualified, and the same page fetched twice counts once. For a metric request ("more than 1K players") each qualifying record counts once by its name and value, so the same games listed again in another layout or by another tool are not new evidence.

When the model tries to finish with too little, it is sent back once and told how much is verified so far ("1 of the 2 needed (rolimons.com)"). If it still falls short, the reply is an honest blocker that says what was verified and what was not, instead of unsupported claims.

## Desktop retries

When a desktop action is refused because the control's reference went stale, and nothing was delivered, Rhea looks at the window again. If the same kind of control with the same label is there exactly once, the click, hover or scroll is repeated once on it, and the result says it was retried and from which reference. Typing, sending and key presses are never repeated automatically, because they could duplicate text or messages; those go back to the model with the fresh screen. A control with no label, or two controls with the same label, is also left to the model. A window with more controls than the native read keeps (25,000) is marked as cut off, never treated as a complete snapshot, and a control that is not found in it is reported as "may be in the part that was cut off" instead of "does not exist".

## Replay tests

The orchestration is tested by replaying recorded model streams (`src/replay.rs`). A scripted server on loopback plays back model replies one request at a time, the tools are stand-ins, and each test checks what the loop did: what it sent back to the model, what the user was shown, and what the ledger recorded. The scenarios cover a plain answer, a tool result returning to the model, a failed tool, a hidden tool being refused, research sent back for more evidence and then accepted, research ending in an honest blocker, a repeated page not counting twice, and a model error. They need no network or account: `cargo test --bin rhea replay`.
