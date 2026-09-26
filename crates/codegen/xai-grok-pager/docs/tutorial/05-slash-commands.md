# Slash Commands

[简体中文](zh-CN/05-slash-commands.md)

Type `/` on an empty prompt and a searchable dropdown of commands appears.
A few worth knowing on day one:

| Command | What it does |
|---------|--------------|
| `/help` | Browse every command and keyboard shortcut |
| `/model` | Switch models or reasoning effort |
| `/resume` | Pick up a previous session where you left off |
| `/new` | Start a fresh session |
| `/compact` | Compress a long conversation to free up context |
| `/btw` | Send Grok an aside *without* interrupting its current task |
| `/rewind` | Rewind the conversation and files to an earlier turn |
| `/undo` | Rewind the conversation to an earlier turn, leaving files alone |
| `/docs` | Full How-to Guides, in the TUI or on the web |
| `/feedback` | Send feedback to the team |

Two of those deserve a second look:

- **`/compact`** takes an optional hint: `/compact keep the auth details`.
  Check context usage anytime with `/context` — Grok also auto-compacts
  when the window fills up.
- **`/rewind`** rewinds an earlier turn, dropping later turns; the
  confirmation picks whether the files that turn touched go back too.
  **`/undo`** always drops just the conversation.

## The command palette

Press **`Ctrl+P`** (or `?` from the scrollback) to open the command palette —
one searchable list of every command, shortcut, and skill. There's also a
full shortcuts cheatsheet on `Ctrl+.` (use `Ctrl+X` if your terminal
swallows it).

You don't need to memorize anything: `/` and `Ctrl+P` will always show you
what's available.

*Go deeper: `/docs Slash Commands`*
