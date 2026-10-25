//! Test-caller heuristic shared with `search_code` grouping.

/// Test code by path or by caller name (Go `TestX`, Python `test_x`, JS specs).
pub(crate) fn is_test_caller(path: &str, caller: &str) -> bool {
    let p = path.to_ascii_lowercase();
    let file = p.rsplit('/').next().unwrap_or(&p);
    file.contains("_test.")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.starts_with("test_")
        || p.starts_with("test/")
        || p.starts_with("tests/")
        || p.contains("/test/")
        || p.contains("/tests/")
        || p.contains("__tests__/")
        || caller.starts_with("Test")
        || caller.starts_with("test_")
        || caller.starts_with("Benchmark")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paths_and_names() {
        assert!(is_test_caller("service/channel_test.go", "TestX"));
        assert!(is_test_caller("a/b.go", "TestShouldDisable"));
        assert!(is_test_caller("tests/vectors.rs", "frame"));
        assert!(is_test_caller("src/__tests__/a.ts", "x"));
        assert!(!is_test_caller("controller/relay.go", "Relay"));
        assert!(!is_test_caller("contest/latest.go", "Run"));
    }
}
