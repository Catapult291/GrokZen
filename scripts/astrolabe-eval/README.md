# Astrolabe tool-selection eval

Phase 4 (§10.3) asks whether the seven native tools actually get chosen, and
whether they reduce calls, turns and tokens compared with `grep`/`lsp` alone.
This directory holds the task set and the runner.

## Layout

| File | Purpose |
|---|---|
| `tasks.toml` | 36 tasks in 12 categories, each with the routing matrix's first choice (`expect`) and the fallbacks that still solve it (`accept`) |
| `run.py` | Runs each task as one headless turn per arm, records the first tool, call order, turns, tokens and wall time, writes `results-*.jsonl` and `summary-*.md` |

## Arms

| Arm | Environment | Tools available |
|---|---|---|
| `auto` | `GROK_ASTROLABE=auto` (default) | 7 Astrolabe tools + `grep` + `lsp` + `read_file` |
| `off` | `GROK_ASTROLABE=off` | baseline: `grep` + `lsp` + `read_file` |

By default the runner also creates an isolated `GROK_HOME` (`results/home-*`)
whose `config.toml` is a copy of the user's with `astrolabe` added to
`disabled_mcp_servers`. Without it the baseline arm reaches for
`search_tool` → `astrolabe__search_code`, so it would not measure grep/lsp.
Pass `--no-isolated-home` to run against the real home.

Optional extra arms for a later pass: `GROK_ASTROLABE_INDEX=1` for
`mode=index`, and `GROK_ASTROLABE_REPLACE_GREP=0` for the grep-first Search
slot (`mode` and `replace_grep` are honored by the same environment layer).

## Running

```bash
# Harness check: prints the exact command lines, calls no model.
python scripts/astrolabe-eval/run.py --dry-run

# Smoke pass (6 tasks, both arms) against a debug build.
cargo build -p xai-grok-pager-bin
python scripts/astrolabe-eval/run.py --limit 6

# Full sweep.
python scripts/astrolabe-eval/run.py --arms auto,off

# Full sweep with a turn cap: open-ended "where do I start" tasks can loop.
python scripts/astrolabe-eval/run.py --arms auto,off --max-turns 10
```

The runner requires a binary with `-p/--print`, `--output-format
streaming-json` and `--always-approve`; override the path with `--bin`.
Debug builds are fine for selection/turn/token numbers; wall time is only
comparable between arms built the same way. Results default to
`../astrolabe-eval-results` (next to the repository, not inside it) because
the isolated `GROK_HOME` contains session transcripts that must not end up in
the index or `git status`.

## Reading the summary

- **strict** — first tool equals `expect`. This is the routing quality signal.
- **acceptable** — first tool is in `accept`. A fallback can solve the task;
  a lower strict rate with a high acceptable rate means the tool table is
  working but the routing hints are not decisive.
- **avg calls / turns** — fewer means the result carried enough context.
- **tokens** — input and output for the whole turn, summed over responses.
- **Misses** — one line per task with the expected tool and the actual first
  calls, so a routing regression can be traced to a description or an order
  change rather than re-read raw transcripts.

Tasks are prompts, not fixtures: they run against this repository, so symbol
names in `tasks.toml` must be updated when the code they mention moves.
