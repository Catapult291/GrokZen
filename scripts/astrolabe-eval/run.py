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

The binary must understand ``-p/--print``, ``--output-format
streaming-json`` and ``--always-approve``; ``--dry-run`` prints the exact
command lines.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import subprocess
import sys
import time
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent


def load_tasks(path: Path) -> list[dict]:
    with path.open("rb") as handle:
        data = tomllib.load(handle)
    tasks = data.get("task", [])
    seen: set[str] = set()
    for task in tasks:
        missing = {"id", "category", "prompt", "expect", "accept"} - task.keys()
        if missing:
            raise SystemExit(f"task {task.get('id', '?')} is missing {sorted(missing)}")
        if task["id"] in seen:
            raise SystemExit(f"duplicate task id {task['id']}")
        seen.add(task["id"])
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


def run_one(args: argparse.Namespace, task: dict, arm: str) -> dict:
    env = os.environ.copy()
    env["GROK_ASTROLABE"] = "off" if arm == "off" else "auto"
    env.setdefault("NO_COLOR", "1")
    if args.home_path:
        env["GROK_HOME"] = args.home_path
    cmd = [
        args.bin,
        "-p",
        task["prompt"],
        "--output-format",
        "streaming-json",
        "--always-approve",
    ]
    if args.max_turns:
        cmd += ["--max-turns", str(args.max_turns)]
    if args.dry_run:
        print(f"[dry-run] ({arm}) cwd={args.repo} GROK_ASTROLABE={env['GROK_ASTROLABE']}")
        print("          " + " ".join(shlex.quote(part) for part in cmd))
        return {**task, "arm": arm, "dry_run": True}
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
            timeout=args.timeout,
        )
        stdout, stderr, code = completed.stdout, completed.stderr, completed.returncode
    except subprocess.TimeoutExpired as expired:
        stdout = expired.stdout or ""
        stderr = (expired.stderr or "") + "\n[timeout]"
        code = -1
    duration = time.monotonic() - started
    parsed = parse_stream(stdout.splitlines())
    result = {
        "id": task["id"],
        "category": task["category"],
        "arm": arm,
        "expect": task["expect"],
        "accept": task["accept"],
        "exit_code": code,
        "duration_s": round(duration, 1),
        "strict": parsed["first_tool"] == task["expect"],
        "acceptable": parsed["first_tool"] in task["accept"],
        **parsed,
    }
    if code != 0 and not result["errors"]:
        result["errors"] = [stderr.strip().splitlines()[-1][:200] if stderr.strip() else f"exit {code}"]
    return result


def summarize(results: list[dict], arms: list[str], tasks: dict[str, dict]) -> str:
    lines = ["# Astrolabe tool-selection eval", ""]
    header = (
        "| arm | tasks | strict | acceptable | avg calls | avg turns "
        "| avg input tok | avg output tok | avg total tok | avg seconds |"
    )
    lines += [header, "|---|---|---|---|---|---|---|---|---|---|"]
    for arm in arms:
        rows = [r for r in results if r["arm"] == arm and not r.get("dry_run")]
        if not rows:
            continue
        count = len(rows)
        strict = sum(1 for r in rows if r["strict"])
        acceptable = sum(1 for r in rows if r["acceptable"])
        calls = sum(len(r["tools"]) for r in rows) / count
        turns = sum(r["turns"] for r in rows) / count
        tokens_in = sum(r["input_tokens"] for r in rows) / count
        tokens_out = sum(r["output_tokens"] for r in rows) / count
        tokens_total = sum(r["total_tokens"] for r in rows) / count
        seconds = sum(r["duration_s"] for r in rows) / count
        lines.append(
            f"| {arm} | {count} | {strict}/{count} | {acceptable}/{count} "
            f"| {calls:.1f} | {turns:.1f} | {tokens_in:.0f} | {tokens_out:.0f} "
            f"| {tokens_total:.0f} | {seconds:.1f} |"
        )
    lines += ["", "## Per category (strict / tasks)", ""]
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
            cells.append(f"{sum(1 for r in subset if r['strict'])}/{len(subset)}")
        lines.append(f"| {category} | " + " | ".join(cells) + " |")
    lines += ["", "## Misses", ""]
    for arm in arms:
        misses = [
            r
            for r in results
            if r["arm"] == arm and not r["strict"] and not r.get("dry_run")
        ]
        if not misses:
            continue
        lines.append(f"### {arm}")
        lines.append("")
        for miss in misses:
            lines.append(
                f"- `{miss['id']}` expected `{miss['expect']}`, first "
                f"`{miss['first_tool']}` (calls: {', '.join(miss['tools'][:6]) or 'none'})"
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
        "--max-turns",
        type=int,
        default=0,
        help="cap model turns per task (0 disables); 10 bounds open-ended tasks",
    )
    parser.add_argument("--out", default=str(REPO.parent / "astrolabe-eval-results"))
    parser.add_argument(
        "--isolated-home",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="run with a GROK_HOME whose config disables the astrolabe MCP server",
    )
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    args.home_path = None

    arms = [arm.strip() for arm in args.arms.split(",") if arm.strip()]
    unknown = set(arms) - {"auto", "off"}
    if unknown:
        raise SystemExit(f"unknown arms: {sorted(unknown)}")
    if not args.dry_run and not Path(args.bin).exists():
        raise SystemExit(f"binary not found: {args.bin} (build it or pass --bin)")

    task_list = load_tasks(Path(args.tasks))
    if args.task:
        wanted = set(args.task)
        task_list = [task for task in task_list if task["id"] in wanted]
    if args.limit:
        task_list = task_list[: args.limit]
    tasks_by_id = {task["id"]: task for task in task_list}

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    if not args.dry_run and args.isolated_home:
        args.home_path = str(prepare_isolated_home(out / f"home-{stamp}"))

    results: list[dict] = []
    live = out / "latest.jsonl"
    with live.open("w", encoding="utf-8") as live_handle:
        for arm in arms:
            for task in task_list:
                result = run_one(args, task, arm)
                results.append(result)
                if not args.dry_run:
                    # Written per run so a long sweep keeps its data if the
                    # process is interrupted.
                    live_handle.write(json.dumps(result, ensure_ascii=False) + "\n")
                    live_handle.flush()
                    state = (
                        "ok" if result["strict"] else ("alt" if result["acceptable"] else "MISS")
                    )
                    print(
                        f"[{arm}] {task['id']}: {state} first={result['first_tool']} "
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
    summary = summarize(results, arms, tasks_by_id)
    report = out / f"summary-{stamp}.md"
    report.write_text(summary, encoding="utf-8")
    print()
    print(summary)
    print(f"wrote {jsonl} and {report}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
