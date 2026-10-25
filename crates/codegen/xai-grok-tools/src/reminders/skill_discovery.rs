//! Skill discovery reminder — discovers new skills near accessed paths.
//!
//! Contains `SkillDiscoveryReminder`, a cross-cutting `Reminder` that fires
//! after every tool call to check for SKILL.md files in `.grok/skills/`,
//! `.agents/skills/`, or `.claude/skills/` directories near the accessed path.
//!
//! The actual tracking logic lives in
//! `types::skill_discovery_tracker::SkillDiscoveryTracker`.

use std::path::{Path, PathBuf};

/// Directories that contain skill definitions (`.grok/skills/`, `.agents/skills/`,
/// `.claude/skills/`, `.cursor/skills/`). Shared between startup skill discovery
/// and runtime `SkillDiscoveryReminder`.
pub const SKILL_CONFIG_DIRS: &[&str] = &[".grok", ".agents", ".claude", ".cursor"];

use crate::implementations::skills::discovery;
use crate::implementations::skills::types::SkillScope;
use crate::types::output::{
    ApplyPatchOutput, ListDirOutput, ReadFileOutput, SearchReplaceOutput, ToolOutput,
};
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::resources::SharedResources;
use crate::types::skill_discovery_tracker::SkillManager;
use crate::types::tool::{Reminder, ToolKind};

/// Fixed one-line absorption-judgment prompt (Phase 6 hard path). Returned by
/// [`SkillDiscoveryReminder`] on the tool call that let new skills enter the
/// listing, so the harness itself nudges the §6 absorption judgment instead of
/// relying on the model to notice the listing increment (Phase 4 soft path).
/// Emission is gated by `SystemRemindersEnabled`: the registry skips
/// `collect_reminders` entirely in concise mode, so this never fires there.
/// The skill names are listed so the model judges only those entries, not the
/// whole listing (reading an already-listed SKILL.md must not look "new").
fn absorption_hint_text(names: &[String]) -> String {
    format!(
        "New skills were just added to the skill listing: {}. When the current task wraps up or the user is idle, run the absorption judgment (skill-curator skill): for each of those skills decide absorb / standalone / noted against the accepted router skills, and record one line in ~/.grok/skill-router-ledger.md.",
        names.join(", ")
    )
}

/// Hint vector for a discovery that entered N skills into the listing.
fn absorption_hint(entered: Vec<crate::implementations::skills::types::SkillInfo>) -> Vec<String> {
    if entered.is_empty() {
        vec![]
    } else {
        let names: Vec<String> = entered.iter().map(|s| s.name.clone()).collect();
        vec![absorption_hint_text(&names)]
    }
}

/// Cross-cutting reminder that discovers skills in subdirectories near filesystem paths accessed by tools. **Concise
/// mode limitation (V1):** This reminder is globally disabled when `SystemRemindersEnabled(false)` is set (concise
/// mode). This means dynamic skill discovery will NOT fire in concise mode. When discovery lets new skills enter the
/// listing, the reminder additionally emits [`ABSORPTION_JUDGMENT_HINT`] (Phase 6 hard path) so the harness itself
/// prompts the absorption judgment.
pub struct SkillDiscoveryReminder;

impl SkillDiscoveryReminder {
    /// Extract the filesystem path the tool accessed from the output. Returns `None` for tools that
    /// don't operate on filesystem paths, or for error variants (no reliable path to extract).
    fn extract_target_path(tool_output: &ToolOutput) -> Option<&Path> {
        match tool_output {
            ToolOutput::ReadFile(ReadFileOutput::FileContent(fc)) => Some(&fc.absolute_path),
            ToolOutput::ListDir(ListDirOutput::Content(content)) => {
                Some(&content.absolute_root_path)
            }
            ToolOutput::SearchReplace(SearchReplaceOutput::EditsApplied(r)) => {
                Some(&r.absolute_path)
            }
            _ => None,
        }
    }

    /// Files the tool touched, for activating `paths:`-gated skills — including
    /// every file of a multi-file `apply_patch`. Bash/grep paths are excluded
    /// (unparseable / incidental).
    fn extract_activation_paths(tool_output: &ToolOutput) -> Vec<PathBuf> {
        match tool_output {
            ToolOutput::ApplyPatch(ApplyPatchOutput::Success { files, .. }) => files
                .iter()
                .flat_map(|f| std::iter::once(f.path.clone()).chain(f.move_to.clone()))
                .collect(),
            other => Self::extract_target_path(other)
                .map(Path::to_path_buf)
                .into_iter()
                .collect(),
        }
    }

    /// Check whether a SKILL.md path is inside a supported skills directory
    /// (`.grok/skills/`, `.agents/skills/`, or `.claude/skills/`).
    fn is_in_supported_skills_dir(path: &Path) -> bool {
        for ancestor in path.ancestors().skip(1) {
            if ancestor.file_name().is_some_and(|n| n == "skills") {
                return ancestor
                    .parent()
                    .and_then(|p| p.file_name())
                    .is_some_and(|n| SKILL_CONFIG_DIRS.iter().any(|d| *d == n));
            }
        }
        false
    }
}

#[async_trait::async_trait]
impl Reminder for SkillDiscoveryReminder {
    fn requires_expr(&self) -> Expr<ToolRequirement> {
        // Finalization-time check: "at least one path-producing tool exists."
        // At runtime, collect_reminders fires after every tool call regardless
        // — output pattern-matching does the actual filtering.
        Expr::Or(vec![
            Expr::Value(ToolRequirement::tool_kind(ToolKind::Read)),
            Expr::Value(ToolRequirement::tool_kind(ToolKind::Edit)),
            Expr::Value(ToolRequirement::tool_kind(ToolKind::List)),
        ])
    }

    async fn collect_reminders(
        &self,
        resources: SharedResources,
        tool_output: &ToolOutput,
    ) -> Vec<String> {
        // 1. Activate `paths:`-gated skills matching any file the tool touched
        //    (includes multi-file `apply_patch` edits).
        let activation_paths = Self::extract_activation_paths(tool_output);
        if !activation_paths.is_empty() {
            let path_refs: Vec<&Path> = activation_paths.iter().map(PathBuf::as_path).collect();
            let mut res = resources.lock().await;
            if let Some(tracker) = res.get_mut::<SkillManager>() {
                tracker.activate_conditional_skills_for_paths(&path_refs);
            }
        }

        // 2. Discovery walks from a single representative path (read/list/edit).
        let Some(target_path) = Self::extract_target_path(tool_output) else {
            return vec![];
        };

        // Direct SKILL.md detection: when a tool writes (or reads) a SKILL.md file, register it immediately. The normal
        // upward-walk discovery cannot find these because it looks for `.grok/skills/` sub-directories in *ancestor* dirs, and
        // user-scope skills (~/.grok/) are outside the git root so the walk breaks early.
        if target_path.file_name().is_some_and(|n| n == "SKILL.md")
            && Self::is_in_supported_skills_dir(target_path)
        {
            let scope = {
                let res = resources.lock().await;
                let tracker = res.get::<SkillManager>();
                let cwd = tracker.and_then(|m| m.cwd.clone());
                let git_root = tracker.and_then(|m| m.git_root.clone());
                match (cwd, git_root) {
                    (Some(cwd), _) if target_path.starts_with(&cwd) => SkillScope::Local,
                    (_, Some(root)) if target_path.starts_with(&root) => SkillScope::Repo,
                    _ => SkillScope::User,
                }
            };
            let skills = discovery::parse_skill_files(vec![(target_path.to_path_buf(), scope)]);
            if !skills.is_empty() {
                let mut res = resources.lock().await;
                if let Some(tracker) = res.get_mut::<SkillManager>() {
                    return absorption_hint(tracker.add_discovered(skills));
                }
            }
            return vec![];
        }

        // 2. Snapshot context under lock, then RELEASE the lock before I/O.
        let (cwd, git_root, mut checked_dirs_snapshot, compat) = {
            let res = resources.lock().await;
            let Some(tracker) = res.get::<SkillManager>() else {
                return vec![];
            };
            let cwd = match tracker.cwd.clone() {
                Some(c) => c,
                None => return vec![],
            };
            (
                cwd,
                tracker.git_root.clone(),
                tracker.checked_dirs.clone(),
                tracker.compat,
            )
        };
        // Lock is released here.

        // 3. Run filesystem discovery OUTSIDE the lock.
        // Calls directly into the discovery module -- no callback indirection.
        let discovered = discovery::discover_skills_for_paths(
            &[target_path],
            &cwd,
            git_root.as_deref(),
            &mut checked_dirs_snapshot,
            compat,
        );

        if discovered.is_empty() {
            // Even if no skills found, merge checked_dirs back so we don't
            // re-stat the same directories on future calls.
            let mut res = resources.lock().await;
            if let Some(tracker) = res.get_mut::<SkillManager>() {
                tracker.checked_dirs.extend(checked_dirs_snapshot);
            }
            return vec![];
        }

        // Re-acquire lock and merge results into tracker. The reminder does NOT produce
        // listing text (the session drains that from the tracker via
        // take_pending_reconciliation()); it only emits the fixed absorption-judgment
        // hint when new skills actually entered the listing.
        let any_new = {
            let mut res = resources.lock().await;
            let tracker = match res.get_mut::<SkillManager>() {
                Some(t) => t,
                None => return vec![],
            };

            // Merge checked_dirs from the snapshot back into the tracker.
            tracker.checked_dirs.extend(checked_dirs_snapshot);

            // Add discovered skills (dedup by canonical path, sets pending flag).
            tracker.add_discovered(discovered)
        };

        absorption_hint(any_new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::output::{ApplyPatchFileResult, FileContent, ReadFileOutput};
    use crate::types::resources::{Resources, SharedResources};

    fn edited(path: &str, move_to: Option<&str>) -> ApplyPatchFileResult {
        ApplyPatchFileResult {
            path: PathBuf::from(path),
            action: "modified".into(),
            old_text: None,
            new_text: String::new(),
            move_to: move_to.map(PathBuf::from),
        }
    }

    #[test]
    fn apply_patch_activation_paths_cover_every_edited_file() {
        let multi_file_patch = ToolOutput::ApplyPatch(ApplyPatchOutput::Success {
            files: vec![edited("/r/a.rs", None), edited("/r/b.rs", Some("/r/c.rs"))],
            tool_output_for_prompt: String::new(),
        });
        assert_eq!(
            SkillDiscoveryReminder::extract_activation_paths(&multi_file_patch),
            vec![
                PathBuf::from("/r/a.rs"),
                PathBuf::from("/r/b.rs"),
                PathBuf::from("/r/c.rs"),
            ],
        );
        let failed_patch = ToolOutput::ApplyPatch(ApplyPatchOutput::ParseError("x".into()));
        assert!(SkillDiscoveryReminder::extract_activation_paths(&failed_patch).is_empty());
    }

    /// Phase 6 hard path: a tool call that registers a new SKILL.md must emit the
    /// absorption-judgment hint exactly once; repeating it must not re-emit.
    #[tokio::test]
    async fn new_skill_discovery_emits_absorption_hint_once() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join(".grok").join("skills").join("newleaf");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        std::fs::write(
            &skill_md,
            "---\nname: newleaf\ndescription: A brand new skill for testing.\n---\n\nbody\n",
        )
        .unwrap();

        let mut mgr = SkillManager::new();
        mgr.cwd = Some(tmp.path().to_path_buf());
        mgr.git_root = None;
        let mut res = Resources::default();
        res.insert(mgr);
        let shared: SharedResources = std::sync::Arc::new(tokio::sync::Mutex::new(res));

        let output = ToolOutput::ReadFile(ReadFileOutput::FileContent(FileContent {
            content: String::new(),
            content_concise: None,
            absolute_path: skill_md,
            offset: None,
            limit: None,
            raw_output: String::new(),
            total_lines: 0,
            extracted_images: Vec::new(),
        }));

        let first = SkillDiscoveryReminder
            .collect_reminders(shared.clone(), &output)
            .await;
        assert_eq!(
            first.len(),
            1,
            "first discovery must emit exactly the hint: {first:?}"
        );
        assert!(
            first[0].contains("skill-curator")
                && first[0].contains("skill-router-ledger")
                && first[0].contains("newleaf"),
            "hint must name the new skill and point at the absorption protocol: {}",
            first[0]
        );

        let second = SkillDiscoveryReminder
            .collect_reminders(shared.clone(), &output)
            .await;
        assert!(
            second.is_empty(),
            "already-registered skill must not re-emit the hint: {second:?}"
        );
    }

    /// Reading a SKILL.md that is already in the startup listing must not emit
    /// the absorption hint (Bettbox-Mod 14:54 false positive: stage-recap write
    /// re-opened existing skills and launched skill-curator).
    #[tokio::test]
    async fn startup_listed_skill_md_read_emits_no_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join(".grok").join("skills").join("stage-recap");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        std::fs::write(
            &skill_md,
            "---\nname: stage-recap\ndescription: Already in the startup listing.\n---\n\nbody\n",
        )
        .unwrap();

        let mut mgr = SkillManager::new();
        mgr.cwd = Some(tmp.path().to_path_buf());
        mgr.git_root = None;
        mgr.seed(
            Some(tmp.path().to_path_buf()),
            None,
            vec![crate::implementations::skills::types::SkillInfo {
                name: "stage-recap".to_string(),
                description: "Already in the startup listing.".to_string(),
                path: skill_md.to_string_lossy().to_string(),
                ..crate::implementations::skills::types::SkillInfo::default()
            }],
            None,
            None,
            None,
        );
        let mut res = Resources::default();
        res.insert(mgr);
        let shared: SharedResources = std::sync::Arc::new(tokio::sync::Mutex::new(res));

        let output = ToolOutput::ReadFile(ReadFileOutput::FileContent(FileContent {
            content: String::new(),
            content_concise: None,
            absolute_path: skill_md,
            offset: None,
            limit: None,
            raw_output: String::new(),
            total_lines: 0,
            extracted_images: Vec::new(),
        }));

        let hints = SkillDiscoveryReminder
            .collect_reminders(shared, &output)
            .await;
        assert!(
            hints.is_empty(),
            "startup-listed SKILL.md must not fire absorption hint: {hints:?}"
        );
    }

    /// No new skills → no hint (walk path with no SKILL.md anywhere nearby).
    #[tokio::test]
    async fn no_discovery_emits_no_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("plain.txt");
        std::fs::write(&file, "hi").unwrap();

        let mut mgr = SkillManager::new();
        mgr.cwd = Some(tmp.path().to_path_buf());
        mgr.git_root = None;
        let mut res = Resources::default();
        res.insert(mgr);
        let shared: SharedResources = std::sync::Arc::new(tokio::sync::Mutex::new(res));

        let output = ToolOutput::ReadFile(ReadFileOutput::FileContent(FileContent {
            content: String::new(),
            content_concise: None,
            absolute_path: file,
            offset: None,
            limit: None,
            raw_output: String::new(),
            total_lines: 0,
            extracted_images: Vec::new(),
        }));

        let hints = SkillDiscoveryReminder
            .collect_reminders(shared, &output)
            .await;
        assert!(hints.is_empty(), "expected no hint, got: {hints:?}");
    }
}
