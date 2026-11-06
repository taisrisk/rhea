# Shell and sessions

The Shell package's `run_command` runs anything a terminal can (PowerShell, cmd, bash, zsh; up to 15 minutes; `detach` for servers and installers). `read_file` can read line ranges and `edit_file` applies several line-range edits atomically. A desktop plan (`jev_run_plan`; the tool keeps this name for compatibility) can mix `shell` steps with GUI steps, so "install it, open it, click through it" is a single plan.

Every chat has a stable session id that keys the ChatGPT prompt cache (see [context](context.md)). The first message also starts a background naming agent that renames the chat with the selected model at its lowest reasoning effort.

On a Mac the shell is `/bin/zsh -lc` and Rhea starts with the PATH a Terminal would have.
