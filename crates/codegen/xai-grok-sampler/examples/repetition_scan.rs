//! Offline false-positive scan for the text-repetition guard (handoff §6.2).
//!
//! Walks every `chat_history.jsonl` under a sessions root, feeds each `type=assistant`
//! `content` through the real [`TextRepetitionGuard`] in stream-sized slices (the same 4–64
//! character cadence the unit tests use), and prints every trip with its file, line number,
//! trigger, and generated-character count.
//!
//! Acceptance: only the known Bettbox incident message should trip on a healthy corpus.
//!
//! ```text
//! cargo run -p xai-grok-sampler --example repetition_scan -- C:\Users\you\.grok\sessions
//! ```

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use xai_grok_sampler::TextRepetitionGuard;
use xai_grok_sampling_types::{TextRepetitionGuardPolicy, TextRepetitionTrip};

/// Same slice cadence as the unit tests: a stream arriving in 4–64 character pieces.
const CHUNK_SIZES: [usize; 6] = [8, 16, 32, 4, 64, 7];

/// Feed `text` like a stream; returns the first trip with the character count fed when it fired.
fn feed_chunked(guard: &mut TextRepetitionGuard, text: &str) -> Option<(TextRepetitionTrip, u64)> {
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0usize;
    let mut cycle = 0usize;
    let mut fed = 0u64;
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

fn collect_jsonl_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, out);
        } else if path.file_name().and_then(|n| n.to_str()) == Some("chat_history.jsonl") {
            out.push(path);
        }
    }
}

fn main() {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_sessions_root);
    if !root.is_dir() {
        eprintln!("sessions root not found: {}", root.display());
        std::process::exit(2);
    }

    let mut files = Vec::new();
    collect_jsonl_files(&root, &mut files);
    files.sort();

    let policy = TextRepetitionGuardPolicy::default();
    let started = std::time::Instant::now();
    let mut messages_scanned = 0u64;
    let mut trips = 0u64;

    for file in &files {
        let Ok(handle) = std::fs::File::open(file) else {
            continue;
        };
        for (line_index, row) in BufReader::new(handle).lines().enumerate() {
            let Ok(row) = row else { continue };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&row) else {
                continue;
            };
            if value.get("type").and_then(|v| v.as_str()) != Some("assistant") {
                continue;
            }
            let Some(content) = value.get("content").and_then(|v| v.as_str()) else {
                continue;
            };
            if content.is_empty() {
                continue;
            }
            messages_scanned += 1;
            let mut guard = TextRepetitionGuard::new(policy);
            if let Some((trip, fed_at_trip)) = feed_chunked(&mut guard, content) {
                trips += 1;
                println!(
                    "TRIP\t{}\tline={}\ttrigger={}\tgenerated={}\tat_trip={}\tthreshold={}",
                    file.display(),
                    line_index,
                    trip.trigger.as_str(),
                    trip.generated_chars,
                    fed_at_trip,
                    trip.threshold,
                );
            }
        }
    }

    println!(
        "SUMMARY\tfiles={}\tmessages={}\ttrips={}\telapsed={:?}",
        files.len(),
        messages_scanned,
        trips,
        started.elapsed()
    );
}

fn default_sessions_root() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".grok").join("sessions")
}
