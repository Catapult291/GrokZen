//! Unconditional refusal for commands that destroy the machine.
//!
//! `always-approve` sessions never prompt, so this is the only layer left for
//! targets no session should reach: the filesystem root, system directories,
//! drive roots, raw disks, and the format/shutdown verbs. Detection is lexical
//! (tree-sitter plus a quote-aware tokenizer) and never touches the filesystem,
//! so Windows spellings are recognized on a POSIX host too.

use tree_sitter::Tree;

use crate::bash_command_splitting::{
    try_parse_shell, try_parse_word_only_commands_sequence, unwrap_wrappers,
};
use crate::policy::{InlineShellScript, ShellWord, shell_dash_c_script};
use crate::shell_access::command_write_paths_in_tree;

/// How many `shell -c` / `cmd /c` / `pwsh -Command` layers may be unwrapped.
/// Deeper nesting still gets the direct checks at the boundary layer.
pub const MAX_INLINE_DEPTH: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardlineHit {
    pub code: &'static str,
    pub reason: &'static str,
}

const POSIX_ROOT_DELETE: HardlineHit = HardlineHit {
    code: "posix_root_delete",
    reason: "deleting the filesystem root",
};
const POSIX_SYSTEM_DIR_DELETE: HardlineHit = HardlineHit {
    code: "posix_system_dir_delete",
    reason: "deleting a POSIX system directory",
};
const HOME_DIR_DELETE: HardlineHit = HardlineHit {
    code: "home_dir_delete",
    reason: "deleting the user home root",
};
const WINDOWS_DRIVE_ROOT: HardlineHit = HardlineHit {
    code: "windows_drive_root",
    reason: "deleting a drive root",
};
const WINDOWS_SYSTEM_DIR_DELETE: HardlineHit = HardlineHit {
    code: "windows_system_dir_delete",
    reason: "deleting a Windows system directory",
};
const WINDOWS_USER_HOME_DELETE: HardlineHit = HardlineHit {
    code: "windows_user_home_delete",
    reason: "deleting a Windows user profile root",
};
const MKFS: HardlineHit = HardlineHit {
    code: "mkfs",
    reason: "formatting a filesystem",
};
const RAW_DEVICE_WRITE: HardlineHit = HardlineHit {
    code: "raw_device_write",
    reason: "writing to a raw disk device",
};
const FORK_BOMB: HardlineHit = HardlineHit {
    code: "fork_bomb",
    reason: "fork bomb",
};
const KILL_ALL: HardlineHit = HardlineHit {
    code: "kill_all",
    reason: "signalling every process",
};
const SYSTEM_POWER: HardlineHit = HardlineHit {
    code: "system_power",
    reason: "shutting down or rebooting the machine",
};
const DISK_MANAGEMENT: HardlineHit = HardlineHit {
    code: "disk_management",
    reason: "formatting or wiping a disk",
};
const SHADOW_COPY_DELETE: HardlineHit = HardlineHit {
    code: "shadow_copy_delete",
    reason: "deleting volume shadow copies or backups",
};
const BOOT_CONFIG_WRITE: HardlineHit = HardlineHit {
    code: "boot_config_write",
    reason: "changing boot configuration",
};
const CIPHER_WIPE: HardlineHit = HardlineHit {
    code: "cipher_wipe",
    reason: "wiping free space",
};

const POSIX_TOP_DIRS: &[&str] = &[
    "home", "root", "etc", "usr", "var", "boot", "bin", "sbin", "lib",
];

const WINDOWS_SYSTEM_DIRS: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
];

const SUDO_VALUE_OPTIONS: &[&str] = &[
    "-u",
    "-g",
    "-p",
    "-C",
    "-T",
    "-U",
    "-r",
    "-t",
    "-R",
    "-D",
    "--user",
    "--group",
    "--prompt",
    "--close-from",
    "--command-timeout",
    "--other-user",
    "--role",
    "--type",
    "--chroot",
    "--chdir",
    "--host",
];

/// Whether `command` hits a hardline rule. Returns the first hit; the reason is
/// meant for the model-facing rejection message and never echoes the command.
pub fn detect_hardline(command: &str) -> Option<HardlineHit> {
    detect_text(command, 0)
}

fn detect_text(text: &str, depth: u8) -> Option<HardlineHit> {
    let folded = fold_ifs(text);
    let text = folded.as_str();
    if text.trim().is_empty() {
        return None;
    }
    if is_fork_bomb(text) {
        return Some(FORK_BOMB);
    }
    if has_diskpart_clean(text) {
        return Some(DISK_MANAGEMENT);
    }

    if let Some(tree) = try_parse_shell(text) {
        if let Some(hit) = raw_device_redirect_hit(&tree, text) {
            return Some(hit);
        }
        if let Some(commands) = try_parse_word_only_commands_sequence(&tree, text) {
            for command in &commands {
                if let Some(hit) = check_tokens(command.words(), depth) {
                    return Some(hit);
                }
            }
        }
    }

    // The bash grammar reads `C:\ -Recurse` as one escaped-word token, and it
    // rejects expansions outright. Run the quote-aware tokenizer as well; it
    // keeps quoted arguments inside their word, so `echo "rm -rf /"` stays inert.
    for windows_backslash in [false, true] {
        for segment in tokenize(text, windows_backslash) {
            if let Some(hit) = check_tokens(&segment, depth) {
                return Some(hit);
            }
        }
    }
    None
}

fn fold_ifs(text: &str) -> String {
    text.replace("${IFS}", " ").replace("$IFS", " ")
}

fn is_fork_bomb(text: &str) -> bool {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    compact.starts_with(':') && compact.contains(":(){:|:&};:")
}

fn has_diskpart_clean(text: &str) -> bool {
    let segments = tokenize(text, true);
    let saw_clean = segments
        .iter()
        .flatten()
        .any(|token| token.eq_ignore_ascii_case("clean"));
    saw_clean
        && segments
            .iter()
            .any(|segment| head_name(peel_prefixes(segment)).is_some_and(|head| head == "diskpart"))
}

fn raw_device_redirect_hit(tree: &Tree, src: &str) -> Option<HardlineHit> {
    let hit = command_write_paths_in_tree(tree.root_node(), src)
        .iter()
        .any(|path| is_raw_device(path));
    hit.then_some(RAW_DEVICE_WRITE)
}

fn check_tokens(tokens: &[String], depth: u8) -> Option<HardlineHit> {
    let tokens = peel_prefixes(tokens);
    let head = head_name(tokens)?;
    let rest = tokens.get(1..).unwrap_or_default();
    match head.as_str() {
        "rm" | "ri" | "del" | "erase" | "rd" | "rmdir" | "remove-item" => {
            check_delete_targets(rest)
        }
        "mkfs" => Some(MKFS),
        head if head.starts_with("mkfs.") => Some(MKFS),
        "dd" => check_dd(rest),
        "kill" => check_kill(rest),
        "shutdown" | "reboot" | "halt" | "poweroff" => Some(SYSTEM_POWER),
        "init" | "telinit" => check_runlevel(rest),
        "systemctl" => check_systemctl(rest),
        "format" => check_format(rest),
        "format-volume" | "clear-disk" => Some(DISK_MANAGEMENT),
        "vssadmin" => check_vssadmin(rest),
        "wbadmin" => check_wbadmin(rest),
        "bcdedit" => check_bcdedit(rest),
        "cipher" => check_cipher(rest),
        "bash" | "sh" | "dash" | "zsh" | "ksh" => check_bash_c(tokens, depth),
        "cmd" => check_cmd(rest, depth),
        "powershell" | "pwsh" => check_pwsh(rest, depth),
        _ => None,
    }
}

fn check_delete_targets(args: &[String]) -> Option<HardlineHit> {
    let mut after_double_dash = false;
    let mut index = 0usize;
    while let Some(arg) = args.get(index) {
        if !after_double_dash {
            if arg == "--" {
                after_double_dash = true;
                index += 1;
                continue;
            }
            if is_delete_flag(arg) {
                index += 1;
                continue;
            }
        }

        // The fallback tokenizer has already dropped quotes, so a quoted path
        // with spaces arrives as several tokens; try every prefix of the run.
        let mut joined = arg.clone();
        loop {
            if let Some(hit) = target_hit(&joined) {
                return Some(hit);
            }
            index += 1;
            let Some(next) = args.get(index) else {
                break;
            };
            if !after_double_dash && is_delete_flag(next) {
                break;
            }
            joined.push(' ');
            joined.push_str(next);
        }
    }
    None
}

fn is_delete_flag(arg: &str) -> bool {
    if arg.len() > 1 && arg.starts_with('-') {
        return true;
    }
    let lower = arg.to_ascii_lowercase();
    matches!(lower.as_str(), "/p" | "/f" | "/s" | "/q" | "/a") || lower.starts_with("/a:")
}

fn target_hit(raw: &str) -> Option<HardlineHit> {
    if raw.trim().is_empty() {
        return None;
    }
    let decoded = decode_word(raw);
    for candidate in [raw, decoded.as_str()] {
        if let Some(hit) = classify_target(candidate) {
            return Some(hit);
        }
    }
    None
}

fn classify_target(raw: &str) -> Option<HardlineHit> {
    let trimmed = trim_quotes(raw.trim());
    if trimmed.is_empty() {
        return None;
    }
    let folded = fold_root_variables(trimmed);
    let normalized = normalize_path(&folded)?;
    classify_normalized(&normalized)
}

fn fold_root_variables(raw: &str) -> String {
    let mut out = raw.to_ascii_lowercase();
    for (needle, replacement) in [
        ("${home}", "~"),
        ("$home", "~"),
        ("$env:userprofile", "~"),
        ("%userprofile%", "~"),
        ("$env:systemdrive", "c:"),
        ("%systemdrive%", "c:"),
        ("$env:systemroot", "c:/windows"),
        ("$env:windir", "c:/windows"),
        ("%systemroot%", "c:/windows"),
        ("%windir%", "c:/windows"),
    ] {
        out = out.replace(needle, replacement);
    }
    out
}

fn normalize_path(raw: &str) -> Option<String> {
    let mut path = raw.replace('\\', "/");
    loop {
        let stripped = ["//?/", "//./", "/??/"]
            .iter()
            .find_map(|prefix| path.strip_prefix(prefix));
        match stripped {
            Some(rest) => path = rest.to_owned(),
            None => break,
        }
    }

    if path == "~" {
        return Some("~".to_owned());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        let parts = components(rest);
        return Some(if parts.is_empty() {
            "~".to_owned()
        } else {
            format!("~/{}", parts.join("/"))
        });
    }

    if let Some(drive) = drive_prefix(&path) {
        let rest = path.get(drive.len()..).unwrap_or_default();
        let parts = components(rest);
        let mut out = drive.to_owned();
        if !parts.is_empty() {
            out.push('/');
            out.push_str(&parts.join("/"));
        }
        return Some(out);
    }

    if let Some(rest) = path.strip_prefix('/') {
        let parts = components(rest);
        let out = if parts.is_empty() {
            "/".to_owned()
        } else {
            format!("/{}", parts.join("/"))
        };
        return Some(mount_as_drive(&out).unwrap_or(out));
    }

    let parts = components(&path);
    Some(parts.join("/"))
}

/// A POSIX spelling of a Windows drive (`/c/…`) means `C:\…` under Git Bash and
/// niubash. Only a single-letter first component is rewritten.
fn mount_as_drive(path: &str) -> Option<String> {
    let rest = path.strip_prefix('/')?;
    let (first, tail) = match rest.split_once('/') {
        Some((first, tail)) => (first, Some(tail)),
        None => (rest, None),
    };
    let letter = first.chars().next()?;
    if first.len() != 1 || !letter.is_ascii_alphabetic() {
        return None;
    }
    Some(match tail {
        Some(tail) => format!("{}:/{}", letter.to_ascii_lowercase(), tail),
        None => format!("{}:", letter.to_ascii_lowercase()),
    })
}

fn classify_normalized(path: &str) -> Option<HardlineHit> {
    if path == "/" || path == "/*" {
        return Some(POSIX_ROOT_DELETE);
    }
    if path == "~" || path == "~/*" {
        return Some(HOME_DIR_DELETE);
    }
    if is_drive_root(path) {
        return Some(WINDOWS_DRIVE_ROOT);
    }
    if is_posix_top_dir(path) {
        return Some(POSIX_SYSTEM_DIR_DELETE);
    }
    if let Some(rest) = windows_rest(path) {
        if is_windows_system_dir(rest) {
            return Some(WINDOWS_SYSTEM_DIR_DELETE);
        }
        if is_windows_user_root(rest) {
            return Some(WINDOWS_USER_HOME_DELETE);
        }
    }
    None
}

fn is_drive_root(path: &str) -> bool {
    if let Some(rest) = path.strip_suffix(':') {
        return rest.len() == 1 && rest.as_bytes().first().is_some_and(u8::is_ascii_alphabetic);
    }
    if let Some(rest) = path.strip_suffix(":/*") {
        return rest.len() == 1 && rest.as_bytes().first().is_some_and(u8::is_ascii_alphabetic);
    }
    false
}

fn is_posix_top_dir(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    let mut parts = rest.split('/');
    let Some(first) = parts.next() else {
        return false;
    };
    if !POSIX_TOP_DIRS.contains(&first) {
        return false;
    }
    match parts.next() {
        None => true,
        Some("*") => parts.next().is_none(),
        Some(_) => false,
    }
}

fn is_windows_system_dir(rest: &str) -> bool {
    WINDOWS_SYSTEM_DIRS.iter().any(|dir| {
        rest == *dir
            || rest
                .strip_prefix(dir)
                .is_some_and(|tail| tail.starts_with('/'))
    })
}

fn is_windows_user_root(rest: &str) -> bool {
    let Some(users) = rest.strip_prefix("users") else {
        return false;
    };
    if let Some(rest) = users.strip_prefix('/') {
        let mut parts = rest.split('/');
        match parts.next() {
            None => true,
            Some("*") => parts.next().is_none(),
            Some(name) => !name.is_empty() && parts.next().is_none(),
        }
    } else {
        // `C:\Users` — everything under it is a profile root.
        users.is_empty()
    }
}

fn windows_rest(path: &str) -> Option<&str> {
    let drive = drive_prefix(path)?;
    path.get(drive.len()..)?.strip_prefix('/')
}

fn drive_prefix(path: &str) -> Option<&str> {
    let bytes = path.as_bytes();
    let first = bytes.first()?;
    if !first.is_ascii_alphabetic() || bytes.get(1) != Some(&b':') {
        return None;
    }
    path.get(..2)
}

fn components(path: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn check_dd(args: &[String]) -> Option<HardlineHit> {
    args.iter()
        .any(|arg| {
            let lower = arg.to_ascii_lowercase();
            lower.strip_prefix("of=").is_some_and(is_raw_device)
        })
        .then_some(RAW_DEVICE_WRITE)
}

fn is_raw_device(raw: &str) -> bool {
    let path = trim_quotes(raw.trim())
        .replace('\\', "/")
        .to_ascii_lowercase();
    let Some(rest) = path.strip_prefix("/dev/") else {
        return false;
    };
    ["sd", "nvme", "hd", "mmcblk", "vd", "xvd"]
        .iter()
        .any(|prefix| rest.starts_with(prefix))
}

fn check_kill(args: &[String]) -> Option<HardlineHit> {
    let args: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| *arg != "--")
        .collect();
    if args.len() == 1 && args.first() == Some(&"-1") {
        return Some(KILL_ALL);
    }
    if args.contains(&"-9") && args.contains(&"-1") {
        return Some(KILL_ALL);
    }
    None
}

fn check_runlevel(args: &[String]) -> Option<HardlineHit> {
    let runlevel = args.iter().find(|arg| !arg.starts_with('-'))?;
    matches!(runlevel.as_str(), "0" | "6").then_some(SYSTEM_POWER)
}

fn check_systemctl(args: &[String]) -> Option<HardlineHit> {
    let hit = args
        .iter()
        .any(|arg| matches!(arg.as_str(), "poweroff" | "reboot" | "halt" | "kexec"));
    hit.then_some(SYSTEM_POWER)
}

fn check_format(args: &[String]) -> Option<HardlineHit> {
    args.iter()
        .any(|arg| is_drive_spec(arg))
        .then_some(DISK_MANAGEMENT)
}

fn is_drive_spec(arg: &str) -> bool {
    let lower = trim_quotes(arg.trim()).to_ascii_lowercase();
    let bytes = lower.as_bytes();
    bytes.len() == 2
        && bytes.first().is_some_and(|byte| byte.is_ascii_alphabetic())
        && bytes.get(1) == Some(&b':')
}

fn check_vssadmin(args: &[String]) -> Option<HardlineHit> {
    let joined = args.join(" ").to_ascii_lowercase();
    (joined.contains("delete") && joined.contains("shadows")).then_some(SHADOW_COPY_DELETE)
}

fn check_wbadmin(args: &[String]) -> Option<HardlineHit> {
    args.iter()
        .any(|arg| arg.eq_ignore_ascii_case("delete"))
        .then_some(SHADOW_COPY_DELETE)
}

fn check_bcdedit(args: &[String]) -> Option<HardlineHit> {
    args.iter()
        .any(|arg| arg.starts_with("/set") || arg.starts_with("-set"))
        .then_some(BOOT_CONFIG_WRITE)
}

fn check_cipher(args: &[String]) -> Option<HardlineHit> {
    args.iter()
        .any(|arg| arg.to_ascii_lowercase().starts_with("/w"))
        .then_some(CIPHER_WIPE)
}

fn check_bash_c(tokens: &[String], depth: u8) -> Option<HardlineHit> {
    let words: Vec<ShellWord<'_>> = tokens
        .iter()
        .map(|word| ShellWord::Literal(word.as_str()))
        .collect();
    match shell_dash_c_script(&words) {
        InlineShellScript::Literal(index) => {
            let script = tokens.get(index)?;
            recurse_inline(std::slice::from_ref(script), depth)
        }
        InlineShellScript::Untrusted | InlineShellScript::Unrecognized => None,
        InlineShellScript::NotInline => None,
    }
}

fn check_cmd(rest: &[String], depth: u8) -> Option<HardlineHit> {
    for (index, arg) in rest.iter().enumerate() {
        if arg.eq_ignore_ascii_case("/c") || arg.eq_ignore_ascii_case("/k") {
            return recurse_inline(rest.get(index + 1..).unwrap_or_default(), depth);
        }
    }
    None
}

fn check_pwsh(rest: &[String], depth: u8) -> Option<HardlineHit> {
    for (index, arg) in rest.iter().enumerate() {
        let lower = arg.to_ascii_lowercase();
        if lower == "-command" || lower == "-c" || lower == "--command" {
            return recurse_inline(rest.get(index + 1..).unwrap_or_default(), depth);
        }
        // `-EncodedCommand` is unauditable; it stays a documented residual risk.
        if lower.starts_with("-enc") || lower == "-file" || lower == "-f" {
            return None;
        }
    }
    None
}

fn recurse_inline(rest: &[String], depth: u8) -> Option<HardlineHit> {
    if rest.is_empty() || depth >= MAX_INLINE_DEPTH {
        return None;
    }
    if rest.len() == 1 {
        detect_text(rest.first()?, depth + 1)
    } else {
        check_tokens(rest, depth + 1)
    }
}

fn peel_prefixes(mut tokens: &[String]) -> &[String] {
    for _ in 0..8 {
        tokens = unwrap_wrappers(tokens);
        let Some(head) = head_name(tokens) else {
            return tokens;
        };
        let rest = tokens.get(1..).unwrap_or_default();
        let next = match head.as_str() {
            "sudo" | "doas" => peel_with_options(rest, SUDO_VALUE_OPTIONS, true),
            "exec" => peel_with_options(rest, &["-a"], true),
            "command" => {
                if rest.iter().any(|arg| arg == "-v" || arg == "-V") {
                    return &[];
                }
                peel_with_options(rest, &[], false)
            }
            "builtin" | "nohup" | "setsid" | "time" => peel_with_options(rest, &[], false),
            "taskset" => peel_with_options(rest, &["-c", "--cpu-list", "-p", "--pid"], true),
            "chroot" => peel_chroot(rest),
            _ => return tokens,
        };
        match next {
            Some(next) if !next.is_empty() => tokens = next,
            _ => return &[],
        }
    }
    tokens
}

fn peel_with_options<'a>(
    args: &'a [String],
    value_options: &[&str],
    skip_assignments: bool,
) -> Option<&'a [String]> {
    let mut index = 0usize;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            return args.get(index + 1..).filter(|rest| !rest.is_empty());
        }
        if arg.len() > 1 && arg.starts_with('-') {
            let name = arg.split('=').next().unwrap_or(arg);
            let consumes_next = value_options.contains(&name) && !arg.contains('=');
            index += if consumes_next { 2 } else { 1 };
            continue;
        }
        if skip_assignments && is_assignment(arg) {
            index += 1;
            continue;
        }
        return args.get(index..).filter(|rest| !rest.is_empty());
    }
    None
}

fn peel_chroot(args: &[String]) -> Option<&[String]> {
    let rest = peel_with_options(args, &[], false)?;
    rest.get(1..).filter(|inner| !inner.is_empty())
}

fn is_assignment(arg: &str) -> bool {
    let Some(eq) = arg.find('=') else {
        return false;
    };
    let name = arg.get(..eq).unwrap_or_default();
    !name.is_empty()
        && name
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn head_name(tokens: &[String]) -> Option<String> {
    let head = tokens.first()?;
    let raw_base = head.rsplit(['/', '\\']).next().unwrap_or(head);
    let decoded = decode_word(head);
    let decoded_base = decoded.rsplit('/').next().unwrap_or(decoded.as_str());
    // `r\m` is an escaped `rm`, while `C:\Windows\...` uses backslashes as
    // separators. Only the path shapes take the backslash-split basename.
    let looks_like_windows_path = head.contains(':')
        || head.matches('\\').count() > 1
        || head.starts_with("\\\\")
        || head.starts_with(".\\")
        || head.starts_with("..\\");
    let base = if looks_like_windows_path {
        raw_base
    } else {
        decoded_base
    };
    let lower = base.to_ascii_lowercase();
    let name = lower.strip_suffix(".exe").unwrap_or(lower.as_str());
    Some(name.to_owned())
}

fn decode_word(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut chars = word.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '\'' | '"' => {}
            _ => out.push(ch),
        }
    }
    out
}

fn trim_quotes(s: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = s
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    s
}

#[derive(Clone, Copy, PartialEq)]
enum Quote {
    None,
    Single,
    Double,
}

/// Split a command into segments at unquoted `;`, `|`, `&`, and newlines, then
/// into words, honoring shell quoting. Two backslash modes let the same input
/// be read as POSIX (`r\m` → `rm`) and as Windows (`C:\Windows` kept intact).
fn tokenize(text: &str, windows_backslash: bool) -> Vec<Vec<String>> {
    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote = Quote::None;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        match quote {
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    word.push(ch);
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '\\' if !windows_backslash => match chars.next() {
                    Some(next @ ('$' | '`' | '"' | '\\')) => word.push(next),
                    Some(next) => {
                        word.push('\\');
                        word.push(next);
                    }
                    None => word.push('\\'),
                },
                '\\' if windows_backslash => match chars.peek() {
                    Some('"') => {
                        word.push(ch);
                    }
                    _ => word.push(ch),
                },
                _ => word.push(ch),
            },
            Quote::None => match ch {
                '\'' => {
                    quote = Quote::Single;
                    in_word = true;
                }
                '"' => {
                    quote = Quote::Double;
                    in_word = true;
                }
                '\\' if !windows_backslash => match chars.next() {
                    Some('\n') => {}
                    Some(next) => {
                        word.push(next);
                        in_word = true;
                    }
                    None => {
                        word.push('\\');
                        in_word = true;
                    }
                },
                '\\' if windows_backslash => {
                    word.push(ch);
                    in_word = true;
                }
                ';' | '|' | '&' | '\n' => {
                    end_word(&mut word, &mut in_word, &mut current);
                    end_segment(&mut current, &mut segments);
                }
                ch if ch.is_whitespace() => {
                    end_word(&mut word, &mut in_word, &mut current);
                }
                _ => {
                    word.push(ch);
                    in_word = true;
                }
            },
        }
    }
    end_word(&mut word, &mut in_word, &mut current);
    end_segment(&mut current, &mut segments);
    segments
}

fn end_word(word: &mut String, in_word: &mut bool, current: &mut Vec<String>) {
    if *in_word {
        current.push(std::mem::take(word));
        *in_word = false;
    }
}

fn end_segment(current: &mut Vec<String>, segments: &mut Vec<Vec<String>>) {
    if !current.is_empty() {
        segments.push(std::mem::take(current));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn assert_hit(command: &str) {
        assert!(
            detect_hardline(command).is_some(),
            "expected hardline for `{command}`"
        );
    }

    #[track_caller]
    fn assert_miss(command: &str) {
        let hit = detect_hardline(command);
        assert!(
            hit.is_none(),
            "unexpected hardline for `{command}`: {hit:?}"
        );
    }

    #[test]
    fn posix_root_and_system_dirs_hit() {
        for command in [
            "rm -rf /",
            "rm -rf \"/\"",
            "rm -rf //",
            "rm -rf /./",
            "rm -rf /../..",
            "rm -rf / *",
            "rm -rf /*",
            "rm -rf /home",
            "rm -rf /home/",
            "rm -rf /home/*",
            "rm -rf /root",
            "rm -rf /etc",
            "rm -rf /usr",
            "rm -rf /var",
            "rm -rf /boot",
            "rm -rf /bin",
            "rm -rf /sbin",
            "rm -rf /lib",
            "sudo rm -rf /",
            "env FOO=1 rm -rf /",
            "timeout 30 rm -rf /",
        ] {
            assert_hit(command);
        }
    }

    #[test]
    fn home_variants_hit() {
        for command in ["rm -rf ~", "rm -rf $HOME", "rm -rf ${HOME}", "rm -rf ~/*"] {
            assert_hit(command);
        }
    }

    #[test]
    fn posix_other_destructive_verbs_hit() {
        for command in [
            "mkfs.ext4 /dev/sda",
            "mkfs /dev/sdb1",
            "dd if=/dev/zero of=/dev/sda",
            "dd of=/dev/nvme0n1 if=/dev/zero",
            "> /dev/sda",
            ":(){ :|:& };:",
            "kill -1",
            "kill -9 -1",
            "shutdown -h now",
            "reboot",
            "halt",
            "poweroff",
            "init 0",
            "init 6",
            "telinit 0",
            "systemctl reboot",
            "systemctl poweroff --no-wall",
        ] {
            assert_hit(command);
        }
    }

    #[test]
    fn windows_delete_targets_hit() {
        for command in [
            "rm -rf C:\\",
            "rm -rf C:/",
            "rm -rf /c/",
            "Remove-Item C:\\ -Recurse -Force",
            "Remove-Item -Path C:\\ -Recurse -Force",
            "rd /s /q C:\\",
            "rmdir /s /q C:\\",
            "del /f /s /q C:\\Windows",
            "erase /f /s /q C:\\Program Files",
            "cmd /c rd /s /q C:\\",
            "powershell -NoProfile -Command \"Remove-Item C:\\ -Recurse -Force\"",
            "pwsh -Command \"Remove-Item C:\\ -Recurse -Force\"",
            "rm -rf C:\\Users\\me",
            "rm -rf /c/Users/me",
            "rm -rf C:\\ProgramData",
            "rm -rf C:\\Program Files (x86)",
            "rm -rf C:\\Windows\\System32",
            "rm -rf C:/Windows/SysWOW64/drivers",
            "Remove-Item C:\\Windows\\System32 -Recurse -Force",
            "del /f /s /q C:\\Windows\\Temp",
            "rm -rf C:\\Program Files\\Some App",
            "rm -rf C:\\ProgramData\\Vendor",
            "rm -rf \\\\?\\C:\\",
            "rm -rf \\\\.\\C:",
        ] {
            assert_hit(command);
        }
    }

    #[test]
    fn windows_disk_verbs_hit() {
        for command in [
            "format C:",
            "format c: /q",
            "Format-Volume -DriveLetter C",
            "Clear-Disk -Number 0",
            "vssadmin delete shadows /all /quiet",
            "wbadmin delete backup",
            "bcdedit /set {default} safeboot minimal",
            "cipher /w:C:\\",
        ] {
            assert_hit(command);
        }
    }

    #[test]
    fn diskpart_clean_hit() {
        assert_hit("diskpart\nclean\n");
        assert_hit("echo clean | diskpart");
    }

    #[test]
    fn quoted_mentions_stay_inert() {
        for command in [
            "echo \"rm -rf /\"",
            "git commit -m \"Remove-Item C:\\\"",
            "echo ':(){ :|:& };:'",
            "echo diskpart clean",
        ] {
            assert_miss(command);
        }
    }

    #[test]
    fn ordinary_cleanup_stays_allowed() {
        for command in [
            "rm -rf ./build",
            "rm -rf build target",
            "Remove-Item .\\build -Recurse -Force",
            "rm -rf /home/user/project",
            "rm -rf /usr/local/share/x",
            "git clean -fdx",
            "git reset --hard",
            "rm -rf $TARGET",
            "rm -rf C:\\Users\\me\\Documents",
            "rm -rf C:\\WindowsOld",
            "rm -rf C:\\Windows.old",
            "rm -rf C:\\ProgramData2",
        ] {
            assert_miss(command);
        }
    }

    #[test]
    fn concatenation_and_escapes_hit() {
        for command in ["r\\m -rf /", "rm -rf \"/\"", "rm -rf $IFS/", "rm -rf ''/''"] {
            assert_hit(command);
        }
    }

    #[test]
    fn known_home_variables_hit_unknown_do_not() {
        assert_hit("rm -rf $HOME");
        assert_hit("rm -rf $env:USERPROFILE");
        assert_hit("rm -rf %USERPROFILE%");
        assert_hit("rm -rf %SystemDrive%");
        assert_miss("rm -rf $TARGET");
    }

    #[test]
    fn inline_shell_recursion_is_bounded() {
        assert_hit("bash -c 'rm -rf /'");
        assert_hit("sh -lc 'rm -rf /'");
        assert_hit("cmd /c \"rd /s /q C:\\\"");
        assert_hit("pwsh -NoProfile -Command \"Remove-Item C:\\ -Recurse -Force\"");
        // Three layers deep: the innermost command is no longer unwrapped.
        assert_miss("cmd /c cmd /c cmd /c rd /s /q C:\\");
    }
}
