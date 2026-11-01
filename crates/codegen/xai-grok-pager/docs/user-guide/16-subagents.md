# Subagents and Personas

[简体中文](zh-CN/16-subagents.md)

Subagents are independent child sessions that handle tasks in parallel. Each subagent has its own context window, so the main agent can delegate work (research, implementation, testing, and code review) without consuming its own context. A subagent reports a summary back to the parent when it finishes.

Subagents are enabled by default.

---

## Agents vs Personas

Agents and personas both customize behavior, but they operate at different levels:

| | **Agents** | **Personas** |
|---|---|---|
| **What they configure** | The whole session: model, tools, prompt mode, system prompt | A behavioral overlay added to a subagent's prompt |
| **Scope** | Primary session or subagent | Subagents only |
| **How you set them** | At startup, or with agent definitions (`.md` files in `.grok/agents/` or `~/.grok/agents/`) | In `config.toml` (`[subagents.personas]`) or `.toml` files under `.grok/personas/`; applied during subagent resolution |
| **What they control** | Model, tool availability, prompt body, skills | Tone, output format, task focus, and input/output contracts |
| **Who edits them** | You -- create, delete, or toggle them in the agents modal or by editing files | You -- define custom personas in config or files; bundled personas are read-only |
| **Examples** | `grok-build`, `explore`, `plan` | `researcher`, `concise` |

An agent defines the session itself. A persona shapes how a subagent behaves within a session. A subagent always runs as an agent type (for example, `general-purpose`), and resolution can layer a persona on top.

Manage both in the agents modal. Open it with `/config-agents` (alias `/agents`), or open the Personas tab directly with `/personas`. The modal has two tabs: **Agents** and **Personas**.

---

## Disabling Subagents

Disable subagents with a CLI flag, an environment variable, or the config file (highest priority first). The same rules apply to the interactive `grok` TUI, `grok agent stdio`, and headless runs.

```bash
grok --no-subagents                  # This session only
export GROK_SUBAGENTS=0              # Environment variable
```

```toml
# ~/.grok/config.toml
[subagents]
enabled = false
```

Only an explicit `enabled = false` turns subagents off. A `[subagents]` table that sets `max_depth`, `[subagents.models]`, or `[subagents.toggle]` without an `enabled` key keeps them on.

---

## How Subagents Work

When the main agent identifies work to delegate, it calls the `spawn_subagent` tool to start a child session. The child runs with:

- Its own context window, independent of the parent
- A toolset determined by its agent type and optional capability mode
- Optional persona instructions applied during resolution

The parent receives the child's output -- usually a summary -- when the child finishes.

---

## Built-in Agent Types

Built-in types can be selected by name. An omitted `subagent_type` still means `general-purpose`. A type is left out of the enum when `[subagents.toggle]` disables it, the parent's `Agent(...)` allowlist excludes it, or a custom agent shadows its name.

| Type | Pick it for | Toolset | Default effort / max_turns |
| --- | --- | --- | --- |
| `general-purpose` | Mixed research and implementation, multi-step tasks; the default when unsure. | All tools (read/write, execute, search, web, plan). | Inherit session / 40 |
| `explore` | Investigation only: find files, read code, trace call paths, change nothing. | Read-only: read, list, `search_code`, `resolve_context`, `find_symbol`, `find_references`, `goto_definition`, `get_diagnostics`; no shell. | `low` / 12 |
| `plan` | Implementation design: explore the code and produce a step-by-step plan. | Read-only tools plus `todo_write`; no shell or edits. | `medium` / 20 |
| `implementer` | Implement and self-verify: edit code, run tests, builds, or linters. | Full read/write/execute toolset (no workflow). | Inherit session / 40 |

Selection guidance: `explore` for investigation only, `plan` for design only, `implementer` for implementation plus verification, `general-purpose` for mixed work or an unclear boundary. Custom types are selected by name, for example `my-plugin:reviewer`.

The schema lists built-ins first, in the order above, then project, user, and plugin agents. If a parent's `tools` list restricts spawning with `Agent(...)`, the enum shows only the allowed types. An unknown type fails the call with the list of valid types.

### `thoroughness`

`thoroughness` applies to `explore` only: `quick` (obvious matches), `medium` (also check nearby call sites and conventions), `very_thorough` (trace every relevant path and report edge cases). Grok Build writes it as the first line of the child prompt, `Thoroughness: <level>.`. Other types log a warning and ignore it.

---

## Personas

A persona is a named behavioral overlay. Its instructions are injected into the subagent's conversation as a `<system-reminder>`, which shapes tone, output format, and task focus without changing the subagent's agent type, model, or tools.

Define personas in `config.toml` or in `.toml` files:

```toml
[subagents.personas.researcher]
instructions = "You are a thorough researcher. Always cite specific file paths."
description = "Deep investigator."
```

Grok Build discovers file-based personas from these locations, in priority order:

- `.grok/personas/*.toml` (project)
- `~/.grok/personas/*.toml` (user)
- The bundled personas directory (lowest priority)

Each file defines one persona, and the file name (without the extension) becomes the persona name. Inline `config.toml` personas take precedence over files. Only `.toml` files are discovered.

Manage personas in the Personas tab of the agents modal (`/personas`). Bundled personas are read-only; personas you define are editable.

> **Note:** Grok Build applies personas through subagent resolution and roles, not through a `spawn_subagent` parameter. The main agent does not pass a persona name when it spawns a child.

### Persona Fields

| Field               | Description                                                          |
| ------------------- | ------------------------------------------------------------------- |
| `instructions`      | Inline instruction text applied as the persona layer.               |
| `instructions_file` | Path to an instruction file, loaded at spawn time and merged after `instructions`. |
| `description`       | Short summary shown in the persona catalog. Falls back to the first paragraph of `instructions`. |
| `inputs` / `outputs`| Declared input and output contract (see below).                     |
| `model`             | Model override applied when the persona is used.                    |
| `reasoning_effort`  | Reasoning effort applied when the persona is used.                  |
| `default_isolation` | Default isolation mode (`none` or `worktree`).                      |

### Input/Output Contracts

A persona can declare the inputs it expects and the outputs it produces. The parent agent reads these to know what context to supply and what artifacts to expect. This lets you chain personas, so one persona's output file becomes the next persona's input:

```toml
[[subagents.personas.reviewer.inputs]]
name = "review_file"
io_type = "file"
required = true
description = "Path to the code under review"

[[subagents.personas.reviewer.outputs]]
name = "summary_file"
io_type = "file"
required = false
description = "Path to write review notes"
```

Each field has a `name`, an `io_type` (defaults to `file`), a `required` flag, and a `description`.

### Persona Resolution

When a persona applies, Grok Build resolves the effective model and reasoning effort in this order, highest priority first:

1. Explicit spawn-time override
2. Role default
3. Persona default
4. Parent session

Isolation follows the same order for the first three steps but defaults to `none` (no worktree) rather than inheriting from the parent session.

If a persona is requested but cannot be resolved -- it is not found, has no instructions, or its `instructions_file` is unreadable -- the spawn fails.

---

## Spawning Subagents

The main agent calls the `spawn_subagent` tool. Its parameters:

| Parameter | Description |
| --- | --- |
| `prompt` | The full task prompt for the subagent. With `output_schema`, require it to end with a single JSON value matching the schema. |
| `description` | A short description of the task. |
| `label` | A 3-5 word display label used by `list_agents` and the subagent UI; omitted means the id is used. |
| `subagent_type` | `general-purpose` (default), `explore`, `plan`, `implementer`, or a custom type name. |
| `thoroughness` | `explore` only: `quick`, `medium`, or `very_thorough`. Ignored by other types. |
| `background` | Defaults to `true`: return a subagent ID immediately, retrieve the result later with `get_command_or_subagent_output`, and get notified on completion. Set `false` to wait in the foreground; a wait over the foreground budget moves the child to the background and returns its ID. |
| `isolation` | `none` (shared workspace, the default) or `worktree` (isolated git worktree). |
| `context` | `fresh` (the default, empty transcript) or `fork` (seed the child with the parent's conversation). A forked child inherits the parent model and runs read-only unless its type is `implementer`; ignored when `resume_from` is set. |
| `output_schema` | Optional JSON Schema constraining the subagent's final answer. See below. |
| `resume_from` | Continue a finished subagent's conversation. Pass its subagent ID. |
| `cwd` | Working directory for the subagent. Mutually exclusive with `isolation: worktree`; ignored when `resume_from` is set (the resumed child inherits its source's directory). |
| `model` | Optional model slug. Inherits the parent model by default; ignored when `resume_from` is set (the source model is pinned). |

When you run a subagent in the background, retrieve its result later with `get_command_or_subagent_output`.

### `output_schema`

With a JSON Schema object, the child's last turn must end with a single JSON value, and Grok Build validates it against the schema:

- Supported keywords: `type`, `properties`, `required`, `items`, `enum`, `additionalProperties`, `minItems`, `maxItems`, `minLength`, `maxLength`, `minimum`, `maximum`, `description`, `$defs`, `definitions`, `$ref`.
- A schema with any other keyword or an invalid shape (for example a bare `true`/`false`) fails before spawn, so no child tokens are spent.
- If validation fails, or the child produces no JSON, the subagent fails and reports the reason.
- A validated result is returned in the result's `structured` field so the parent and workflows can destructure it. Without `output_schema`, the subagent returns free text exactly as before.

### Sending messages to subagents

The `send_subagent_message` tool is on by default. Disable it with `GROK_ACTIVE_AGENT_MESSAGES=0` or `[features] active_agent_messages = false` (a kill switch).

The root session can send a follow-up to a subagent it owns. A granted child also receives the tool, and its prompt gets an `<agent-roster>` block listing the main agent and any named siblings:

- `subagent_id: "parent"` targets that child's active parent subagent.
- A durable agent id targets another local subagent. An eligible completed subagent resumes with the same identity.

A child whose parent is the root session cannot message the root. Curated harness toolsets never receive the tool. A capability mode that excludes this kind also removes it.

Child senders are bounded: 4 in-flight messages per sender-target pair, and 32 outbound messages per sender attempt. A send over the limit returns `QuotaExceeded`.

An inactive subagent always wakes with the message as its next turn. For an active subagent, the optional `delivery` parameter controls how the message lands:

- `steer` (the default) joins the current turn at its next safe point.
- `queue` waits as a protected later turn instead of entering the active turn.
- `interject` is urgent: it is delivered ahead of pending steers at the earliest safe point, and it interrupts a subagent that is blocked waiting on background work so the subagent reads the message at once. Only the wait call ends early. The background work keeps running.

If the subagent is active but between turns, `steer` and `interject` each become one protected queued turn and the subagent starts on it. The legacy `queue: true` flag is still accepted and means `delivery: "queue"`. `delivery` wins when both are present.

The transcript shows each send as a one-line `Message` row: a verb for the outcome, then the subagent's label (its persona, role, tag, or Subagent fallback) and its description in curly quotes, as its `Subagent …: “…”` scrollback row quotes it, clamped to the first line and 40 characters. The verb carries the delivery, so a steer stays unmarked:

- `Message sent to Subagent “find callers”` (steer)
- `Message queued for Subagent “find callers”` / `Message interjected to Subagent “find callers”`
- `Message sending to …` with an animated bullet while the send is in flight
- `Message rejected · Subagent “find callers”` for a refused send, `Message unconfirmed · Subagent “find callers”` for one the shell could not confirm
- `Message sent to parent` when a child messages its parent

The collapsed row never shows the message or the reason. **Right** (or `l`/`e` in vim mode) expands the row to show the requested delivery, the full message text, and the reason of a rejected or unconfirmed send; **Left** (or `h`) collapses it again. **Enter**, **Ctrl+F**, or a double-click on the row opens that subagent's view, exactly as on its `Subagent` row (Right/Left still fold it). If the subagent was never spawned in this session (a headless `grok export`, or an id from another session), the row names it `subagent …xxxxxxxx` from the last 8 characters of its id, shows the raw `Subagent ID:` when expanded, and cannot open it.

### Listing subagents (`list_agents`)

`list_agents` enumerates the subagents this session spawned, so the model does not have to remember ids.

| Parameter | Description |
| --- | --- |
| `include_finished` | Include finished subagents. Defaults to `true`. |
| `parent_id` | Restrict the result to descendants of this subagent id; omitted lists the whole session tree. |
| `limit` | Maximum rows, 1-50. Defaults to 50. |

Each row carries `id`, `label`, `type`, `status`, `model`, `parent_id`, `depth`, `elapsed_ms`, `tokens_in`, `tokens_out`, `context_pct`, `current_tool`, `queue_position`, and `resumable`.

| `status` | Meaning |
| --- | --- |
| `queued` | Admitted, waiting for a concurrency slot. |
| `running` | Running a turn. |
| `idle` | Online, between turns. |
| `ready` | Finished with a resumable session kept. |
| `completed` / `failed` / `cancelled` | Terminal. |

A row with `resumable: true` (usually `ready`) can be continued with `resume_from`; a `failed` or `cancelled` row may keep only a partial record, so resuming it is not guaranteed. Use `id` with `send_subagent_message` to message a live row, or `kill_command_or_subagent` to stop it. Subagents owned by a workflow do not appear in the list.

---

## Capability Modes

Capability mode is not a spawn argument. A child's tools come from its **agent type** and any **role / definition default**. `general-purpose` and `implementer` use full toolsets; the built-in `explore` and `plan` types read and search but cannot run shell commands or edit files.

| Mode         | Read | Write | Execute | Description                                  |
| ------------ | ---- | ----- | ------- | -------------------------------------------- |
| `read-only`  | Yes  | No    | No      | Read, search, and inspect (also web search and LSP); no file edits or shell. |
| `read-write` | Yes  | Yes   | No      | Read, plus create, edit, delete, and move files. No shell. |
| `execute`    | Yes  | No    | Yes     | Read, plus run shell commands and background tasks. No file edits. |
| `all`        | Yes  | Yes   | Yes     | Unrestricted tool access. Default for `general-purpose`. |

---

## Context Inheritance

### resume_from

The `resume_from` parameter lets a new subagent continue where a finished subagent left off, which is useful for multi-stage workflows:

1. Spawn a research subagent to investigate a problem.
2. Spawn a second subagent with `resume_from` set to the first subagent's ID, so it picks up with the full research context.

Boundaries:

- The source must be finished (`completed` / `failed` / `cancelled`) and belong to the current parent session; its session record must still be in the current workspace's session directory. `resumable` in `list_agents` reflects this.
- A still-running source no longer fails outright: the `prompt` is queued as a follow-up with `send_subagent_message`'s `queue` semantics and runs as the child's next turn; the call returns an explanatory text result.
- The resumed child inherits the source's transcript, tool state, model, and subagent type. A passed `subagent_type` or `model` is ignored, and the system prompt and tools are re-rendered from the current definition. If the current definition lacks a tool the source used, that is only logged as a warning.
- The source transcript must fit the model's context window; a transcript over the resume limit fails with a message to compact first or use a larger-context model.
- A child truncated at `max_turns` returns `partial = true` and can be continued with `resume_from`.
- `context` and `cwd` are ignored when `resume_from` is set.

### MCP inheritance

The primary session overlays the active agent’s `mcpServers` frontmatter onto the disk/client merge by name (agent.md headers beat `config.toml`). Switching the primary agent replaces that overlay with the new seat only. Child inline `mcpServers` still become owned clients and beat inherited shared clients. Plugin agents cannot declare `mcpServers`.

Subagents inherit the parent session’s **already-connected** MCP servers by default. That includes local stdio/HTTP servers and plugin-sourced agents (for example `my-plugin:reviewer`). The child discovers and calls those tools with `search_tool` / `use_tool` the same way the parent does.

Control inheritance with agent frontmatter `mcpInheritance`:

| Value | Effect |
| ----- | ------ |
| `all` (default if omitted) | Inherit every parent-connected MCP server |
| `none` | Inherit no parent MCP servers |
| `named: [server, …]` | Inherit only the listed server names |
| `except: [server, …]` | Inherit all parent servers except the listed names |

Example:

```yaml
---
name: research-only
description: Read MCP tools but not internal connectors
tools: search_tool, use_tool, Read
mcpInheritance:
  except:
    - internal-tools
---
```

**Plugin agents** inherit parent MCP the same way. For security they still cannot:

- Declare their own `mcpServers` in agent frontmatter (ignored with a warning)
- Declare hooks in agent frontmatter
- Set `permissionMode: bypassPermissions`

Plugin-bundled MCP servers (plugin `.mcp.json`) still attach to the **parent/session** after the plugin is trusted — they are not a child-only frontmatter declaration. See [Plugins](09-plugins.md) and [MCP Servers](07-mcp-servers.md).

---

## Isolation: Worktree Mode

For tasks that modify files, run a subagent in an isolated git worktree with `isolation: worktree`. This keeps the child's edits from conflicting with the parent's:

- The subagent works in its own copy of the working tree.
- Its changes stay isolated from the parent until you merge them.
- At completion, a worktree with no changes is removed and its path is cleared from the subagent record.
- A worktree with changes is kept, and the result carries `<worktree_path>` and `<worktree_actions>`:
  - apply: `x.ai/git/worktree/apply` with `{"worktreePath": "…", "mode": "merge"}` (`mode: "overwrite"` replaces parent files instead).
  - discard: `x.ai/git/worktree/remove` with `{"worktreePath": "…", "force": true}`.
- With `[features] subagent_worktree_snapshot` enabled, a changed worktree is snapshotted and then removed.

The worktree base is controlled by `[subagents] worktree_base`: `head` (the default, the parent worktree's current `HEAD`) or `default_branch`. The `GROK_SUBAGENTS_WORKTREE_BASE` environment variable takes precedence over the config file.

---

## Configuration

### Per-Type Toggles and Model Overrides

Disable specific agent types, or route them to a different model:

```toml
[subagents.toggle]
explore = true                       # default -- omit to keep enabled
plan = false                         # disable the plan subagent

[subagents.models]
explore = "grok-4.6"                 # route explore to a specific model
```

Per-type model overrides apply for any parent. Without an override, a subagent inherits the parent's model.

### Depth, total, and concurrency

```toml
[subagents]
max_depth = 2            # default 2; 1 means only the root session can spawn
max_total_agents = 16    # cumulative per root session (queued, running, and finished all count)
max_concurrent = 32      # concurrently running
limit_behavior = "queue" # queue (default) or fail
```

Environment variables: `GROK_SUBAGENTS_MAX_DEPTH`, `GROK_SUBAGENTS_MAX_TOTAL_AGENTS`, `GROK_MAX_CONCURRENT_SUBAGENTS`, `GROK_SUBAGENT_LIMIT_BEHAVIOR`. Zero does not disable these limits; values below 1 are clamped to 1.

### Model Selection by the Agent

The `spawn_subagent` tool offers the agent a `model` argument, and its description lists the models you can pick, for when you explicitly ask for a subagent on a different model. With `[features] subagent_model_inheritance = true` (or `GROK_SUBAGENT_MODEL_INHERITANCE=1`), both are hidden whenever every model in your picker is an xAI model: subagents then always inherit the parent's model, and a spawn that still names one fails with a message asking the agent to retry without it. Catalogs with a third-party model, a model with no declared family, or a catalog still loading keep the argument. `[subagents.models]` pins, roles, and personas are unaffected. Read when a session starts; changing it requires a restart. Precedence: a `requirements.toml`/MDM pin, then the environment variable, then `config.toml`, then remote settings, then the default (off).

You can also toggle it from `/settings` → Models → **Subagent model inheritance**:

- On: Grok cannot set models for subagents
- Off: Grok may choose a different model for a subagent. Takes effect after restart.
- NOTE: This setting only applies when all models are xAI "model_family". You likely don't need to configure this setting.

The row shows the value that applies after restart. Toggling writes `[features] subagent_model_inheritance = true` or `= false` (an explicit `false` overrides a remote `true`); `d` (reset) deletes the key so `managed_config.toml`, remote settings, or the default apply again. Agents already running keep the mode they started with. When a layer your `config.toml` cannot override decides the value — a `requirements.toml`/MDM pin, the environment variable, the `GROK_CONFIG` overlay, or an active campaign — both the toggle and the reset are refused with a toast that names that layer.

### Custom Roles and Personas

Define custom roles with their own capability and model defaults:

```toml
[subagents.roles.researcher]
description = "Deep research agent"
default_capability_mode = "read-only"
model = "grok-4.6"
prompt_file = ".grok/prompts/researcher.md"
```

Define custom personas with behavioral instructions:

```toml
[subagents.personas.concise]
instructions = "Be concise. No filler words."
# instructions_file = ".grok/personas/concise.md"  # or load from a file
```

Grok Build also discovers roles from `.grok/roles/*.toml` and personas from `.grok/personas/*.toml`. Inline `config.toml` definitions take precedence over files.

---

## The Tasks Pane (TUI)

Grok Build shows running and finished work in side panes on the agent screen:

- Press `Ctrl+G` to toggle the tasks pane, which lists active and completed subagents and background commands with their status.
- Press `Ctrl+T` to toggle the separate todo pane.

To view the available agent types and personas, open the command palette with `Ctrl+P` and choose **Manage Agents** (`/config-agents`).

Subagents appear at the top of the tasks pane in their own collapsible "Subagents" group.

---

## Viewing Subagents in the TUI

Subagents appear in several places in the interactive TUI:

### Scrollback (parent conversation history)

When a subagent is spawned, a compact lifecycle block is added to the *parent's* scrollback:

- `Subagent running: "do the thing" (Implementer · grok-4.6) · Thinking`
- Or for background subagents: `Subagent started: "..."`

While running, the block shows a live activity suffix (e.g. "Running: cargo test", "Compacting", "Retrying (2/3)") pulled from the child's turn tracker. The bullet animates (or is colored) according to state.

Press **Enter** (or Ctrl-F) on the block to open the subagent's full transcript.

For blocking subagents the single entry updates its bullet color when the child finishes. For background ones, a follow-up `Subagent completed/failed/cancelled in Xs: "..."` block is appended.

### Tasks pane (Ctrl+G)

As noted above — grouped under "Subagents", with spinners, elapsed times, and quick access to kill or inspect. Press `h` to toggle hide-completed / show-all.

### Dock (when enabled)

The dock above the prompt lists subagents. With the dock focused, `h` toggles hide-completed / show-all (same filter as the tasks pane). Left / Right collapse and expand a section header.

### Fullscreen framed view (the child transcript)

When you open a subagent (from a scrollback block or the tasks pane), a bordered frame replaces the parent view and shows the child's full transcript:

- Title bar inside the frame: status icon (spinner / ✓ / ✗), label + bold description + model, optional "resumed"/"forked" badge, live activity · elapsed time, and [✗] close button.
- The child's own scrollback, thinking, and tool calls render inside the frame.
- The parent tasks pane, todos pane, and dock hide for the duration of the view.

This view is observational. The composer is hidden (zero rows). You cannot focus it, type a prompt, stash a draft, or send a follow-up from here. The parent session still owns prompts. To steer a running child, close the view and use `send_subagent_message` from the parent (see [Sending messages to subagents](#sending-messages-to-subagents)).

**What still works**

- Scroll, fold, copy, open links, and open the block viewer on the child's transcript.
- `Ctrl+C` cancels **this child's** turn. It does not cancel the parent.
- `Ctrl+.` / `Ctrl+X` opens the shortcuts cheatsheet for the child's keys.
- The child view paints no `[Dashboard]` button. Inside the dashboard overlay the button is the way back.
- Idle `Enter` in the **block viewer** quotes the selected line into the parent composer and closes the view.

**What does nothing (fail closed)**

Root-only chords never start on this surface. They do not open a modal on the child, and they do not leak to the parent:

- Command palette (`Ctrl+P`), model picker (`Ctrl+M`), session picker (`Ctrl+R`)
- Settings, extensions, always-approve (`Ctrl+O`), send-to-background (`Ctrl+B`)
- External prompt editor, Shift+Tab mode cycle

A denied action is a silent redraw. There is no toast.

If a prompt-queue overlay appears, it is a **read-only mirror**. You cannot edit, send-now, or remove rows. Queue RPCs always target the parent session.

**How to leave**

- `q` or `Esc` from bare scrollback, or click [✗].
- If scrollback search is open, `q` / `Esc` closes search first. A later press closes the view.
- `Ctrl+Q` always quits Grok. It is never swallowed here.

The parent's scrollback keeps showing the subagent's status after you close.

---

## Depth Limits

Subagents can spawn further subagents, nested up to two levels by default: root session (depth 0) → first-level child (depth 1) → second-level child (depth 2). `[subagents] max_depth` (or `GROK_SUBAGENTS_MAX_DEPTH`) changes the cap; `1` allows only the root session to spawn.

At the depth cap, `spawn_subagent` stays visible and the call returns an explicit depth-limit error (`Subagent depth limit exceeded (current depth: …, max: …)`) instead of the tool silently disappearing.

Two more caps apply: `max_total_agents` (default 16) cumulative children per root session, and `max_concurrent` (default 32) running at once. A spawn over the cumulative cap is rejected with a message to collect existing results or reuse one with `resume_from`; a spawn over the concurrency cap queues by default, or fails immediately with `limit_behavior = "fail"`.

---

## When to Use Subagents

**Good use cases:**

- Researching a codebase while the parent continues other work
- Running tests in parallel while the parent implements changes
- Reviewing generated changes before you commit them
- Delegating independent tasks that do not depend on each other

**When not to use:**

- Simple tasks that the parent can handle directly
- Tasks that require tight back-and-forth with the user, since a subagent runs autonomously and isn't suited to interactive exchanges
- Tasks where the context setup cost exceeds the parallelism benefit
