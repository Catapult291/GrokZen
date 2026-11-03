#!/usr/bin/env python3
"""Prompt-cache statistics for Grok Build sessions.

The session writes two usage streams to the unified log:

* ``shell.turn.inference_done`` - main-loop model calls (``ctx.channel == "main"``)
* ``shell.side.inference_done`` - auxiliary calls (flush/title/summary/suggest/...),
  tagged with ``ctx.channel == "side"`` and ``ctx.kind``

Keeping them separate is what makes a side channel's 0% misses stop dragging the
session-wide number down. ``shell.cache.cold`` events are emitted for both streams
and carry the same timestamp + prompt-token pair, so they can be aligned with:

* the 3055 shim log (``state/shim.jsonl``, events ``cold-cache-retry*`` / ``cold-cache-skip``)
* an AxonHub request log (JSONL; prompt tokens read from the common field shapes)

Alignment is by absolute time distance plus an exact prompt-token match, which is
the only shared key across the three processes.

Examples::

    python scripts/cache_miss_stats.py
    python scripts/cache_miss_stats.py --session 01a19fb5-...
    python scripts/cache_miss_stats.py --shim-log ../中转网关/cline-taskid-shim/state/shim.jsonl
    python scripts/cache_miss_stats.py --axonhub-log axonhub-requests.jsonl --json
    python scripts/cache_miss_stats.py --self-test
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

MAIN_EVENT = "shell.turn.inference_done"
SIDE_EVENT = "shell.side.inference_done"
COLD_EVENT = "shell.cache.cold"
SHIM_COLD_PREFIXES = ("cold-cache-retry", "cold-cache-skip")


def parse_ts(value: Any) -> float | None:
    if not isinstance(value, str) or not value.strip():
        return None
    text = value.strip()
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.timestamp() * 1000.0


def _number(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        return int(value)
    if isinstance(value, str) and value.isdigit():
        return int(value)
    return None


def _first_number(*values: Any) -> int | None:
    for value in values:
        number = _number(value)
        if number is not None:
            return number
    return None


def usage_tokens(record: dict[str, Any]) -> tuple[int | None, int | None]:
    """Prompt and cached prompt tokens from a Grok, shim, or AxonHub record."""
    ctx = record.get("ctx") if isinstance(record.get("ctx"), dict) else {}
    usage = record.get("usage") if isinstance(record.get("usage"), dict) else {}
    prompt_details = (
        usage.get("prompt_tokens_details")
        or usage.get("input_tokens_details")
        or record.get("prompt_tokens_details")
        or record.get("input_tokens_details")
        or {}
    )
    if not isinstance(prompt_details, dict):
        prompt_details = {}
    prompt = _first_number(
        ctx.get("prompt_tokens"),
        ctx.get("prompt"),
        record.get("prompt_tokens"),
        record.get("input_tokens"),
        record.get("prompt"),
        usage.get("prompt_tokens"),
        usage.get("input_tokens"),
    )
    cached = _first_number(
        ctx.get("cached_prompt_tokens"),
        ctx.get("cached"),
        record.get("cached_prompt_tokens"),
        record.get("cached_tokens"),
        record.get("cached"),
        usage.get("cached_prompt_tokens"),
        usage.get("cached_tokens"),
        prompt_details.get("cached_tokens"),
        usage.get("cache_read_input_tokens"),
        prompt_details.get("cache_read_input_tokens"),
    )
    return prompt, cached


def record_session(record: dict[str, Any]) -> str | None:
    for key in ("sid", "session_id", "sessionId", "taskId"):
        value = record.get(key)
        if isinstance(value, str) and value:
            return value
    ctx = record.get("ctx")
    if isinstance(ctx, dict):
        for key in ("session_id", "sessionId"):
            value = ctx.get(key)
            if isinstance(value, str) and value:
                return value
    return None


def load_jsonl(path: Path) -> tuple[list[dict[str, Any]], int]:
    records: list[dict[str, Any]] = []
    skipped = 0
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                skipped += 1
                continue
            if isinstance(value, dict):
                records.append(value)
    return records, skipped


@dataclass
class ChannelStats:
    requests: int = 0
    prompt_tokens: int = 0
    cached_tokens: int = 0

    def add(self, prompt: int | None, cached: int | None) -> None:
        self.requests += 1
        self.prompt_tokens += prompt or 0
        self.cached_tokens += cached or 0

    @property
    def hit_rate(self) -> float | None:
        if self.prompt_tokens <= 0:
            return None
        return self.cached_tokens / self.prompt_tokens

    def as_dict(self) -> dict[str, Any]:
        return {
            "requests": self.requests,
            "prompt_tokens": self.prompt_tokens,
            "cached_tokens": self.cached_tokens,
            "weighted_hit_rate": self.hit_rate,
        }


@dataclass
class ColdEvent:
    ts_ms: float
    channel: str
    kind: str | None
    model: str | None
    prompt: int | None
    cached: int | None
    session: str | None

    def as_dict(self) -> dict[str, Any]:
        return {
            "ts_ms": self.ts_ms,
            "channel": self.channel,
            "kind": self.kind,
            "model": self.model,
            "prompt_tokens": self.prompt,
            "cached_tokens": self.cached,
            "session": self.session,
        }


@dataclass
class Summary:
    main: ChannelStats = field(default_factory=ChannelStats)
    side: ChannelStats = field(default_factory=ChannelStats)
    by_side_kind: dict[str, ChannelStats] = field(default_factory=dict)
    cold_events: list[ColdEvent] = field(default_factory=list)
    skipped_lines: int = 0

    def as_dict(self) -> dict[str, Any]:
        return {
            "main": self.main.as_dict(),
            "side": self.side.as_dict(),
            "side_by_kind": {
                kind: stats.as_dict() for kind, stats in sorted(self.by_side_kind.items())
            },
            "cold_events": [event.as_dict() for event in self.cold_events],
            "skipped_lines": self.skipped_lines,
        }


def summarize(
    records: Iterable[dict[str, Any]],
    *,
    session: str | None = None,
    since_ms: float | None = None,
) -> Summary:
    summary = Summary()
    for record in records:
        msg = record.get("msg")
        is_main = msg == MAIN_EVENT
        is_side = msg == SIDE_EVENT
        is_cold = msg == COLD_EVENT
        if not (is_main or is_side or is_cold):
            continue
        if session is not None and record_session(record) != session:
            continue
        ts_ms = parse_ts(record.get("ts"))
        if since_ms is not None and (ts_ms is None or ts_ms < since_ms):
            continue
        ctx = record.get("ctx") if isinstance(record.get("ctx"), dict) else {}
        prompt, cached = usage_tokens(record)
        if is_main:
            summary.main.add(prompt, cached)
            continue
        if is_side:
            summary.side.add(prompt, cached)
            kind = ctx.get("kind")
            kind_key = kind if isinstance(kind, str) and kind else "unknown"
            summary.by_side_kind.setdefault(kind_key, ChannelStats()).add(prompt, cached)
            continue
        channel = ctx.get("channel")
        kind = ctx.get("kind")
        model = ctx.get("model")
        summary.cold_events.append(
            ColdEvent(
                ts_ms=ts_ms or 0.0,
                channel=channel if isinstance(channel, str) and channel else "main",
                kind=kind if isinstance(kind, str) and kind else None,
                model=model if isinstance(model, str) and model else None,
                prompt=prompt,
                cached=cached,
                session=record_session(record),
            )
        )
    return summary


@dataclass
class Candidate:
    ts_ms: float
    prompt: int | None
    label: str
    session: str | None
    source: str


def shim_candidates(records: Iterable[dict[str, Any]]) -> list[Candidate]:
    candidates: list[Candidate] = []
    for record in records:
        event = record.get("event")
        if not isinstance(event, str) or not event.startswith(SHIM_COLD_PREFIXES):
            continue
        ts_ms = parse_ts(record.get("ts"))
        if ts_ms is None:
            continue
        prompt, _ = usage_tokens(record)
        candidates.append(
            Candidate(
                ts_ms=ts_ms,
                prompt=prompt,
                label=event,
                session=record_session(record),
                source="3055",
            )
        )
    return candidates


def axonhub_candidates(records: Iterable[dict[str, Any]]) -> list[Candidate]:
    candidates: list[Candidate] = []
    for record in records:
        ts_ms = parse_ts(record.get("ts") or record.get("timestamp") or record.get("time"))
        if ts_ms is None:
            continue
        prompt, _ = usage_tokens(record)
        if prompt is None:
            continue
        status = record.get("status") or record.get("status_code")
        model = record.get("model")
        label_parts = ["axonhub"]
        if isinstance(model, str) and model:
            label_parts.append(model)
        if status is not None:
            label_parts.append(str(status))
        candidates.append(
            Candidate(
                ts_ms=ts_ms,
                prompt=prompt,
                label=" ".join(label_parts),
                session=record_session(record),
                source="axonhub",
            )
        )
    return candidates


@dataclass
class Alignment:
    source: str
    window_ms: int
    prompt_tolerance: int
    matched: list[tuple[ColdEvent, Candidate]] = field(default_factory=list)
    unmatched: list[ColdEvent] = field(default_factory=list)

    def as_dict(self) -> dict[str, Any]:
        return {
            "source": self.source,
            "window_ms": self.window_ms,
            "prompt_tolerance": self.prompt_tolerance,
            "matched": len(self.matched),
            "unmatched": len(self.unmatched),
            "pairs": [
                {"cold": cold.as_dict(), "candidate": candidate.label, "delta_ms": delta}
                for cold, candidate, delta in self.pairs()
            ],
        }

    def pairs(self) -> Iterable[tuple[ColdEvent, Candidate, int]]:
        for cold, candidate in self.matched:
            yield cold, candidate, int(candidate.ts_ms - cold.ts_ms)


def align(
    cold_events: Iterable[ColdEvent],
    candidates: Iterable[Candidate],
    *,
    source: str,
    window_ms: int = 5_000,
    prompt_tolerance: int = 0,
    session: str | None = None,
) -> Alignment:
    pool = [
        candidate
        for candidate in candidates
        if session is None or candidate.session == session
    ]
    result = Alignment(source=source, window_ms=window_ms, prompt_tolerance=prompt_tolerance)
    for cold in cold_events:
        best: tuple[int, Candidate] | None = None
        for candidate in pool:
            if candidate.prompt is None or cold.prompt is None:
                continue
            if abs(candidate.prompt - cold.prompt) > prompt_tolerance:
                continue
            delta = abs(candidate.ts_ms - cold.ts_ms)
            if delta > window_ms:
                continue
            if best is None or delta < best[0]:
                best = (int(delta), candidate)
        if best is None:
            result.unmatched.append(cold)
        else:
            result.matched.append((cold, best[1]))
    return result


def format_rate(stats: ChannelStats) -> str:
    rate = stats.hit_rate
    return "-" if rate is None else f"{rate * 100:.2f}%"


def print_report(
    summary: Summary,
    alignments: list[Alignment],
    *,
    shim_count: int | None,
    axonhub_count: int | None,
) -> None:
    rows = [("main", summary.main), ("side", summary.side)]
    for kind, stats in sorted(summary.by_side_kind.items()):
        rows.append((f"  {kind}", stats))
    width = max(len(label) for label, _ in rows) + 2
    print(f"{'channel':<{width}}{'requests':>10}{'prompt':>14}{'cached':>14}{'weighted hit':>14}")
    for label, stats in rows:
        print(
            f"{label:<{width}}{stats.requests:>10}{stats.prompt_tokens:>14}"
            f"{stats.cached_tokens:>14}{format_rate(stats):>14}"
        )
    print()
    print(f"cold events: {len(summary.cold_events)}")
    if shim_count is not None:
        print(f"  3055 cold candidates: {shim_count}")
    if axonhub_count is not None:
        print(f"  AxonHub candidates: {axonhub_count}")
    for alignment in alignments:
        print(
            f"  align[{alignment.source}] window={alignment.window_ms}ms "
            f"tolerance={alignment.prompt_tolerance}: "
            f"{len(alignment.matched)} matched / {len(alignment.unmatched)} unmatched"
        )
        for cold, candidate in alignment.matched:
            delta = int(candidate.ts_ms - cold.ts_ms)
            print(
                f"    {cold.channel}{'/' + cold.kind if cold.kind else ''} "
                f"prompt={cold.prompt} cached={cold.cached} -> {candidate.label} ({delta:+d} ms)"
            )
        for cold in alignment.unmatched:
            print(
                f"    {cold.channel}{'/' + cold.kind if cold.kind else ''} "
                f"prompt={cold.prompt} cached={cold.cached} -> no match"
            )


def default_grok_log() -> Path:
    return Path.home() / ".grok" / "logs" / "unified.jsonl"


def self_test() -> None:
    grok = [
        {
            "ts": "2026-11-02T12:00:00.000Z",
            "sid": "s1",
            "msg": MAIN_EVENT,
            "ctx": {"channel": "main", "prompt_tokens": 100_000, "cached_prompt_tokens": 90_000},
        },
        {
            "ts": "2026-11-02T12:01:00.000Z",
            "sid": "s1",
            "msg": MAIN_EVENT,
            "ctx": {"channel": "main", "prompt_tokens": 50_000, "cached_prompt_tokens": 0},
        },
        {
            "ts": "2026-11-02T12:01:05.000Z",
            "sid": "s1",
            "msg": SIDE_EVENT,
            "ctx": {
                "channel": "side",
                "kind": "memory_flush",
                "prompt_tokens": 4_000,
                "cached_prompt_tokens": 0,
            },
        },
        {
            "ts": "2026-11-02T12:01:10.000Z",
            "sid": "s1",
            "msg": COLD_EVENT,
            "ctx": {"channel": "main", "prompt": 50_000, "cached": 0},
        },
        {
            "ts": "2026-11-02T12:01:11.000Z",
            "sid": "s1",
            "msg": COLD_EVENT,
            "ctx": {"channel": "side", "kind": "title_refresh", "prompt": 60_000, "cached": 0},
        },
    ]
    summary = summarize(grok)
    assert summary.main.requests == 2
    assert summary.main.prompt_tokens == 150_000
    assert summary.main.cached_tokens == 90_000
    assert summary.side.requests == 1
    assert summary.by_side_kind["memory_flush"].requests == 1
    assert len(summary.cold_events) == 2

    shim = [
        {
            "ts": "2026-11-02T12:01:10.250Z",
            "event": "cold-cache-retry",
            "prompt": 50_000,
            "cached": 0,
            "taskId": "s1",
        },
        {
            "ts": "2026-11-02T12:01:20.000Z",
            "event": "cold-cache-retry",
            "prompt": 60_000,
            "cached": 0,
            "taskId": "s1",
        },
    ]
    alignment = align(
        summary.cold_events, shim_candidates(shim), source="3055", window_ms=5_000
    )
    assert len(alignment.matched) == 1
    assert alignment.matched[0][0].channel == "main"
    assert len(alignment.unmatched) == 1
    assert alignment.unmatched[0].channel == "side"

    filtered = summarize(grok, session="other")
    assert filtered.main.requests == 0

    print("self-test: ok")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--grok-log", type=Path, default=default_grok_log())
    parser.add_argument("--shim-log", type=Path)
    parser.add_argument("--axonhub-log", type=Path)
    parser.add_argument("--session", help="session id / taskId to restrict to")
    parser.add_argument("--since", help="ISO-8601 lower bound (inclusive)")
    parser.add_argument("--window-ms", type=int, default=5_000)
    parser.add_argument("--prompt-tolerance", type=int, default=0)
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)

    if args.self_test:
        self_test()
        return 0

    since_ms = None
    if args.since:
        since_ms = parse_ts(args.since)
        if since_ms is None:
            parser.error(f"--since is not a timestamp: {args.since}")
    if not args.grok_log.exists():
        parser.error(f"grok log not found: {args.grok_log}")

    grok_records, grok_skipped = load_jsonl(args.grok_log)
    summary = summarize(grok_records, session=args.session, since_ms=since_ms)
    summary.skipped_lines = grok_skipped

    alignments: list[Alignment] = []
    shim_count = None
    axonhub_count = None
    if args.shim_log:
        if not args.shim_log.exists():
            parser.error(f"shim log not found: {args.shim_log}")
        shim_records, _ = load_jsonl(args.shim_log)
        candidates = shim_candidates(shim_records)
        shim_count = len(candidates)
        alignments.append(
            align(
                summary.cold_events,
                candidates,
                source="3055",
                window_ms=args.window_ms,
                prompt_tolerance=args.prompt_tolerance,
                session=args.session,
            )
        )
    if args.axonhub_log:
        if not args.axonhub_log.exists():
            parser.error(f"axonhub log not found: {args.axonhub_log}")
        axonhub_records, _ = load_jsonl(args.axonhub_log)
        candidates = axonhub_candidates(axonhub_records)
        axonhub_count = len(candidates)
        alignments.append(
            align(
                summary.cold_events,
                candidates,
                source="axonhub",
                window_ms=args.window_ms,
                prompt_tolerance=args.prompt_tolerance,
                session=args.session,
            )
        )

    if args.json:
        payload = summary.as_dict()
        payload["alignment"] = [alignment.as_dict() for alignment in alignments]
        print(json.dumps(payload, indent=2, ensure_ascii=False))
    else:
        print_report(
            summary,
            alignments,
            shim_count=shim_count,
            axonhub_count=axonhub_count,
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
