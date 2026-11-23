//! Unit tests for the text-repetition guard (handoff §6.1).

use super::*;

/// Slice sizes the incident/fixture streams are replayed with: 4–64 characters, cycling.
const CHUNK_SIZES: [usize; 6] = [8, 16, 32, 4, 64, 7];

/// Feed `text` in 4–64-character slices; returns `(trip, characters fed when it fired)`.
fn feed_chunked(guard: &mut TextRepetitionGuard, text: &str) -> Option<(TextRepetitionTrip, u64)> {
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0usize;
    let mut fed = 0u64;
    let mut cycle = 0usize;
    while index < chars.len() {
        let size = CHUNK_SIZES[cycle % CHUNK_SIZES.len()].min(chars.len() - index);
        let slice: String = chars[index..index + size].iter().collect();
        fed += slice.len() as u64;
        if let Some(trip) = guard.on_text_chunk(&slice) {
            return Some((trip, fed));
        }
        index += size;
        cycle += 1;
    }
    None
}

/// The Bettbox incident message (session `01a136c5`, 0-based message 256), reconstructed:
/// one normal sentence followed by 37,397 `<br>` units — the 187 KB single-message flood of
/// 2026-10-14 04:02 (UTC+8) that none of the existing defenses caught.
fn incident_flood() -> String {
    let mut text =
        String::from("「进程分流」入口已显示（OCR rect [224,278,325,306]）。点击进入：\n");
    for _ in 0..37_397 {
        text.push_str("<br>\n");
    }
    text
}

/// ~4,700 characters of content that must never trip: a markdown table, nested lists, a code
/// block with repeated `console.log` lines, dividers, blockquotes, and blank-line runs.
fn legal_content() -> String {
    let mut text = String::new();
    text.push_str("# Section\n\n");
    text.push_str("| id | value | note |\n|---|---|---|\n");
    for row in 0..12 {
        text.push_str(&format!("| {row} | same | cell |\n"));
    }
    text.push_str("\nFirst paragraph of ordinary prose that explains the table above.\n\n");
    text.push_str("- first bullet item\n- second bullet item\n- third bullet item\n");
    text.push_str("  1. nested numbered one\n  2. nested numbered two\n");
    text.push_str("\n\n\n");
    text.push_str("```rust\n");
    for _ in 0..20 {
        text.push_str("console.log('x');\n");
    }
    text.push_str("```\n\n");
    text.push_str("===\n\n");
    text.push_str("A line of prose after the divider, wrapping up the section.\n");
    text.push_str("----\n\n");
    text.push_str("> quoted line one\n> quoted line two\n> quoted line three\n\n");
    text.push_str("\n\n\n");
    text.push_str("Closing paragraph with its own distinct wording entirely.\n\n");
    // Pad past 5,000 characters with unique non-periodic words.
    let mut i = 0usize;
    while text.chars().count() < 5_200 {
        text.push_str(&format!(" filler{i}"));
        i += 1;
    }
    text
}

#[test]
fn incident_flood_trips_within_two_thousand_chars() {
    let mut guard = TextRepetitionGuard::new(TextRepetitionGuardPolicy::default());
    let (trip, fed) = feed_chunked(&mut guard, &incident_flood())
        .expect("the incident flood must trip the guard");
    assert!(
        fed <= 2_000,
        "trip must fire within 2,000 generated characters, fired at {fed} ({:?})",
        trip.trigger
    );
    assert!(
        matches!(
            trip.trigger,
            TextRepetitionTrigger::IdenticalRun | TextRepetitionTrigger::ChunkPeriodicity
        ),
        "unexpected trigger {:?}",
        trip.trigger
    );
}

/// The same assertion replayed against the real session transcript when this machine has it.
/// Skips silently elsewhere so the test still runs in fresh checkouts.
#[test]
fn incident_jsonl_fixture_trips_within_two_thousand_chars() {
    let path = std::path::Path::new(
        r"C:\Users\bypassnro\.grok\sessions\C%3A%5CUsers%5Cbypassnro%5CDocuments%5Cproject%5CBettbox-Mod\01a136c5-9378-7631-8b84-4623552291a9\chat_history.jsonl",
    );
    let Ok(file) = std::fs::File::open(path) else {
        eprintln!(
            "skipping: incident fixture not present at {}",
            path.display()
        );
        return;
    };
    use std::io::{BufRead, BufReader};
    let mut line = String::new();
    for (index, row) in BufReader::new(file).lines().enumerate() {
        let Ok(row) = row else { continue };
        if index == 256 {
            line = row;
            break;
        }
    }
    assert!(!line.is_empty(), "message 256 not found in fixture");
    let value: serde_json::Value = serde_json::from_str(&line).expect("fixture line must be JSON");
    let content = value
        .get("content")
        .and_then(serde_json::Value::as_str)
        .expect("assistant message must carry string content");
    assert_eq!(content.matches("<br>").count(), 37_397);

    let mut guard = TextRepetitionGuard::new(TextRepetitionGuardPolicy::default());
    let (trip, fed) =
        feed_chunked(&mut guard, content).expect("the real incident message must trip the guard");
    assert!(
        fed <= 2_000,
        "real fixture must trip within 2,000 characters, fired at {fed} ({:?})",
        trip.trigger
    );
}

#[test]
fn legal_markdown_content_never_trips() {
    let text = legal_content();
    assert!(
        text.chars().count() > 5_000,
        "fixture must cover the full 5,000-character acceptance window"
    );
    let mut guard = TextRepetitionGuard::new(TextRepetitionGuardPolicy::default());
    let trip = feed_chunked(&mut guard, &text);
    assert!(trip.is_none(), "legal content must not trip: {trip:?}");
}

#[test]
fn periodic_ascii_loop_trips_chunk_periodicity() {
    let mut guard = TextRepetitionGuard::new(TextRepetitionGuardPolicy::default());
    let mut text = String::new();
    for _ in 0..400 {
        text.push_str("abcde\n");
    }
    let (trip, fed) = feed_chunked(&mut guard, &text).expect("periodic loop must trip");
    assert_eq!(trip.trigger, TextRepetitionTrigger::ChunkPeriodicity);
    assert!(fed <= 2_000, "periodic loop tripped late at {fed}");
}

#[test]
fn two_token_alternation_trips_chunk_periodicity() {
    let mut guard = TextRepetitionGuard::new(TextRepetitionGuardPolicy::default());
    let mut text = String::new();
    for _ in 0..500 {
        text.push_str("ab cd ");
    }
    let (trip, _) = feed_chunked(&mut guard, &text).expect("alternating tokens must trip");
    assert_eq!(trip.trigger, TextRepetitionTrigger::ChunkPeriodicity);
}

/// D1's cap specifically: 63 identical tokens must not trip, the 64th must.
/// The whitespace between tokens grows (`x` + 1 space, `x` + 2 spaces, …) so the *text* is not
/// periodic — otherwise D2's much lower character-level threshold would fire first and the run
/// cap would be unobservable. Growing separators keep the stream under the 4,000-character tail
/// gate as well, so this exercises the primary run counter alone.
#[test]
fn identical_run_boundary_trips_only_at_the_cap() {
    let policy = TextRepetitionGuardPolicy::default();
    assert_eq!(policy.max_identical_token_run, 64);

    let mut below = String::new();
    for i in 0..63usize {
        below.push('x');
        below.push_str(&" ".repeat(i + 1));
    }
    let mut guard = TextRepetitionGuard::new(policy);
    assert!(
        feed_chunked(&mut guard, &below).is_none(),
        "63 identical tokens must stay under the 64-token cap"
    );

    let mut at_cap = below.clone();
    at_cap.push('x');
    at_cap.push(' ');
    let mut guard = TextRepetitionGuard::new(policy);
    let (trip, _) = feed_chunked(&mut guard, &at_cap).expect("the 64th token must trip");
    assert_eq!(trip.trigger, TextRepetitionTrigger::IdenticalRun);
    assert_eq!(trip.threshold, 64);
}

/// Once tripped the guard latches, and a fresh guard (the next attempt) starts clean.
#[test]
fn trip_latches_and_a_fresh_guard_starts_clean() {
    let mut guard = TextRepetitionGuard::new(TextRepetitionGuardPolicy::default());
    let flood = "yes ".repeat(500);
    assert!(guard.on_text_chunk(&flood).is_some());
    // Latched: further feeds never produce another trip (and never reset).
    assert!(guard.on_text_chunk(&flood).is_none());
    assert!(guard.on_text_chunk(&flood).is_none());

    let mut fresh = TextRepetitionGuard::new(TextRepetitionGuardPolicy::default());
    assert!(
        fresh
            .on_text_chunk("A single ordinary reply. Short and done.")
            .is_none()
    );
    assert!(fresh.on_text_chunk(&flood).is_some(), "fresh guard re-arms");
}

/// Repetition inside a fenced code block is never judged; the block's own fences reset tracking.
#[test]
fn code_block_content_is_never_judged() {
    let mut guard = TextRepetitionGuard::new(TextRepetitionGuardPolicy::default());
    let mut text = String::from("```javascript\n");
    for _ in 0..400 {
        text.push_str("spam\n");
    }
    text.push_str("```\n");
    text.push_str("And now a normal closing sentence.\n");
    assert!(guard.on_text_chunk(&text).is_none());
}

/// The tail-anchored double check arms past 4,000 characters: eight identical tokens at the end
/// of a long non-repeating reply trip even though the primary run counter is nowhere near 64.
#[test]
fn tail_window_double_check_trips_after_four_thousand_chars() {
    let mut filler = String::new();
    let mut i = 0usize;
    while filler.len() < 4_100 {
        filler.push_str(&format!("unique{i} "));
        i += 1;
    }
    let mut text = filler;
    for _ in 0..8 {
        text.push_str("zz ");
    }
    let mut guard = TextRepetitionGuard::new(TextRepetitionGuardPolicy::default());
    let (trip, _) = feed_chunked(&mut guard, &text).expect("tail double check must trip");
    assert_eq!(trip.trigger, TextRepetitionTrigger::IdenticalRun);
    assert!(
        trip.generated_chars >= 4_000,
        "tail check must be armed only past the window gate: {}",
        trip.generated_chars
    );
    assert!(trip.threshold >= TAIL_IDENTICAL_TOKENS as u32);
}

/// A wall of blank lines (the legal-content guardrail) never enters D2 and never trips.
#[test]
fn blank_line_runs_never_trip() {
    let mut guard = TextRepetitionGuard::new(TextRepetitionGuardPolicy::default());
    let mut text = String::from("Intro sentence.\n");
    for _ in 0..1_200 {
        text.push('\n');
    }
    text.push_str("Outro sentence.\n");
    assert!(guard.on_text_chunk(&text).is_none());
}

/// A divider wall (`===` lines) is classified as a divider, not as a token flood.
#[test]
fn divider_walls_never_trip() {
    let mut guard = TextRepetitionGuard::new(TextRepetitionGuardPolicy::default());
    let mut text = String::new();
    for _ in 0..600 {
        text.push_str("===\n");
    }
    assert!(guard.on_text_chunk(&text).is_none());
}

// ── 思考（reasoning）通道子配置 ──────────────────────────────────────────────

/// 思考流整句循环（stage-log §162 复现样本形状）：同一句无限重复、上游缺终止事件。
fn reasoning_sentence_loop() -> String {
    "That paddinginese text is not the task.\n\n".repeat(600)
}

/// 思考流符号洪泛（真实事故样本形状）：`!!9D4D4C…` 式无空白 token 无限流。
fn reasoning_symbol_flood() -> String {
    "!!9D4D4C".repeat(3_000)
}

/// 行长 60 字符的周期行：同块（50 字符窗口）以步长 60 重复 `repeats` 次；
/// 无列表/标题/围栏形状，D1 无连续同 token 长串，纯测 D2 阈值边界。
fn periodic_lines(repeats: usize) -> String {
    "reasoning loop boundary line for the guard check 0123456789\n".repeat(repeats)
}

#[test]
fn reasoning_guard_policy_defaults_and_derivation() {
    let policy = TextRepetitionGuardPolicy::default();
    assert!(policy.reasoning.enforce_on_reasoning, "思考通道默认启用");
    assert_eq!(
        policy.reasoning.max_identical_token_run, 128,
        "思考 D1 阈值"
    );
    assert_eq!(policy.reasoning.chunk_repeat_threshold, 16, "思考 D2 阈值");

    let derived = policy.reasoning_guard_policy();
    assert_eq!(derived.max_identical_token_run, 128);
    assert_eq!(derived.chunk_repeat_threshold, 16);
    // chunk_size 与历史窗沿用正文配置：两个 guard 实例共用检测实现，仅阈值不同。
    assert_eq!(derived.chunk_size, policy.chunk_size);
    assert_eq!(derived.history_limit, policy.history_limit);
}

#[test]
fn reasoning_chunk_threshold_is_looser_than_the_text_guard() {
    // 14 次同块重复：正文 guard（阈值 10）熔断，思考 guard（阈值 16）放行——
    // 思考文本天然重复率高，宁可漏报不误报。
    let text = periodic_lines(14);

    let mut text_guard = TextRepetitionGuard::new(TextRepetitionGuardPolicy::default());
    assert!(
        feed_chunked(&mut text_guard, &text).is_some(),
        "正文 guard 第 10 次同块重复即熔断"
    );

    let mut reasoning_guard =
        TextRepetitionGuard::new(TextRepetitionGuardPolicy::default().reasoning_guard_policy());
    assert!(
        feed_chunked(&mut reasoning_guard, &text).is_none(),
        "思考 guard 阈值 16：14 次重复必须放行"
    );

    // 补到 18 次重复后思考 guard 也熔断（D2 命中）。
    let mut reasoning_guard =
        TextRepetitionGuard::new(TextRepetitionGuardPolicy::default().reasoning_guard_policy());
    let (trip, fed) =
        feed_chunked(&mut reasoning_guard, &periodic_lines(18)).expect("18 次同块重复必须熔断");
    assert_eq!(trip.trigger, TextRepetitionTrigger::ChunkPeriodicity);
    assert!(fed <= 18 * 60, "同块周期熔断不应晚于第 18 行，实际 {fed}");
}

#[test]
fn reasoning_sentence_loop_trips_chunk_periodicity() {
    let mut guard =
        TextRepetitionGuard::new(TextRepetitionGuardPolicy::default().reasoning_guard_policy());
    let (trip, fed) =
        feed_chunked(&mut guard, &reasoning_sentence_loop()).expect("整句无限循环必须熔断");
    assert_eq!(trip.trigger, TextRepetitionTrigger::ChunkPeriodicity);
    assert!(fed < 4_000, "整句循环应在千字符级熔断，实际 {fed}");
}

#[test]
fn reasoning_symbol_flood_trips_early() {
    let mut guard =
        TextRepetitionGuard::new(TextRepetitionGuardPolicy::default().reasoning_guard_policy());
    let (trip, fed) =
        feed_chunked(&mut guard, &reasoning_symbol_flood()).expect("符号洪泛必须熔断");
    // 无空白 token 以 1024 字节强制刷出的 D1 需 128 次重复（约 128 KB），
    // D2 的 50 字符窗口以步长 8 重复，先于 D1 命中。
    assert_eq!(trip.trigger, TextRepetitionTrigger::ChunkPeriodicity);
    assert!(fed < 4_000, "符号洪泛应在千字符级熔断，实际 {fed}");
}

/// 正常长思考：编号列表、重述句式、重复关键词——结构化重复经 markdown 形状重置后不得触发。
#[test]
fn normal_reasoning_with_repetitive_structure_does_not_trip() {
    let mut text = String::from("Let me work through this step by step.\n\n");
    for i in 0..200 {
        text.push_str(&format!(
            "{i}. check candidate {i} against the constraint set\n"
        ));
        text.push_str(&format!(
            "   verdict for candidate {i}: keep it for the next pass\n\n"
        ));
    }
    let mut guard =
        TextRepetitionGuard::new(TextRepetitionGuardPolicy::default().reasoning_guard_policy());
    assert!(
        feed_chunked(&mut guard, &text).is_none(),
        "结构化重复的正常思考不得触发"
    );
}
