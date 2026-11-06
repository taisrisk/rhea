# Local model (Laya)

Laya ([convaiinnovations/laya](https://huggingface.co/convaiinnovations/laya)) is a small, non-autoregressive "System 1" decision model. You give it a situation and typed questions and it answers in a fraction of a second with calibrated probabilities:

- `choice`: pick one option (`criteria` = `{option: description}`),
- `score`: rate on levels (`criteria` = a list),
- `noul`: a yes/no question.

It is not a chat model. In Rhea it does two jobs: ChatGPT can call it as the `laya_decide` tool for quick bounded judgments, and desktop steps use it to choose between look-alike controls when it is already running (only when it is at least 85% sure; otherwise the step stays ambiguous as before).

## Installing (Settings > Local model)

One click on **Install** downloads everything into `~/.rhea/laya` (about 1.5 GB). Nothing else has to be installed, and Unsloth is not needed:

1. a private Python 3.12 (python-build-standalone from GitHub) is unpacked into `laya/python`; your own Python is never touched,
2. `pip install laya[serve]` adds the model code and PyTorch to that private Python,
3. the model weights (about 800 MB) are fetched from Hugging Face into `laya/hf`.

It works the same on Windows and macOS (the right Python build is chosen for the chip). Delete `~/.rhea/laya` to remove it completely.

## Running

The server (`python -m laya.serve`, the `/v1/systemone` protocol) starts the first time something needs it, listens only on `127.0.0.1` on a free port with a random bearer token, never reaches the network at start-up, and is stopped when Rhea closes. **Start**, **Stop** and **Test** are on the same page; Test asks a real question and reports the time (about 0.3 s on a CPU for a short state).

## The tool

`laya_decide {state, questions}` returns `answers.<name>.choice` with `probabilities`, plus `latency_ms`. If the model is not installed the tool says so and points to Settings.
