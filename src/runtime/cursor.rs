//! Where Cursor Agent makes a worktree (`cursor-agent --worktree <name>`):
//! `~/.cursor/worktrees/<project directory name>/<name>`, on a new branch
//! `<name>` from HEAD, as `cursor-agent --help` documents; the directory name
//! lowercased, as the 0.4.14 trials observed (`trial-…T170432Z-…` became
//! `trial-…t170432z-…`). Observed with
//! Cursor Agent 2026.09.28 (`docs/plan-0.4.14.md` §3 C): it leaves no marker
//! in the worktree's Git metadata and no lock, so the place and the branch
//! together are the only sign, and the end of its session is not observable.
//! Nothing here lists or walks `~/.cursor`: it tests one path Git gave.

use std::path::Path;

/// The worktree's name, which is its session as far as Dispatch can tell, if
/// Cursor Agent made the worktree at `worktree` (canonical, as Git listed it,
/// on `branch`) for the project at `root`, with the user's home at `home`.
pub fn session(
    root: &Path,
    worktree: &Path,
    branch: Option<&str>,
    home: Option<&Path>,
) -> Option<String> {
    let name = worktree.file_name()?.to_str()?;
    let project = root.file_name()?.to_str()?.to_lowercase();
    // Only Cursor's own directory is resolved; the rest is compared as
    // written, so a case-insensitive filesystem matches no more than another.
    let worktrees = std::fs::canonicalize(home?.join(".cursor").join("worktrees")).ok()?;
    (worktree == worktrees.join(project).join(name)
        && branch == Some(&format!("refs/heads/{name}")))
    .then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_its_documented_place_on_its_own_branch_is_a_cursor_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let root = temp.path().join("code/repo");
        let made = home.join(".cursor/worktrees/repo/p0-cursor");
        let other = home.join(".cursor/elsewhere/repo/p0-cursor");
        for dir in [&root, &made, &other] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let made = std::fs::canonicalize(made).unwrap();
        let other = std::fs::canonicalize(other).unwrap();
        let home = Some(home.as_path());
        let branch = Some("refs/heads/p0-cursor");
        assert_eq!(
            session(&root, &made, branch, home).as_deref(),
            Some("p0-cursor")
        );
        assert_eq!(session(&root, &made, Some("refs/heads/main"), home), None);
        assert_eq!(session(&root, &made, None, home), None);
        assert_eq!(session(&root, &other, branch, home), None);
        assert_eq!(
            session(&temp.path().join("other"), &made, branch, home),
            None
        );
        assert_eq!(session(&root, &made, branch, None), None);
    }

    /// Found in the 0.4.14 trials: Cursor lowercases the project's directory
    /// name, so a project named with capitals has its worktrees under the
    /// lowercased name, and never under the name as written.
    #[test]
    fn the_project_directory_name_is_lowercased() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("code/Trial-Cursor-20261014T170432Z");
        let home = temp.path().join("home");
        let made = home.join(".cursor/worktrees/trial-cursor-20261014t170432z/trial-b");
        // As written: in a home of its own, so it exists on disk even where
        // the filesystem ignores case.
        let other_home = temp.path().join("other-home");
        let as_written = other_home.join(".cursor/worktrees/Trial-Cursor-20261014T170432Z/trial-b");
        for dir in [&root, &made, &as_written] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let branch = Some("refs/heads/trial-b");
        let made = std::fs::canonicalize(made).unwrap();
        assert_eq!(
            session(&root, &made, branch, Some(&home)).as_deref(),
            Some("trial-b")
        );
        let as_written = std::fs::canonicalize(as_written).unwrap();
        assert_eq!(session(&root, &as_written, branch, Some(&other_home)), None);
    }
}
