#!/usr/bin/env python3
"""Phase 4 tool-selection eval for the native Astrolabe tools.

Runs every task in ``tasks.toml`` as one headless turn (``grok-zh -p ...``)
against two arms:

* ``auto``  — the Astrolabe tools are registered (``GROK_ASTROLABE=auto``);
* ``off``   — the baseline, ``GROK_ASTROLABE=off``, leaving grep/lsp/read_file.

For each run it records the first tool called, the full call order, turns
(assistant responses), tokens and wall time, then prints and writes a summary.

Examples
--------
::

    # Validate the harness without a binary or model calls:
    python scripts/astrolabe-eval/run.py --dry-run

    # Smoke 6 tasks against a debug build:
    python scripts/astrolabe-eval/run.py --limit 6

    # Full sweep, both arms:
    python scripts/astrolabe-eval/run.py --arms auto,off

    # Repeat each task N times and majority-vote the first tool:
    python scripts/astrolabe-eval/run.py --arms auto,off --repeat 3

The binary must understand ``-p/--print``, ``--output-format
streaming-json`` and ``--always-approve``; ``--dry-run`` prints the exact
command lines.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shlex
import shutil
import subprocess
import sys
import time
import tomllib
from collections import Counter
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
LIMIT_KEYS = ("max_turns", "timeout_s")
MAX_TURNS_MARKERS = ("max turns reached", "max_turns_reached")
IO_DENIED_MARKERS = ("0x80070005", "access is denied", "拒绝访问")
# `hover_symbol` expects `lsp` in the tool table. Without a non-empty
# lsp.json plus GROK_LSP_TOOLS the tool is not registered, so both arms
# score 0/3 for environment reasons rather than routing.
LSP_CATEGORIES = frozenset({"hover_symbol"})


def load_tasks(path: Path) -> list[dict]:
    with path.open("rb") as handle:
        data = tomllib.load(handle)
    tasks = data.get("task", [])
    category_limits = data.get("category_limits", {})
    seen: set[str] = set()
    for task in tasks:
        missing = {"id", "category", "prompt", "expect", "accept"} - task.keys()
        if missing:
            raise SystemExit(f"task {task.get('id', '?')} is missing {sorted(missing)}")
        if task["id"] in seen:
            raise SystemExit(f"duplicate task id {task['id']}")
        seen.add(task["id"])
        limits = category_limits.get(task["category"], {})
        for key in LIMIT_KEYS:
            value = task.get(key, limits.get(key))
            if value is not None:
                if int(value) <= 0:
                    raise SystemExit(f"task {task['id']}: {key} must be positive")
                task[key] = int(value)
    return tasks


def parse_stream(lines: list[str]) -> dict:
    """Extract tool calls, turns, tokens and the stop reason from NDJSON."""
    tools: list[str] = []
    usage_count = 0
    usage_sum = {"input_tokens": 0, "output_tokens": 0, "cache_read_input_tokens": 0}
    end: dict = {}
    errors: list[str] = []
    for line in lines:
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind = obj.get("type") or obj.get("kind")
        if kind == "tool_call":
            name = obj.get("toolName") or obj.get("title") or ""
            if name:
                tools.append(name)
        elif kind == "usage":
            usage_count += 1
            usage = obj.get("usage") or {}
            for key in usage_sum:
                usage_sum[key] += int(usage.get(key) or 0)
        elif kind == "end":
            end = obj
        elif kind == "error":
            errors.append(str(obj.get("message", ""))[:200])
    usage = end.get("usage") or usage_sum
    input_tokens = int(usage.get("input_tokens") or 0)
    output_tokens = int(usage.get("output_tokens") or 0)
    cache_tokens = int(usage.get("cache_read_input_tokens") or 0)
    total_tokens = int(
        usage.get("total_tokens") or input_tokens + output_tokens + cache_tokens
    )
    return {
        "tools": tools,
        "first_tool": tools[0] if tools else None,
        "turns": int(end.get("num_turns") or usage_count),
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "cache_read_tokens": cache_tokens,
        "total_tokens": total_tokens,
        "stop_reason": end.get("stopReason"),
        "errors": errors,
    }


def find_rust_analyzer() -> str | None:
    """Locate rust-analyzer for the optional LSP fixture."""
    for name in ("rust-analyzer", "rust-analyzer.exe"):
        found = shutil.which(name)
        if found:
            return found
    for candidate in (
        Path.home() / ".cargo" / "bin" / "rust-analyzer.exe",
        Path.home() / ".cargo" / "bin" / "rust-analyzer",
    ):
        if candidate.is_file():
            return str(candidate)
    return None


def prepare_lsp_fixture(home: Path, enabled: bool) -> str | None:
    """Write an optional lsp.json into the isolated home.

    Preference order: the host's real ``~/.grok/lsp.json``, then a
    rust-analyzer fixture when the binary is on PATH or in ``~/.cargo/bin``.
    Returns the server description for the summary, or None when no fixture
    could be prepared (``hover_symbol`` then stays environment-gated).
    """
    if not enabled:
        return None
    user_lsp = Path.home() / ".grok" / "lsp.json"
    if user_lsp.is_file():
        (home / "lsp.json").write_bytes(user_lsp.read_bytes())
        return f"copied {user_lsp}"
    ra = find_rust_analyzer()
    if not ra:
        return None
    fixture = {
        "rust": {
            "command": ra,
            "args": [],
            "extensionToLanguage": {".rs": "rust"},
            "startupTimeout": 60000,
        }
    }
    (home / "lsp.json").write_text(
        json.dumps(fixture, indent=2) + "\n", encoding="utf-8"
    )
    return f"rust-analyzer fixture ({ra})"


def prepare_isolated_home(out_dir: Path) -> Path:
    """A GROK_HOME with the user's config but the astrolabe MCP server off.

    The native tools and the MCP server expose the same capabilities. Without
    this the baseline arm simply reaches for `search_tool` → `astrolabe__*`
    and stops being a grep/lsp baseline.
    """
    home = out_dir / "home"
    home.mkdir(parents=True, exist_ok=True)
    source = Path.home() / ".grok" / "config.toml"
    text = source.read_text(encoding="utf-8") if source.exists() else ""
    marker = "disabled_mcp_servers = ["
    if marker in text:
        text = text.replace(marker, f"{marker}\n    \"astrolabe\",", 1)
    else:
        text = f'disabled_mcp_servers = ["astrolabe"]\n{text}'
    (home / "config.toml").write_text(text, encoding="utf-8")
    return home


def _text(value) -> str:
    if isinstance(value, bytes):
        return value.decode("utf-8", "replace")
    return value or ""


def error_text(result: dict) -> str:
    joined = " ".join(result.get("errors") or [])
    return f"{joined} {result.get('stderr_tail', '')}".lower()


def is_io_denied(result: dict) -> bool:
    text = error_text(result)
    return any(marker in text for marker in IO_DENIED_MARKERS)


def classify(result: dict) -> str:
    """Routing outcome for finished runs, run-level outcome otherwise."""
    text = error_text(result)
    if result["exit_code"] == -1 or "[timeout]" in text:
        return "timeout"
    if any(marker in text for marker in MAX_TURNS_MARKERS):
        return "max_turns"
    if result["exit_code"] != 0:
        return "error"
    if result["strict"]:
        return "ok"
    if result["acceptable"]:
        return "alt"
    return "miss"


def build_cmd(args: argparse.Namespace, task: dict, max_turns: int) -> list[str]:
    cmd = [
        args.bin,
        "-p",
        task["prompt"],
        "--output-format",
        "streaming-json",
        "--always-approve",
    ]
    if max_turns:
        cmd += ["--max-turns", str(max_turns)]
    return cmd


def execute(
    args: argparse.Namespace, task: dict, arm: str, env: dict, max_turns: int, timeout: int
) -> dict:
    cmd = build_cmd(args, task, max_turns)
    started = time.monotonic()
    try:
        completed = subprocess.run(
            cmd,
            cwd=args.repo,
            env=env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
        )
        stdout = _text(completed.stdout)
        stderr = _text(completed.stderr)
        code = completed.returncode
    except subprocess.TimeoutExpired as expired:
        stdout = _text(expired.stdout)
        stderr = _text(expired.stderr) + "\n[timeout]"
        code = -1
    duration = time.monotonic() - started
    parsed = parse_stream(stdout.splitlines())
    result = {
        "id": task["id"],
        "category": task["category"],
        "arm": arm,
        "expect": task["expect"],
        "accept": task["accept"],
        "max_turns": max_turns,
        "timeout_s": timeout,
        "exit_code": code,
        "duration_s": round(duration, 1),
        "strict": parsed["first_tool"] == task["expect"],
        "acceptable": parsed["first_tool"] in task["accept"],
        "stderr_tail": stderr.strip()[-400:],
        **parsed,
    }
    if code != 0 and not result["errors"]:
        result["errors"] = [stderr.strip().splitlines()[-1][:200] if stderr.strip() else f"exit {code}"]
    return result


def run_one(
    args: argparse.Namespace, task: dict, arm: str, sample: int = 1
) -> dict:
    env = os.environ.copy()
    env["GROK_ASTROLABE"] = "off" if arm == "off" else "auto"
    env.setdefault("NO_COLOR", "1")
    if args.home_path:
        env["GROK_HOME"] = args.home_path
    if args.lsp_ready:
        # Feature flag is independent of the config file; both are required
        # for the model-visible `lsp` tool.
        env["GROK_LSP_TOOLS"] = "1"
    max_turns = task.get("max_turns") or args.max_turns
    timeout = task.get("timeout_s") or args.timeout
    if args.dry_run:
        cmd = build_cmd(args, task, max_turns)
        lsp_note = "GROK_LSP_TOOLS=1" if args.lsp_ready else "GROK_LSP_TOOLS unset"
        print(
            f"[dry-run] ({arm}) cwd={args.repo} GROK_ASTROLABE={env['GROK_ASTROLABE']} "
            f"{lsp_note} timeout={timeout}s sample={sample}/{args.repeat}"
        )
        print("          " + " ".join(shlex.quote(part) for part in cmd))
        return {**task, "arm": arm, "sample": sample, "dry_run": True}
    attempts = 0
    while True:
        attempts += 1
        result = execute(args, task, arm, env, max_turns, timeout)
        if attempts > args.retries or not is_io_denied(result):
            break
        print(
            f"[{arm}] {task['id']}: session I/O denied, retry {attempts}/{args.retries}",
            flush=True,
        )
        time.sleep(1.0)
    result["attempts"] = attempts
    result["sample"] = sample
    result["status"] = classify(result)
    return result


def aggregate_majority(results: list[dict]) -> list[dict]:
    """Collapse N samples of each (arm, task) into one majority-vote row.

    Routing (``first_tool`` / ``strict`` / ``acceptable`` / ``status``) uses
    the majority first tool among completed samples only. Ties prefer the
    expected tool, then the first accepted fallback, then lexicographic
    order. When every sample is unfinished the row keeps the most common
    incomplete status. Cost metrics (calls/turns/tokens/seconds) stay the
    arithmetic mean over all samples.
    """
    by_key: dict[tuple[str, str], list[dict]] = {}
    for row in results:
        if row.get("dry_run"):
            continue
        by_key.setdefault((row["arm"], row["id"]), []).append(row)

    aggregated: list[dict] = []
    for samples in by_key.values():
        samples = sorted(samples, key=lambda r: int(r.get("sample") or 1))
        base = samples[0]
        completed = [r for r in samples if r["status"] in ("ok", "alt", "miss")]
        incomplete = [r for r in samples if r["status"] not in ("ok", "alt", "miss")]
        votes = Counter(r.get("first_tool") for r in completed if r.get("first_tool"))
        sample_statuses = [r["status"] for r in samples]
        sample_first = [r.get("first_tool") for r in samples]

        if votes:
            top = [tool for tool, n in votes.items() if n == max(votes.values())]
            if base["expect"] in top:
                first_tool = base["expect"]
            else:
                first_tool = next((tool for tool in base["accept"] if tool in top), top[0])
            strict = first_tool == base["expect"]
            acceptable = first_tool in base["accept"]
            status = "ok" if strict else ("alt" if acceptable else "miss")
        else:
            # Every sample unfinished: keep the modal first tool for display
            # and the most common incomplete status.
            incomplete_votes = Counter(
                r.get("first_tool") for r in samples if r.get("first_tool")
            )
            first_tool = incomplete_votes.most_common(1)[0][0] if incomplete_votes else None
            strict = first_tool == base["expect"] if first_tool else False
            acceptable = first_tool in base["accept"] if first_tool else False
            status = Counter(sample_statuses).most_common(1)[0][0]

        def mean(key: str) -> float:
            return sum(float(r.get(key) or 0) for r in samples) / len(samples)

        aggregated.append(
            {
                "id": base["id"],
                "category": base["category"],
                "arm": base["arm"],
                "expect": base["expect"],
                "accept": base["accept"],
                "max_turns": base.get("max_turns"),
                "timeout_s": base.get("timeout_s"),
                "sample": len(samples),
                "samples": len(samples),
                "completed_samples": len(completed),
                "incomplete_samples": len(incomplete),
                "first_tool": first_tool,
                "strict": strict,
                "acceptable": acceptable,
                "status": status,
                "votes": dict(votes),
                "sample_statuses": sample_statuses,
                "sample_first_tools": sample_first,
                "tools": (completed[0] if completed else samples[0]).get("tools", []),
                "turns": int(mean("turns")),
                "input_tokens": int(mean("input_tokens")),
                "output_tokens": int(mean("output_tokens")),
                "total_tokens": int(mean("total_tokens")),
                "duration_s": round(mean("duration_s"), 1),
                "exit_code": 0 if completed else samples[0].get("exit_code", 0),
                "errors": [],
            }
        )
    aggregated.sort(key=lambda r: (r["arm"], r["id"]))
    return aggregated


def summarize(
    results: list[dict],
    arms: list[str],
    tasks: dict[str, dict],
    tasks_hash: str = "",
    lsp_ready: str | None = None,
    repeat: int = 1,
) -> str:
    lines = ["# Astrolabe tool-selection eval", ""]
    if tasks_hash:
        lines += [f"tasks: {len(tasks)} (`sha256:{tasks_hash}`)", ""]
    if repeat > 1:
        lines += [
            f"repeat: {repeat} (majority vote on first tool among completed samples)",
            "",
        ]
    if lsp_ready:
        lines += [f"lsp fixture: {lsp_ready} (`GROK_LSP_TOOLS=1`)", ""]
    else:
        lines += [
            "lsp fixture: none — `hover_symbol` is environment-gated "
            "(no host lsp.json / rust-analyzer); those tasks do not "
            "participate in the strict two-arm comparison.",
            "",
        ]
    header = (
        "| arm | tasks | completed | max turns | timeout+error | strict | acceptable "
        "| avg calls | avg turns | avg input tok | avg output tok | avg total tok | avg seconds |"
    )
    lines += [header, "|---" * 13 + "|"]
    for arm in arms:
        rows = [r for r in results if r["arm"] == arm and not r.get("dry_run")]
        if not rows:
            continue
        count = len(rows)
        statuses = Counter(r["status"] for r in rows)
        completed = sum(statuses[s] for s in ("ok", "alt", "miss"))
        failed = statuses["timeout"] + statuses["error"]
        strict = sum(1 for r in rows if r["strict"])
        acceptable = sum(1 for r in rows if r["acceptable"])
        calls = sum(len(r["tools"]) for r in rows) / count
        turns = sum(r["turns"] for r in rows) / count
        tokens_in = sum(r["input_tokens"] for r in rows) / count
        tokens_out = sum(r["output_tokens"] for r in rows) / count
        tokens_total = sum(r["total_tokens"] for r in rows) / count
        seconds = sum(r["duration_s"] for r in rows) / count
        lines.append(
            f"| {arm} | {count} | {completed} | {statuses['max_turns']} | {failed} "
            f"| {strict}/{count} | {acceptable}/{count} "
            f"| {calls:.1f} | {turns:.1f} | {tokens_in:.0f} | {tokens_out:.0f} "
            f"| {tokens_total:.0f} | {seconds:.1f} |"
        )
    lines += ["", "## Per category (strict / tasks, completed)", ""]
    categories = sorted({t["category"] for t in tasks.values()})
    rows = "| category | " + " | ".join(arms) + " |"
    lines += [rows, "|---|" + "---|" * len(arms)]
    for category in categories:
        cells = []
        for arm in arms:
            subset = [
                r
                for r in results
                if r["arm"] == arm and r["category"] == category and not r.get("dry_run")
            ]
            completed = sum(1 for r in subset if r["status"] in ("ok", "alt", "miss"))
            label = f"{sum(1 for r in subset if r['strict'])}/{len(subset)} ({completed})"
            if category in LSP_CATEGORIES and not lsp_ready:
                label += " no-lsp"
            cells.append(label)
        lines.append(f"| {category} | " + " | ".join(cells) + " |")
    if repeat > 1:
        lines += ["", "## Majority votes", ""]
        for arm in arms:
            rows = [
                r
                for r in results
                if r["arm"] == arm and not r.get("dry_run") and r.get("votes")
            ]
            if not rows:
                continue
            lines.append(f"### {arm}")
            lines.append("")
            for row in rows:
                vote_text = ", ".join(
                    f"{tool}={count}" for tool, count in sorted(
                        row["votes"].items(), key=lambda item: (-item[1], item[0] or "")
                    )
                )
                lines.append(
                    f"- `{row['id']}` majority `{row['first_tool']}` "
                    f"({row['completed_samples']}/{row['samples']} completed; "
                    f"{vote_text})"
                )
            lines.append("")
    lines += ["", "## Routing misses", ""]
    for arm in arms:
        misses = [
            r
            for r in results
            if r["arm"] == arm and not r.get("dry_run") and r["status"] == "miss"
        ]
        if not misses:
            continue
        lines.append(f"### {arm}")
        lines.append("")
        for miss in misses:
            samples_note = ""
            if repeat > 1:
                firsts = ", ".join(
                    str(tool) for tool in (miss.get("sample_first_tools") or [])
                )
                samples_note = f"; samples: {firsts or 'n/a'}"
            lines.append(
                f"- `{miss['id']}` expected `{miss['expect']}`, majority "
                f"`{miss['first_tool']}` (calls: {', '.join(miss['tools'][:6]) or 'none'})"
                f"{samples_note}"
            )
        lines.append("")
    lines += ["", "## Incomplete runs", ""]
    for arm in arms:
        incomplete = [
            r
            for r in results
            if r["arm"] == arm
            and not r.get("dry_run")
            and r["status"] in ("max_turns", "timeout", "error")
        ]
        if not incomplete:
            continue
        lines.append(f"### {arm}")
        lines.append("")
        for row in incomplete:
            detail = (row.get("errors") or [f"exit {row['exit_code']}"])[0].replace("\n", " ")
            lines.append(
                f"- `{row['id']}` {row['status']} after {row['turns']} turns, "
                f"first `{row['first_tool']}` (`max_turns`={row.get('max_turns')}, "
                f"timeout={row.get('timeout_s')}s): {detail}"
            )
        lines.append("")
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--bin", default=str(REPO / "target" / "debug" / "grok-zh.exe"))
    parser.add_argument("--repo", default=str(REPO))
    parser.add_argument("--tasks", default=str(HERE / "tasks.toml"))
    parser.add_argument("--arms", default="auto,off")
    parser.add_argument("--limit", type=int, default=0, help="run only the first N tasks")
    parser.add_argument("--task", action="append", default=[], help="run only these ids")
    parser.add_argument("--timeout", type=int, default=240)
    parser.add_argument(
        "--retries",
        type=int,
        default=1,
        help="retries for transient session I/O failures (0x80070005)",
    )
    parser.add_argument(
        "--max-turns",
        type=int,
        default=0,
        help="default turn cap when the task and its category set none (0 disables)",
    )
    parser.add_argument(
        "--repeat",
        type=int,
        default=1,
        help="run each task N times and majority-vote the first tool",
    )
    parser.add_argument("--out", default=str(REPO.parent / "astrolabe-eval-results"))
    parser.add_argument(
        "--isolated-home",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="run with a GROK_HOME whose config disables the astrolabe MCP server",
    )
    parser.add_argument(
        "--lsp-fixture",
        action=argparse.BooleanOptionalAction,
        default=True,
        help=(
            "prepare an lsp.json in the isolated home (host lsp.json, else "
            "rust-analyzer) and set GROK_LSP_TOOLS=1 so hover_symbol can see `lsp`"
        ),
    )
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    args.home_path = None
    args.lsp_ready = None

    arms = [arm.strip() for arm in args.arms.split(",") if arm.strip()]
    unknown = set(arms) - {"auto", "off"}
    if unknown:
        raise SystemExit(f"unknown arms: {sorted(unknown)}")
    if args.repeat < 1:
        raise SystemExit("--repeat must be >= 1")
    if not args.dry_run and not Path(args.bin).exists():
        raise SystemExit(f"binary not found: {args.bin} (build it or pass --bin)")

    tasks_path = Path(args.tasks)
    task_list = load_tasks(tasks_path)
    if args.task:
        wanted = set(args.task)
        task_list = [task for task in task_list if task["id"] in wanted]
    if args.limit:
        task_list = task_list[: args.limit]
    tasks_by_id = {task["id"]: task for task in task_list}

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    if args.dry_run and args.lsp_fixture and (args.isolated_home or (
        Path.home() / ".grok" / "lsp.json"
    ).is_file()):
        # Probe only: dry-run must not create a home tree.
        if (Path.home() / ".grok" / "lsp.json").is_file():
            args.lsp_ready = "host ~/.grok/lsp.json (dry-run)"
        elif find_rust_analyzer():
            args.lsp_ready = "rust-analyzer fixture (dry-run)"
    elif not args.dry_run and args.isolated_home:
        home_root = out / f"home-{stamp}"
        args.home_path = str(prepare_isolated_home(home_root))
        args.lsp_ready = prepare_lsp_fixture(Path(args.home_path), args.lsp_fixture)
        if args.lsp_fixture and not args.lsp_ready:
            print(
                "[lsp] no host ~/.grok/lsp.json and rust-analyzer not found; "
                "hover_symbol stays environment-gated",
                flush=True,
            )
        elif args.lsp_ready:
            print(f"[lsp] fixture: {args.lsp_ready}", flush=True)
    elif not args.dry_run and args.lsp_fixture:
        # Real home: only enable the feature flag when the host already has
        # a user lsp.json; the fixture path needs an isolated home to write to.
        user_lsp = Path.home() / ".grok" / "lsp.json"
        if user_lsp.is_file():
            args.lsp_ready = f"host {user_lsp}"
            print(f"[lsp] using host {user_lsp}", flush=True)
        else:
            print(
                "[lsp] no host ~/.grok/lsp.json and no isolated home; "
                "hover_symbol stays environment-gated",
                flush=True,
            )

    results: list[dict] = []
    live = out / "latest.jsonl"
    with live.open("w", encoding="utf-8") as live_handle:
        for arm in arms:
            for task in task_list:
                for sample in range(1, args.repeat + 1):
                    result = run_one(args, task, arm, sample)
                    results.append(result)
                    if not args.dry_run:
                        # Written per run so a long sweep keeps its data if the
                        # process is interrupted.
                        live_handle.write(json.dumps(result, ensure_ascii=False) + "\n")
                        live_handle.flush()
                        sample_note = (
                            f" sample {sample}/{args.repeat}" if args.repeat > 1 else ""
                        )
                        print(
                            f"[{arm}] {task['id']}{sample_note}: {result['status']} "
                            f"first={result['first_tool']} "
                            f"calls={len(result['tools'])} turns={result['turns']} "
                            f"{result['duration_s']}s",
                            flush=True,
                        )

    if args.dry_run:
        return 0

    jsonl = out / f"results-{stamp}.jsonl"
    with jsonl.open("w", encoding="utf-8") as handle:
        for result in results:
            handle.write(json.dumps(result, ensure_ascii=False) + "\n")
    tasks_hash = hashlib.sha256(tasks_path.read_bytes()).hexdigest()[:12]
    summary = summarize(
        aggregate_majority(results),
        arms,
        tasks_by_id,
        tasks_hash,
        lsp_ready=args.lsp_ready,
        repeat=args.repeat,
    )
    report = out / f"summary-{stamp}.md"
    report.write_text(summary, encoding="utf-8")
    print()
    print(summary)
    print(f"wrote {jsonl} and {report}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
