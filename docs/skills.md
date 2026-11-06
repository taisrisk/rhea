# Skills

A skill is a folder with a `SKILL.md` (a name, a description, then instructions and optional helper files). Rhea sees the list of installed skills on every run, loads one with `use_skill` when a request matches, and can install more from GitHub with `install_skill`.

## Where skills live

- **`~/.rhea/skills`**: Rhea's own folder. Every new install goes here.
- **`~/.agents/skills`** and **`~/.claude/skills`**: shared with your other agent tools. Rhea reads them and never writes there.

If the same skill exists in Rhea's folder and in a shared folder (same name and same files) it is listed once and Rhea's copy wins. A *different* skill that happens to share a name stays visible, under `<name>-<folder>`.

## Built in and bundled

- Always active: **systematic debugging** and **brainstorming**, adapted from [obra/superpowers](https://github.com/obra/superpowers) (MIT).
- Preinstalled on first launch from GitHub: **design-taste-frontend** ([taste-skill](https://github.com/Leonxlnx/taste-skill), MIT) and **unslop** ([heydamir/unslop](https://github.com/heydamir/unslop), CC BY 4.0, by Damir Sokolovsky). Remove one in Settings and it stays removed.

## Installing

In Settings > Agents, skills & hooks paste a GitHub link, or ask Rhea: "install https://github.com/owner/repo/tree/main/skills/name". A link to a skill folder, to its `SKILL.md`, or to a repository with exactly one skill works; a repository with several lists them so you can pick.

## Skills that bring hooks

If the installed skill contains `hooks/hooks.json` (or `hooks.json`) in the Claude Code format, its hooks are registered automatically in `~/.rhea/hooks.json`, tagged with the skill's name:

- `${CLAUDE_PLUGIN_ROOT}`, `${CLAUDE_SKILL_DIR}`, `${SKILL_DIR}` and `${RHEA_SKILL_DIR}` in commands become the skill's folder,
- events Rhea has no equivalent for are skipped and reported,
- reinstalling replaces the skill's hooks instead of duplicating them, your own hooks are never touched, and removing the skill removes its hooks,
- the install result lists every hook it registered, and they appear in the hooks editor under Settings.

Hooks run commands on your computer, so read a skill's `hooks` file before installing skills you do not trust.
