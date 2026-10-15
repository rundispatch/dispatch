//! What Codex records about a worktree it made (`codex exec --worktree`),
//! read without running Git. Observed with Codex 0.155.1
//! (`docs/plan-0.4.14.md` §3 C): the worktree is
//! `~/.codex/worktrees/<id>/<project directory name>`, detached at the
//! project's HEAD, and its Git metadata directory holds `codex-thread.json`
//! naming the thread that owns it. Nothing names the process, so the end of
//! its session is not observable.

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Thread {
    owner_thread_id: String,
}

/// The thread that owns the worktree at `worktree`, if Codex made it: the
/// marker alone decides, wherever the worktree is.
pub fn thread(worktree: &std::path::Path) -> Option<String> {
    let gitdir = super::claude::linked_gitdir(worktree)?;
    let bytes = std::fs::read(gitdir.join("codex-thread.json")).ok()?;
    let thread: Thread = serde_json::from_slice(&bytes).ok()?;
    let id = thread.owner_thread_id.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_readable_thread_marker_names_a_codex_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let gitdir = temp.path().join("repo/.git/worktrees/repo");
        let worktree = temp.path().join("codex/7db1/repo");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        assert_eq!(thread(&worktree), None);
        let marker = gitdir.join("codex-thread.json");
        for malformed in [
            "{",
            "{}",
            r#"{"version":1}"#,
            r#"{"version":1,"ownerThreadId":""}"#,
            r#"{"version":1,"ownerThreadId":7}"#,
        ] {
            std::fs::write(&marker, malformed).unwrap();
            assert_eq!(thread(&worktree), None, "{malformed}");
        }
        // As Codex 0.155.1 wrote it.
        std::fs::write(
            &marker,
            r#"{"version":1,"ownerThreadId":"01a13b38-0758-7861-93a0-594c6fc24aa2"}"#,
        )
        .unwrap();
        assert_eq!(
            thread(&worktree).as_deref(),
            Some("01a13b38-0758-7861-93a0-594c6fc24aa2")
        );
        assert_eq!(thread(&temp.path().join("plain")), None);
    }
}
