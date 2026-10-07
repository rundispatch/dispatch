use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fs::{self, File, Metadata},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::Builder;
use walkdir::{DirEntry, WalkDir};

use crate::{
    DiffStats, RunRecord, SourceKind,
    executor::{trusted_host_executable, trusted_host_path},
    state::write_atomically,
};

const BASELINE_DIRECTORY: &str = "baseline";
const EXCLUDED_DIRECTORIES: [&str; 2] = [".git", ".dispatch"];
const MAX_CANDIDATE_FILES: u64 = 200_000;
const MAX_CANDIDATE_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CANDIDATE_TREE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_GIT_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// The immutable, Dispatch-managed representation of a source tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSnapshot {
    pub kind: SourceKind,
    pub git_head: Option<String>,
    pub fingerprint: String,
    pub baseline_path: PathBuf,
    pub baseline_commit: String,
}

/// A summary of an explicitly applied candidate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplyReport {
    pub candidate_label: String,
    pub files_changed: u64,
}

/// Resolve an explicit source directory, or the current directory when omitted.
pub fn resolve_source(input: Option<&Path>) -> Result<PathBuf> {
    let input = input.unwrap_or_else(|| Path::new("."));
    let source = fs::canonicalize(input)
        .with_context(|| format!("failed to resolve source {}", input.display()))?;
    ensure!(
        fs::metadata(&source)
            .with_context(|| format!("failed to inspect source {}", source.display()))?
            .is_dir(),
        "source is not a directory: {}",
        source.display()
    );
    Ok(source)
}

/// Identify whether a source is an ordinary directory, a Git repository, or a
/// linked Git worktree. The returned commit is `None` for an unborn repository.
pub fn inspect_source(source: &Path) -> Result<(SourceKind, Option<String>)> {
    let source = resolve_source(Some(source))?;

    let mut inside = git_command(&source);
    inside.args(["rev-parse", "--is-inside-work-tree"]);
    let output = inside
        .output()
        .with_context(|| "failed to run Git while inspecting the source")?;
    if !output.status.success() || trim_ascii(&output.stdout) != b"true" {
        return Ok((SourceKind::Directory, None));
    }

    let mut git_dir_command = git_command(&source);
    git_dir_command.args(["rev-parse", "--absolute-git-dir"]);
    let git_dir_output = checked_output(
        git_dir_command,
        "failed to locate the source repository's Git directory",
    )?;
    let git_dir = bytes_to_path(trim_ascii(&git_dir_output.stdout));
    let kind = if git_dir.join("commondir").is_file() {
        SourceKind::GitWorktree
    } else {
        SourceKind::Git
    };

    let mut head_command = git_command(&source);
    head_command.args(["rev-parse", "--verify", "HEAD"]);
    let head_output = head_command
        .output()
        .with_context(|| "failed to query the source repository's HEAD")?;
    let head = if head_output.status.success() {
        Some(
            String::from_utf8(head_output.stdout)
                .context("Git returned a non-UTF-8 HEAD commit")?
                .trim()
                .to_owned(),
        )
    } else {
        None
    };

    Ok((kind, head))
}

/// The identity of a Git repository, shared by its main worktree and every
/// linked worktree: the common Git directory (canonical), the main worktree's
/// path (canonical), and a key derived from the common directory so two paths
/// of the same repository always compare equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdentity {
    pub common_dir: PathBuf,
    pub main_worktree: PathBuf,
    pub key: String,
}

/// Identify the repository a path belongs to. `None` for a plain directory
/// (whatever `inspect_source` reports as `SourceKind::Directory`).
pub fn repo_identity(path: &Path) -> Result<Option<RepoIdentity>> {
    let path = resolve_source(Some(path))?;
    let (kind, _) = inspect_source(&path)?;
    if matches!(kind, SourceKind::Directory) {
        return Ok(None);
    }

    let mut common_dir_command = git_command(&path);
    common_dir_command.args(["rev-parse", "--git-common-dir"]);
    let common_dir_output = checked_output(
        common_dir_command,
        "failed to locate the repository's common Git directory",
    )?;
    // A linked worktree already prints an absolute path here; the main
    // worktree prints one relative to itself (typically ".git").
    let raw_common_dir = bytes_to_path(trim_ascii(&common_dir_output.stdout));
    let common_dir = if raw_common_dir.is_absolute() {
        raw_common_dir
    } else {
        path.join(raw_common_dir)
    };
    let common_dir = fs::canonicalize(&common_dir).with_context(|| {
        format!(
            "failed to resolve the repository's common Git directory {}",
            common_dir.display()
        )
    })?;

    let mut worktree_list_command = git_command(&path);
    worktree_list_command.args(["worktree", "list", "--porcelain"]);
    let worktree_list_output = checked_output(
        worktree_list_command,
        "failed to list the repository's worktrees",
    )?;
    let main_worktree_line = worktree_list_output
        .stdout
        .split(|byte| *byte == b'\n')
        .find(|line| line.starts_with(b"worktree "))
        .context("git worktree list produced no worktree entry")?;
    let raw_main_worktree = bytes_to_path(trim_ascii(&main_worktree_line[b"worktree ".len()..]));
    let main_worktree = fs::canonicalize(&raw_main_worktree).with_context(|| {
        format!(
            "failed to resolve the repository's main worktree {}",
            raw_main_worktree.display()
        )
    })?;

    let mut hasher = Sha256::new();
    hasher.update(path_bytes(&common_dir));
    let key = hex::encode(hasher.finalize());

    Ok(Some(RepoIdentity {
        common_dir,
        main_worktree,
        key,
    }))
}

/// The top of the Git checkout `path` lies in (a subdirectory's worktree
/// root), or `None` outside any Git checkout.
pub fn checkout_top(path: &Path) -> Result<Option<PathBuf>> {
    let path = resolve_source(Some(path))?;
    if repo_identity(&path)?.is_none() {
        return Ok(None);
    }
    let mut top = git_command(&path);
    top.args(["rev-parse", "--show-toplevel"]);
    let output = checked_output(top, "failed to find the checkout's top")?;
    let top = bytes_to_path(trim_ascii(&output.stdout));
    Ok(Some(fs::canonicalize(&top).with_context(|| {
        format!("failed to resolve {}", top.display())
    })?))
}

/// The merge base of `root`'s `HEAD` and `workspace`'s `HEAD`, or `None` when
/// they share no history. `root` and `workspace` must name the same
/// repository (equal `repo_identity` keys); a workspace from an unrelated
/// repository is refused rather than compared, since Git's answer there would
/// be either an error or a misleading coincidence.
pub fn merge_base(root: &Path, workspace: &Path) -> Result<Option<String>> {
    let root = resolve_source(Some(root))?;
    let workspace = resolve_source(Some(workspace))?;

    let root_identity = repo_identity(&root)?
        .with_context(|| format!("{} is not a Git repository", root.display()))?;
    let workspace_identity = repo_identity(&workspace)?
        .with_context(|| format!("{} is not a Git repository", workspace.display()))?;
    ensure!(
        root_identity.key == workspace_identity.key,
        "workspace {} belongs to a different repository than {}",
        workspace.display(),
        root.display()
    );

    let mut head_command = git_command(&workspace);
    head_command.args(["rev-parse", "--verify", "HEAD"]);
    let head_output = checked_output(head_command, "failed to resolve the workspace HEAD")?;
    let workspace_head = String::from_utf8(trim_ascii(&head_output.stdout).to_vec())
        .context("Git returned a non-UTF-8 workspace HEAD commit")?;

    let mut merge_base_command = git_command(&root);
    merge_base_command.args(["merge-base", "HEAD", &workspace_head]);
    let output = run_git(merge_base_command, None, "failed to compute the merge base")?;
    if output.status.success() {
        let commit = String::from_utf8(trim_ascii(&output.stdout).to_vec())
            .context("Git returned a non-UTF-8 merge base commit")?;
        Ok(Some(commit))
    } else if output.status.code() == Some(1) {
        // No common ancestor: the documented "no merge base" exit status.
        Ok(None)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            bail!(
                "failed to compute the merge base (Git exited with {})",
                output.status
            );
        }
        bail!("failed to compute the merge base: {stderr}");
    }
}

/// Freeze the exact current source contents in a private Git repository.
///
/// This deliberately snapshots the working tree rather than merely cloning
/// `HEAD`, so dirty and untracked source files are part of every candidate's
/// common baseline. Git and Dispatch state directories are never copied.
pub fn create_snapshot(source: &Path, run_dir: &Path) -> Result<SourceSnapshot> {
    let source = resolve_source(Some(source))?;
    let projected_run_dir = canonicalize_allow_missing(run_dir)?;
    ensure!(
        !projected_run_dir.starts_with(&source),
        "Dispatch run directory must be outside the source tree: {}",
        run_dir.display()
    );

    let (kind, git_head) = inspect_source(&source)?;
    let fingerprint_before = fingerprint_tree(&source)?;

    fs::create_dir_all(run_dir)
        .with_context(|| format!("failed to create run directory {}", run_dir.display()))?;
    let baseline_path = run_dir.join(BASELINE_DIRECTORY);
    ensure!(
        fs::symlink_metadata(&baseline_path).is_err(),
        "baseline already exists: {}",
        baseline_path.display()
    );

    let staging = Builder::new()
        .prefix(".dispatch-baseline-")
        .tempdir_in(run_dir)
        .with_context(|| format!("failed to create a snapshot in {}", run_dir.display()))?;
    copy_tree_contents(&source, staging.path())?;

    // A source changed while it was being copied is not a trustworthy baseline.
    // Check both ends so a mixed snapshot cannot be accepted silently.
    let fingerprint_after = fingerprint_tree(&source)?;
    let copied_fingerprint = fingerprint_tree(staging.path())?;
    ensure!(
        fingerprint_before == fingerprint_after && fingerprint_before == copied_fingerprint,
        "source changed while Dispatch was creating its baseline; retry the run"
    );

    // A Git source's world (`world::observe`) is the tree minus its own ignore
    // rules; the baseline commit must track exactly that, or a build artifact
    // that happens to exist at snapshot time gets force-tracked, regenerated by
    // a check, and mistaken for part of the candidate's Δ. A plain directory has
    // no ignore rules for its world, so it keeps tracking everything.
    let honor_ignore = matches!(kind, SourceKind::Git | SourceKind::GitWorktree);
    initialize_internal_repository(staging.path(), &source, honor_ignore)?;
    let baseline_commit = git_head_at(staging.path())?
        .context("internal baseline repository did not produce a commit")?;

    fs::rename(staging.path(), &baseline_path)
        .with_context(|| format!("failed to finalize baseline {}", baseline_path.display()))?;

    Ok(SourceSnapshot {
        kind,
        git_head,
        fingerprint: fingerprint_before,
        baseline_path,
        baseline_commit,
    })
}

/// Materialize a commit's tree (not the live working tree) as the baseline
/// for attached work: `S0` is `merge_base(root, workspace)`, a real commit
/// that predates the agent's edits, rather than a snapshot of anything
/// currently checked out. Follows the same staging/fingerprint/rename
/// discipline as `create_snapshot`, and the same reasoning for honoring the
/// source's ignore rules: the baseline must track exactly what
/// `world::observe` considers part of the world, or a build artifact
/// materialized at the commit gets force-tracked and mistaken for Δ. `repo`
/// supplies both the commit's objects and, via `initialize_internal_repository`,
/// its `info/exclude`.
pub fn materialize_baseline_from_commit(
    repo: &Path,
    commit: &str,
    run_dir: &Path,
) -> Result<SourceSnapshot> {
    let repo = resolve_source(Some(repo))?;
    fs::create_dir_all(run_dir)
        .with_context(|| format!("failed to create run directory {}", run_dir.display()))?;
    let baseline_path = run_dir.join(BASELINE_DIRECTORY);
    ensure!(
        fs::symlink_metadata(&baseline_path).is_err(),
        "baseline already exists: {}",
        baseline_path.display()
    );

    let staging = Builder::new()
        .prefix(".dispatch-baseline-")
        .tempdir_in(run_dir)
        .with_context(|| format!("failed to create a baseline in {}", run_dir.display()))?;
    export_commit_tree(&repo, commit, staging.path())?;

    initialize_internal_repository(staging.path(), &repo, true)?;
    let baseline_commit = git_head_at(staging.path())?
        .context("internal baseline repository did not produce a commit")?;
    let fingerprint = fingerprint_tree(staging.path())?;

    fs::rename(staging.path(), &baseline_path)
        .with_context(|| format!("failed to finalize baseline {}", baseline_path.display()))?;

    Ok(SourceSnapshot {
        kind: SourceKind::Git,
        git_head: Some(commit.to_owned()),
        fingerprint,
        baseline_path,
        baseline_commit,
    })
}

/// The exact world of a Git checkout as a commit, without copying files: its
/// tracked files as they are now, dirty or deleted, and its untracked files
/// its ignore rules do not exclude (what `world::observe` calls the world),
/// with `HEAD` as parent. Built like `git stash create`: staged into a copy of
/// the checkout's own index (so Git's stat cache spares unchanged files),
/// never its real index. Untracked nested repositories are left out, as the
/// world leaves them out. The commit's objects live in the checkout's own
/// repository, written durably; `materialize_baseline_from_commit` turns it
/// into a baseline.
///
/// A capture during which the checkout moved is never accepted, since it may
/// mix files from before and after a change: the checkout's change signal
/// (`world::signal`) is taken before and after it. If it moved, the capture is
/// made once more; if that one moves too, this is an error.
pub fn world_commit(checkout: &Path) -> Result<String> {
    let checkout = resolve_source(Some(checkout))?;
    capture_settled(&checkout, || capture_world(&checkout))
}

fn capture_settled(checkout: &Path, mut capture: impl FnMut() -> Result<String>) -> Result<String> {
    let signal = || crate::coherence::world::signal(checkout, &SourceKind::Git);
    let mut before = signal()?;
    for _ in 0..2 {
        let commit = capture()?;
        let after = signal()?;
        if after == before {
            return Ok(commit);
        }
        before = after;
    }
    bail!(
        "the workspace changed while its starting state was being captured ({}); \
         try again once it is quiet",
        checkout.display()
    )
}

/// Objects S0 is made of are fsynced as they are written.
const FSYNC_OBJECTS: [&str; 2] = ["-c", "core.fsync=loose-object"];

fn capture_world(checkout: &Path) -> Result<String> {
    let scratch = Builder::new()
        .prefix("dispatch-world-")
        .tempdir()
        .context("failed to create a temporary Git index")?;
    let index = scratch.path().join("index");

    let mut index_path = git_command(checkout);
    index_path.args(["rev-parse", "--path-format=absolute", "--git-path", "index"]);
    let real_index = PathBuf::from(
        String::from_utf8_lossy(
            &checked_output(index_path, "failed to locate the Git index")?.stdout,
        )
        .trim(),
    );
    if real_index.is_file() {
        fs::copy(&real_index, &index)
            .with_context(|| format!("failed to copy {}", real_index.display()))?;
    }
    let head = git_head_at(checkout)?;
    if !real_index.is_file() && head.is_some() {
        let mut read_tree = git_command(checkout);
        read_tree
            .env("GIT_INDEX_FILE", &index)
            .args(["read-tree", "HEAD"]);
        checked_output(read_tree, "failed to initialize a temporary Git index")?;
    }

    // Untracked nested repositories are listed as `dir/`; the world skips them.
    let mut list = git_command(checkout);
    list.env("GIT_INDEX_FILE", &index)
        .args(["ls-files", "--others", "--exclude-standard", "-z"]);
    let listed = checked_output(list, "failed to list the checkout's untracked files")?;
    let nested: Vec<String> = listed
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|raw| raw.strip_suffix(b"/"))
        .map(|directory| format!(":(exclude,literal){}", String::from_utf8_lossy(directory)))
        .collect();

    let mut add = git_command(checkout);
    add.env("GIT_INDEX_FILE", &index)
        .args(FSYNC_OBJECTS)
        .args(["add", "-A", "--", "."])
        .args(dispatch_exclusion_pathspecs())
        .args(&nested);
    checked_output(add, "failed to stage the checkout's world")?;

    let mut write_tree = git_command(checkout);
    write_tree
        .env("GIT_INDEX_FILE", &index)
        .args(FSYNC_OBJECTS)
        .arg("write-tree");
    let tree = checked_output(write_tree, "failed to write the checkout's world")?.stdout;
    let tree = String::from_utf8_lossy(&tree).trim().to_owned();

    let mut commit = git_command(checkout);
    commit
        .env("GIT_AUTHOR_NAME", "Dispatch")
        .env("GIT_AUTHOR_EMAIL", "dispatch@localhost")
        .env("GIT_COMMITTER_NAME", "Dispatch")
        .env("GIT_COMMITTER_EMAIL", "dispatch@localhost")
        .args(FSYNC_OBJECTS)
        .args(["commit-tree", &tree, "-m", "Dispatch S0"]);
    if let Some(head) = &head {
        commit.args(["-p", head]);
    }
    let commit = checked_output(commit, "failed to record the checkout's world")?.stdout;
    Ok(String::from_utf8_lossy(&commit).trim().to_owned())
}

/// A private workspace at `dir` that is the baseline plus `patch`: the Work
/// as it was when its own workspace was removed, for verifying it.
pub fn rebuild_workspace(baseline_path: &Path, patch: &Path, dir: &Path) -> Result<PathBuf> {
    let workspace = create_candidate_workspace(baseline_path, dir)?;
    if fs::metadata(patch)?.len() > 0 {
        let mut apply = git_command(&workspace);
        apply
            .args(["apply", "--binary", "--whitespace=nowarn", "--"])
            .arg(patch);
        checked_output(apply, "failed to rebuild the work from its kept changes")?;
    }
    Ok(workspace)
}

/// A linked worktree of `checkout`'s repository at `path`, on a new `branch`
/// at `commit`: a workspace whose files are exactly that commit. `path` must
/// lie outside the checkout.
pub fn create_linked_workspace(
    checkout: &Path,
    commit: &str,
    path: &Path,
    branch: &str,
) -> Result<()> {
    let checkout = resolve_source(Some(checkout))?;
    let projected = canonicalize_allow_missing(path)?;
    ensure!(
        !projected.starts_with(&checkout),
        "a workspace must be outside the checkout: {}",
        path.display()
    );
    ensure!(
        fs::symlink_metadata(path).is_err(),
        "workspace already exists: {}",
        path.display()
    );
    let mut add = git_command(&checkout);
    add.args(["worktree", "add", "--quiet", "-b", branch, "--"])
        .arg(path)
        .arg(commit);
    checked_output(add, "failed to create the workspace")?;
    Ok(())
}

/// Remove a linked worktree `create_linked_workspace` made, and its branch.
pub fn remove_linked_workspace(checkout: &Path, path: &Path, branch: &str) -> Result<()> {
    let mut remove = git_command(checkout);
    remove
        .args(["worktree", "remove", "--force", "--"])
        .arg(path);
    checked_output(remove, "failed to remove the workspace")?;
    let mut delete = git_command(checkout);
    delete.args(["branch", "-D", "--", branch]);
    checked_output(delete, "failed to delete the workspace's branch")?;
    Ok(())
}

/// Create a complete, independent workspace at the frozen baseline commit.
pub fn create_candidate_workspace(baseline_path: &Path, candidate_dir: &Path) -> Result<PathBuf> {
    let baseline_path = resolve_source(Some(baseline_path))?;
    ensure_internal_repository(&baseline_path)?;
    ensure_repository_clean(&baseline_path, "baseline")?;

    ensure!(
        fs::symlink_metadata(candidate_dir).is_err(),
        "candidate workspace already exists: {}",
        candidate_dir.display()
    );
    let candidate_dir = canonicalize_allow_missing(candidate_dir)?;
    ensure!(
        !candidate_dir.starts_with(&baseline_path),
        "candidate workspace must not be inside the baseline repository"
    );
    if let Some(parent) = candidate_dir.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create candidate parent {}", parent.display()))?;
    }

    let result = (|| {
        let mut clone = plain_git_command();
        clone
            .args(["clone", "--quiet", "--no-hardlinks", "--no-checkout", "--"])
            .arg(&baseline_path)
            .arg(&candidate_dir);
        checked_output(clone, "failed to clone the frozen baseline")?;

        configure_candidate_repository(&candidate_dir)?;
        let mut read_tree = git_command(&candidate_dir);
        read_tree.args(["read-tree", "HEAD"]);
        checked_output(read_tree, "failed to prepare the candidate Git index")?;

        // The clone intentionally has no checkout. Copying the pristine
        // worktree creates the exact source bytes, empty directories, symlink
        // targets, and permissions instead of asking Git to reconstruct the
        // subset of metadata its object model represents.
        copy_tree_contents(&baseline_path, &candidate_dir)?;
        ensure_repository_clean(&candidate_dir, "candidate workspace")?;
        ensure!(
            fingerprint_tree(&baseline_path)? == fingerprint_tree(&candidate_dir)?,
            "candidate workspace does not exactly match the frozen baseline"
        );
        Ok(candidate_dir.clone())
    })();

    if result.is_err() {
        remove_created_directory(&candidate_dir);
    }
    result
}

/// Collect a binary-capable unified patch and deterministic change statistics.
///
/// A temporary Git index is used so untracked files can be represented in the
/// patch without altering the candidate's real index.
pub fn collect_diff(baseline_path: &Path, workspace: &Path, diff_path: &Path) -> Result<DiffStats> {
    let baseline_path = resolve_source(Some(baseline_path))?;
    let workspace = resolve_source(Some(workspace))?;
    ensure_internal_repository(&baseline_path)?;
    ensure_repository_clean(&baseline_path, "baseline")?;

    let projected_diff_path = canonicalize_allow_missing(diff_path)?;
    ensure!(
        !projected_diff_path.starts_with(&workspace),
        "diff artifact must be stored outside the candidate workspace"
    );
    validate_candidate_tree(&baseline_path, &workspace)?;

    let untracked_files = list_untracked_files(&baseline_path, &workspace)?;
    let index_directory = Builder::new()
        .prefix("dispatch-index-")
        .tempdir()
        .context("failed to create a temporary Git index")?;
    let index_path = index_directory.path().join("index");

    let mut read_tree = comparison_git_command(&baseline_path, &workspace);
    read_tree
        .env("GIT_INDEX_FILE", &index_path)
        .args(["read-tree", "HEAD"]);
    checked_output(read_tree, "failed to initialize temporary Git index")?;

    let mut add = comparison_git_command(&baseline_path, &workspace);
    add.env("GIT_INDEX_FILE", &index_path)
        .args(["add", "-A", "--", "."])
        .args(dispatch_exclusion_pathspecs());
    checked_output(add, "failed to stage candidate changes for diff collection")?;

    let mut diff = comparison_git_command(&baseline_path, &workspace);
    diff.env("GIT_INDEX_FILE", &index_path).args([
        "diff",
        "--cached",
        "--binary",
        "--full-index",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "HEAD",
        "--",
    ]);
    let patch = checked_output(diff, "failed to collect candidate diff")?.stdout;

    let mut numstat = comparison_git_command(&baseline_path, &workspace);
    numstat.env("GIT_INDEX_FILE", &index_path).args([
        "diff",
        "--cached",
        "--numstat",
        "--no-renames",
        "-z",
        "HEAD",
        "--",
    ]);
    let numstat = checked_output(numstat, "failed to collect candidate diff statistics")?;
    let (files_changed, lines_added, lines_removed, paths) = parse_numstat(&numstat.stdout)?;
    let mut changed_files = paths
        .into_iter()
        .map(|path| String::from_utf8_lossy(&path).into_owned())
        .collect::<Vec<_>>();
    changed_files.sort();

    if let Some(parent) = diff_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create diff directory {}", parent.display()))?;
    }
    fs::write(diff_path, patch)
        .with_context(|| format!("failed to write diff artifact {}", diff_path.display()))?;

    Ok(DiffStats {
        files_changed,
        lines_added,
        lines_removed,
        changed_files,
        untracked_files,
    })
}

/// Write the work-so-far of a live candidate workspace as a patch against the
/// baseline, for the mid-run coherence watcher. It uses the same hardened
/// temporary-index technique as `collect_diff` but does not validate the tree
/// (the agent is still writing it) and computes no statistics. The result is
/// advisory evidence only and is never applied.
pub fn snapshot_delta(baseline_path: &Path, workspace: &Path, out_patch: &Path) -> Result<()> {
    let index_directory = Builder::new()
        .prefix("dispatch-index-")
        .tempdir()
        .context("failed to create a temporary Git index")?;
    snapshot_delta_indexed(
        baseline_path,
        workspace,
        out_patch,
        &index_directory.path().join("index"),
    )
}

/// `snapshot_delta` with an index the caller keeps between snapshots of the
/// same workspace: Git's stat cache then re-hashes only files that changed,
/// which is what makes following work every tick affordable. The baseline's
/// HEAD never moves, so the index is initialized from it only once.
pub fn snapshot_delta_indexed(
    baseline_path: &Path,
    workspace: &Path,
    out_patch: &Path,
    index_path: &Path,
) -> Result<()> {
    if !index_path.exists() {
        let mut read_tree = comparison_git_command(baseline_path, workspace);
        read_tree
            .env("GIT_INDEX_FILE", index_path)
            .args(["read-tree", "HEAD"]);
        checked_output(read_tree, "failed to initialize temporary Git index")?;
    }

    let mut add = comparison_git_command(baseline_path, workspace);
    add.env("GIT_INDEX_FILE", index_path)
        .args(["add", "-A", "--", "."])
        .args(dispatch_exclusion_pathspecs());
    checked_output(add, "failed to stage the work in progress")?;

    let mut diff = comparison_git_command(baseline_path, workspace);
    diff.env("GIT_INDEX_FILE", index_path).args([
        "diff",
        "--cached",
        "--binary",
        "--full-index",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "HEAD",
        "--",
    ]);
    let patch = checked_output(diff, "failed to collect the work in progress")?.stdout;
    // Watchers and the owner read this file while it is rewritten, so publish
    // it by rename. No fsync: it is advisory evidence, not durable state.
    write_atomically(out_patch, &patch)
}

/// Hash the complete logical source tree, excluding Git and Dispatch state.
/// Content, paths, symlink targets, file kinds, and permission bits are covered;
/// timestamps are intentionally ignored. An entry that disappears during the
/// walk (a build deleting its output) is a difference, never an error: it is
/// hashed as `VANISHED`, so the value equals no tree that held the entry.
pub fn fingerprint_tree(root: &Path) -> Result<String> {
    let root = resolve_source(Some(root))?;
    let entries = list_tree(&root)?;
    hash_tree(&root, entries)
}

/// Hashed in place of an entry's kind (`l`, `d` or `f`) when it vanished.
const VANISHED: &[u8] = b"x";

/// Every path under `root` but Git and Dispatch state, in fingerprint order,
/// each with whether it could be listed: a directory that vanished before the
/// walk read it is kept, as not listed.
fn list_tree(root: &Path) -> Result<Vec<(PathBuf, bool)>> {
    let mut entries = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(include_entry)
    {
        match entry {
            Ok(entry) if entry.depth() > 0 => entries.push((entry.into_path(), true)),
            Ok(_) => {}
            Err(error) => {
                let vanished = error.depth() > 0
                    && error
                        .io_error()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound);
                match error.path() {
                    Some(path) if vanished => entries.push((path.to_owned(), false)),
                    _ => {
                        return Err(error)
                            .with_context(|| format!("failed to walk {}", root.display()));
                    }
                }
            }
        }
    }
    entries.sort_by(|(left, _), (right, _)| {
        path_bytes(left.strip_prefix(root).expect("walked path is under root")).cmp(&path_bytes(
            right.strip_prefix(root).expect("walked path is under root"),
        ))
    });
    Ok(entries)
}

/// `Ok(None)` when the entry was not found: it vanished since it was listed.
fn unless_vanished<T>(
    result: std::io::Result<T>,
    context: impl FnOnce() -> String,
) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(context),
    }
}

fn hash_tree(root: &Path, entries: Vec<(PathBuf, bool)>) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"dispatch-tree-v1\0");
    let mut buffer = vec![0_u8; 64 * 1024];
    for (path, listed) in entries {
        let relative = path.strip_prefix(root).expect("walked path is under root");
        let path_key = path_bytes(relative);
        hash_sized_bytes(&mut hasher, &path_key);

        let metadata = if listed {
            unless_vanished(fs::symlink_metadata(&path), || {
                format!("failed to inspect {}", path.display())
            })?
        } else {
            None
        };
        let Some(metadata) = metadata else {
            hasher.update(VANISHED);
            continue;
        };
        if metadata.file_type().is_symlink() {
            let Some(target) = unless_vanished(fs::read_link(&path), || {
                format!("failed to read symlink {}", path.display())
            })?
            else {
                hasher.update(VANISHED);
                continue;
            };
            ensure_symlink_stays_within(root, &path, &target)?;
            hasher.update(b"l");
            hash_mode(&mut hasher, &metadata);
            hash_sized_bytes(&mut hasher, &path_bytes(&target));
        } else if metadata.is_dir() {
            hasher.update(b"d");
            hash_mode(&mut hasher, &metadata);
        } else if metadata.is_file() {
            let Some(mut file) = unless_vanished(File::open(&path), || {
                format!("failed to read {}", path.display())
            })?
            else {
                hasher.update(VANISHED);
                continue;
            };
            hasher.update(b"f");
            hash_mode(&mut hasher, &metadata);
            hasher.update(metadata.len().to_le_bytes());
            loop {
                let read = file
                    .read(&mut buffer)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
        } else {
            bail!(
                "unsupported special file in source tree: {}",
                path.display()
            );
        }
    }

    Ok(hex::encode(hasher.finalize()))
}

/// Apply one stored candidate patch after proving that the original source has
/// not changed since snapshotting. `git apply` performs a complete dry run
/// before the real application, and its default all-or-nothing behavior is kept.
pub fn safe_apply(run: &RunRecord, candidate_label: &str) -> Result<ApplyReport> {
    apply_checked(run, candidate_label, ExpectedWorld::Fingerprint)
}

/// Like `safe_apply`, but for a source that moved after snapshotting and whose
/// current non-ignored world (`world_digest`) coherence validation has already
/// judged compatible. The patch is applied only if that exact world is still
/// present at the check, after the dry run, and therefore at the real apply.
pub fn apply_validated(
    run: &RunRecord,
    candidate_label: &str,
    world_digest: &str,
) -> Result<ApplyReport> {
    apply_checked(run, candidate_label, ExpectedWorld::Digest(world_digest))
}

#[derive(Clone, Copy)]
enum ExpectedWorld<'a> {
    Fingerprint,
    Digest(&'a str),
}

impl ExpectedWorld<'_> {
    fn holds(self, run: &RunRecord, source: &Path) -> Result<bool> {
        Ok(match self {
            Self::Fingerprint => fingerprint_tree(source)? == run.source_fingerprint,
            Self::Digest(expected) => {
                crate::coherence::world::observe(
                    source,
                    &run.baseline_path,
                    &run.baseline_commit,
                    &run.source_kind,
                )?
                .digest
                    == expected
            }
        })
    }
}

fn apply_checked(
    run: &RunRecord,
    candidate_label: &str,
    expected: ExpectedWorld,
) -> Result<ApplyReport> {
    let matches = run
        .candidates
        .iter()
        .filter(|candidate| candidate.label == candidate_label)
        .collect::<Vec<_>>();
    let candidate = match matches.as_slice() {
        [candidate] => *candidate,
        [] => bail!("candidate not found: {candidate_label}"),
        _ => bail!("candidate label is ambiguous: {candidate_label}"),
    };

    let source = resolve_source(Some(&run.source_path))?;
    ensure!(
        expected.holds(run, &source)?,
        "source has changed since this run was created; refusing to apply candidate {candidate_label}"
    );

    let patch_metadata = fs::metadata(&candidate.diff_path).with_context(|| {
        format!(
            "failed to inspect candidate diff {}",
            candidate.diff_path.display()
        )
    })?;
    ensure!(
        patch_metadata.is_file(),
        "candidate diff is not a file: {}",
        candidate.diff_path.display()
    );
    if patch_metadata.len() == 0 {
        return Ok(ApplyReport {
            candidate_label: candidate_label.to_owned(),
            files_changed: 0,
        });
    }

    let patch_paths = inspect_patch_paths(&source, &candidate.diff_path)?;
    ensure!(
        !patch_paths.is_empty(),
        "candidate diff contains no applicable file changes"
    );
    for path in &patch_paths {
        ensure_safe_patch_path(path)?;
    }

    let mut check = git_command(&source);
    check
        .args(["apply", "--check", "--binary", "--whitespace=nowarn", "--"])
        .arg(&candidate.diff_path);
    checked_output(
        check,
        "candidate cannot be applied cleanly; the source was left unchanged",
    )?;

    // Close the most useful check/apply race window. The real `git apply` also
    // validates every hunk before writing any file.
    ensure!(
        expected.holds(run, &source)?,
        "source changed during apply validation; the source was left unchanged"
    );

    let mut apply = git_command(&source);
    apply
        .args(["apply", "--binary", "--whitespace=nowarn", "--"])
        .arg(&candidate.diff_path);
    checked_output(
        apply,
        "candidate application failed; Git did not apply the patch",
    )?;

    Ok(ApplyReport {
        candidate_label: candidate_label.to_owned(),
        files_changed: patch_paths.len() as u64,
    })
}

/// Apply a candidate patch inside a disposable workspace (never the source).
pub(crate) fn apply_patch_in_workspace(workspace: &Path, patch: &Path) -> Result<()> {
    let mut apply = git_command(workspace);
    apply
        .args(["apply", "--binary", "--whitespace=nowarn", "--"])
        .arg(patch);
    checked_output(apply, "candidate patch does not apply to the merged tree")?;
    Ok(())
}

/// Copy the files that make up the current source into `dest` for a throwaway
/// checking tree. Unlike `create_snapshot`, ignored build output (`target/`,
/// `node_modules/`, ...) is not copied: a Git source contributes exactly what
/// `git ls-files -co --exclude-standard` lists, a directory source its whole
/// tree. Build caches are therefore absent, so a check such as `cargo test`
/// builds from scratch in the scratch tree; a check may point `CARGO_TARGET_DIR`
/// (or the equivalent) elsewhere to reuse a cache. Nested repositories that Git
/// lists as one directory entry are copied whole. The result is a plain
/// directory, not a Git repository. A tracked file deleted from the worktree is
/// simply absent; a file that vanishes while it is copied is an error.
pub(crate) fn create_scratch_tree(source: &Path, kind: &SourceKind, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    if matches!(kind, SourceKind::Directory) {
        return copy_tree_contents(source, dest);
    }
    let mut list = git_command(source);
    list.args(["ls-files", "-co", "--exclude-standard", "-z"]);
    let listing = checked_output(list, "failed to list the current source files")?;
    let mut real_directories = HashSet::new();
    for raw in listing.stdout.split(|byte| *byte == 0) {
        let relative = bytes_to_path(raw.strip_suffix(b"/").unwrap_or(raw));
        if raw.is_empty()
            || relative
                .components()
                .any(|part| is_excluded_name(part.as_os_str()))
        {
            continue;
        }
        let from = source.join(&relative);
        let to = dest.join(&relative);
        // Reading through a parent that became a symlink would leave the tree.
        if !parents_are_directories(source, &relative, &mut real_directories)
            || fs::symlink_metadata(&to).is_ok()
        {
            continue;
        }
        let metadata = match fs::symlink_metadata(&from) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to inspect {}", from.display()));
            }
        };
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        if metadata.is_dir() {
            fs::create_dir(&to).with_context(|| format!("failed to create {}", to.display()))?;
            copy_tree_contents(&from, &to)?;
        } else if metadata.file_type().is_symlink() {
            let link_target = fs::read_link(&from)
                .with_context(|| format!("failed to read symlink {}", from.display()))?;
            ensure_symlink_stays_within(source, &from, &link_target)?;
            create_symlink(&link_target, &to, &from)
                .with_context(|| format!("failed to copy symlink {}", from.display()))?;
        } else if metadata.is_file() {
            // `fs::copy` carries the permission bits, including execute.
            fs::copy(&from, &to).with_context(|| format!("failed to copy {}", from.display()))?;
        } else {
            bail!(
                "unsupported special file in source tree: {}",
                from.display()
            );
        }
    }
    Ok(())
}

/// The first parent of `relative` under `root` that is a symlink, if any.
fn symlinked_parent(root: &Path, relative: &Path) -> Result<Option<PathBuf>> {
    let mut current = root.to_path_buf();
    let parent = relative.parent().unwrap_or(Path::new(""));
    for component in parent.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(Some(current)),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", current.display()));
            }
        }
    }
    Ok(None)
}

/// True when every parent directory of `relative` under `root` is a real
/// directory (not a symlink); verified parents are remembered in `known`.
pub(crate) fn parents_are_directories(
    root: &Path,
    relative: &Path,
    known: &mut HashSet<Vec<u8>>,
) -> bool {
    let Some(parent) = relative
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return true;
    };
    if known.contains(&path_bytes(parent)) {
        return true;
    }
    if !parents_are_directories(root, parent, known) {
        return false;
    }
    match fs::symlink_metadata(root.join(parent)) {
        Ok(metadata) if metadata.is_dir() => {
            known.insert(path_bytes(parent));
            true
        }
        _ => false,
    }
}

fn copy_tree_contents(source: &Path, destination: &Path) -> Result<()> {
    let mut directory_permissions = Vec::new();
    for entry in WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(include_entry)
    {
        let entry = entry.with_context(|| format!("failed to walk {}", source.display()))?;
        if entry.depth() == 0 {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .expect("walked path is under source");
        let target = destination.join(relative);
        let metadata = fs::symlink_metadata(entry.path())
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;

        if metadata.is_dir() {
            match fs::symlink_metadata(&target) {
                Ok(existing) if !existing.is_dir() || existing.file_type().is_symlink() => bail!(
                    "snapshot destination unexpectedly contains {}",
                    target.display()
                ),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&target).with_context(|| {
                        format!("failed to create directory {}", target.display())
                    })?;
                }
                Err(error) => return Err(error.into()),
            }
            directory_permissions.push((entry.depth(), target, metadata.permissions()));
        } else if metadata.is_file() {
            ensure!(
                fs::symlink_metadata(&target).is_err(),
                "snapshot destination unexpectedly contains {}",
                target.display()
            );
            fs::copy(entry.path(), &target).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    entry.path().display(),
                    target.display()
                )
            })?;
            fs::set_permissions(&target, metadata.permissions()).with_context(|| {
                format!("failed to preserve permissions on {}", target.display())
            })?;
        } else if metadata.file_type().is_symlink() {
            ensure!(
                fs::symlink_metadata(&target).is_err(),
                "snapshot destination unexpectedly contains {}",
                target.display()
            );
            let link_target = fs::read_link(entry.path())
                .with_context(|| format!("failed to read symlink {}", entry.path().display()))?;
            create_symlink(&link_target, &target, entry.path())
                .with_context(|| format!("failed to copy symlink {}", entry.path().display()))?;
        } else {
            bail!(
                "unsupported special file in source tree: {}",
                entry.path().display()
            );
        }
    }

    // Apply child directory modes first so restrictive parents cannot prevent
    // the remaining metadata operations.
    directory_permissions.sort_by_key(|(depth, _, _)| std::cmp::Reverse(*depth));
    for (_, path, permissions) in directory_permissions {
        fs::set_permissions(&path, permissions)
            .with_context(|| format!("failed to preserve permissions on {}", path.display()))?;
    }
    Ok(())
}

fn ensure_symlink_stays_within(root: &Path, link: &Path, target: &Path) -> Result<()> {
    use std::path::Component;

    ensure!(
        !target.is_absolute(),
        "source contains an absolute symlink that escapes candidate isolation: {} -> {}",
        link.display(),
        target.display()
    );
    let parent = link
        .parent()
        .context("source symlink has no parent")?
        .strip_prefix(root)
        .context("source symlink is outside the source root")?;
    let mut components = Vec::new();
    for component in parent.join(target).components() {
        match component {
            Component::Normal(value) => components.push(value.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(
                    components.pop().is_some(),
                    "source contains a symlink that escapes candidate isolation: {} -> {}",
                    link.display(),
                    target.display()
                );
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "source contains a symlink that escapes candidate isolation: {} -> {}",
                    link.display(),
                    target.display()
                );
            }
        }
    }
    Ok(())
}

/// Refuse a candidate whose Δ would be unsafe or too large: at most
/// `MAX_CANDIDATE_FILES` files of at most `MAX_CANDIDATE_FILE_BYTES` each and
/// `MAX_CANDIDATE_TREE_BYTES` in all, no special files, no symlink that leaves
/// the tree. Only paths that can enter Δ count: the baseline's tracked paths
/// and the workspace's untracked paths its ignore rules do not exclude, as the
/// trusted baseline repository lists them for `git add -A`. Ignored build
/// output (`target/`, `node_modules/`) can never enter Δ, so it is neither
/// counted nor refused.
fn validate_candidate_tree(baseline_path: &Path, root: &Path) -> Result<()> {
    let mut list = comparison_git_command(baseline_path, root);
    list.args([
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
        "--",
        ".",
    ])
    .args(dispatch_exclusion_pathspecs());
    let listing = checked_output(list, "failed to enumerate the candidate's changes")?;
    let mut files = 0_u64;
    let mut logical_bytes = 0_u64;
    let mut real_directories = HashSet::new();
    let mut checked_links = HashSet::new();
    for raw in listing.stdout.split(|byte| *byte == 0) {
        // An untracked nested repository is listed as `dir/`, not as files.
        if raw.is_empty() || raw.ends_with(b"/") {
            continue;
        }
        let relative = bytes_to_path(raw);
        // Under a parent that became a symlink, the link is what Git records.
        if !parents_are_directories(root, &relative, &mut real_directories) {
            if let Some(link) = symlinked_parent(root, &relative)?
                && checked_links.insert(link.clone())
            {
                let target = fs::read_link(&link)
                    .with_context(|| format!("failed to read symlink {}", link.display()))?;
                ensure_symlink_stays_within(root, &link, &target)?;
            }
            continue;
        }
        let path = root.join(&relative);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            // Deleted since the baseline: part of Δ, but nothing to inspect.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect candidate path {}", path.display())
                });
            }
        };
        if metadata.is_dir() {
            continue;
        }
        files = files.saturating_add(1);
        ensure!(
            files <= MAX_CANDIDATE_FILES,
            "candidate contains more than {MAX_CANDIDATE_FILES} files; diff collection was refused"
        );
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path)
                .with_context(|| format!("failed to read symlink {}", path.display()))?;
            ensure_symlink_stays_within(root, &path, &target)?;
        } else if metadata.is_file() {
            ensure!(
                metadata.len() <= MAX_CANDIDATE_FILE_BYTES,
                "candidate file {} is larger than the {} MiB safety limit",
                path.display(),
                MAX_CANDIDATE_FILE_BYTES / (1024 * 1024)
            );
            logical_bytes = logical_bytes.saturating_add(metadata.len());
            ensure!(
                logical_bytes <= MAX_CANDIDATE_TREE_BYTES,
                "candidate tree exceeds the {} GiB logical-size safety limit",
                MAX_CANDIDATE_TREE_BYTES / (1024 * 1024 * 1024)
            );
        } else {
            bail!(
                "candidate contains unsupported special file: {}",
                path.display()
            );
        }
    }
    Ok(())
}

/// Materialize a commit's tree as real files under `staging`, via a temporary
/// index scoped to this call (`GIT_INDEX_FILE`) checked out into `staging`
/// (`GIT_WORK_TREE`), rather than `repo`'s own working tree or index.
///
/// Chosen over `git archive --format=tar <commit> | tar -x`: it stays inside
/// this module's hardened `git_command`/`checked_output` process wrapper (one
/// external tool, the existing timeout and output-size safety net) instead of
/// piping two subprocesses together and reimplementing that safety net for a
/// second tool this codebase does not otherwise depend on. Verified
/// empirically that it preserves symlinks and the executable bit exactly
/// (mode 100755 and 120000 entries round-trip byte-for-byte), and that a
/// gitlink (mode 160000, a nested repository) is left unpopulated as a bare
/// directory rather than checked out — the same treatment `world::observe`
/// gives nested repositories.
fn export_commit_tree(repo: &Path, commit: &str, staging: &Path) -> Result<()> {
    let index_directory = Builder::new()
        .prefix("dispatch-checkout-")
        .tempdir()
        .context("failed to create a temporary Git index")?;
    let index_path = index_directory.path().join("index");

    let mut read_tree = git_command(repo);
    read_tree
        .env("GIT_INDEX_FILE", &index_path)
        .args(["read-tree", commit]);
    checked_output(
        read_tree,
        "failed to read the commit tree into a temporary index",
    )?;

    let mut checkout = git_command(repo);
    checkout
        .env("GIT_INDEX_FILE", &index_path)
        .env("GIT_WORK_TREE", staging)
        .args(["checkout-index", "--all", "--force"]);
    checked_output(checkout, "failed to materialize the commit tree")?;
    Ok(())
}

/// Initialize the private Git repository that backs a baseline (or a
/// republished contribution). When `honor_ignore` is true, staging uses the
/// source's own ignore rules (`.gitignore` files, already copied into `path`
/// by the caller, and `info/exclude`, copied here from `source`) so the
/// baseline commit tracks exactly the paths `world::observe` considers part of
/// the world; ignored files remain on disk for build caches. When false,
/// staging force-adds everything, matching a plain-directory source, which has
/// no ignore rules for its world.
fn initialize_internal_repository(path: &Path, source: &Path, honor_ignore: bool) -> Result<()> {
    let mut init = git_command(path);
    init.args([
        "-c",
        "init.defaultBranch=dispatch-baseline",
        "init",
        "--quiet",
    ]);
    checked_output(init, "failed to initialize internal baseline repository")?;

    install_internal_attributes(path)?;
    if honor_ignore {
        copy_source_info_exclude(source, path)?;
    }

    for (key, value) in [
        ("user.name", "Dispatch"),
        ("user.email", "dispatch@localhost"),
        ("commit.gpgSign", "false"),
        ("core.autocrlf", "false"),
    ] {
        let mut config = git_command(path);
        config.args(["config", "--local", key, value]);
        checked_output(config, "failed to configure internal baseline repository")?;
    }

    let mut add = git_command(path);
    if honor_ignore {
        add.args(["add", "-A", "--", "."]);
    } else {
        add.args(["add", "-A", "-f", "--", "."]);
    }
    checked_output(add, "failed to stage internal baseline")?;

    let mut commit = git_command(path);
    commit
        .env("GIT_AUTHOR_NAME", "Dispatch")
        .env("GIT_AUTHOR_EMAIL", "dispatch@localhost")
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
        .env("GIT_COMMITTER_NAME", "Dispatch")
        .env("GIT_COMMITTER_EMAIL", "dispatch@localhost")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
        .args([
            "commit",
            "--quiet",
            "--no-gpg-sign",
            "--no-verify",
            "--allow-empty",
            "-m",
            "Dispatch baseline",
        ]);
    checked_output(commit, "failed to commit internal baseline")?;
    Ok(())
}

fn configure_candidate_repository(path: &Path) -> Result<()> {
    let mut config = git_command(path);
    config.args(["config", "--local", "core.autocrlf", "false"]);
    checked_output(config, "failed to configure candidate repository")?;
    install_internal_attributes(path)
}

/// The private repositories must describe the literal local files. Higher
/// priority info attributes neutralize source/global clean filters (including
/// LFS), newline normalization, and working-tree encodings that could otherwise
/// make an internal commit differ from the bytes Dispatch froze.
fn install_internal_attributes(path: &Path) -> Result<()> {
    let mut command = git_command(path);
    command.args(["rev-parse", "--absolute-git-dir"]);
    let output = checked_output(command, "failed to locate internal Git directory")?;
    let git_dir = bytes_to_path(trim_ascii(&output.stdout));
    let info = git_dir.join("info");
    fs::create_dir_all(&info).with_context(|| format!("failed to create {}", info.display()))?;
    fs::write(
        info.join("attributes"),
        b"* -text -eol -filter -ident -working-tree-encoding\n",
    )
    .context("failed to disable content filters in internal repository")?;
    Ok(())
}

/// Copy the source repository's `info/exclude` into the internal repository's
/// `info/exclude`, so repository-local excludes are honored the same way
/// `git ls-files --exclude-standard` honors them in `world::observe`. Located
/// with `git rev-parse --git-path info/exclude` run in the source, which
/// resolves to the shared main repository even for a linked worktree. Global
/// excludes are already neutralized by `GIT_CONFIG_GLOBAL=/dev/null` in
/// `plain_git_command`, so there is nothing to do for them. A source with no
/// `info/exclude` (or none of interest) leaves the internal repository as
/// `git init` created it.
fn copy_source_info_exclude(source: &Path, internal_path: &Path) -> Result<()> {
    let mut locate = git_command(source);
    locate.args(["rev-parse", "--git-path", "info/exclude"]);
    let output = checked_output(
        locate,
        "failed to locate the source repository's info/exclude",
    )?;
    let relative = bytes_to_path(trim_ascii(&output.stdout));
    let exclude_path = if relative.is_absolute() {
        relative
    } else {
        source.join(relative)
    };
    let Ok(contents) = fs::read(&exclude_path) else {
        return Ok(());
    };

    let mut git_dir_command = git_command(internal_path);
    git_dir_command.args(["rev-parse", "--absolute-git-dir"]);
    let git_dir_output =
        checked_output(git_dir_command, "failed to locate internal Git directory")?;
    let git_dir = bytes_to_path(trim_ascii(&git_dir_output.stdout));
    let info = git_dir.join("info");
    fs::create_dir_all(&info).with_context(|| format!("failed to create {}", info.display()))?;
    fs::write(info.join("exclude"), contents)
        .context("failed to copy source info/exclude into internal repository")?;
    Ok(())
}

fn ensure_internal_repository(path: &Path) -> Result<()> {
    let mut command = git_command(path);
    command.args(["rev-parse", "--verify", "HEAD"]);
    checked_output(command, "path is not a usable baseline Git repository")?;
    Ok(())
}

fn ensure_repository_clean(path: &Path, description: &str) -> Result<()> {
    let mut status = git_command(path);
    status.args([
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--ignored=no",
    ]);
    let output = checked_output(status, &format!("failed to inspect {description}"))?;
    ensure!(
        output.stdout.is_empty(),
        "{description} has unexpected changes and cannot be used safely"
    );
    Ok(())
}

fn git_head_at(path: &Path) -> Result<Option<String>> {
    let mut command = git_command(path);
    command.args(["rev-parse", "--verify", "HEAD"]);
    let output = command.output().context("failed to query Git HEAD")?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8(output.stdout)
            .context("Git returned a non-UTF-8 commit identifier")?
            .trim()
            .to_owned(),
    ))
}

fn list_untracked_files(baseline_path: &Path, workspace: &Path) -> Result<Vec<String>> {
    let mut command = comparison_git_command(baseline_path, workspace);
    command
        .args([
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ])
        .args(dispatch_exclusion_pathspecs());
    let output = checked_output(command, "failed to enumerate untracked candidate files")?;
    let mut files = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn inspect_patch_paths(source: &Path, diff_path: &Path) -> Result<Vec<PathBuf>> {
    let mut command = git_command(source);
    command
        .args(["apply", "--numstat", "-z", "--"])
        .arg(diff_path);
    let output = checked_output(command, "candidate diff is not a valid Git patch")?;
    let (_, _, _, paths) = parse_numstat(&output.stdout)?;
    Ok(paths.into_iter().map(|path| bytes_to_path(&path)).collect())
}

fn parse_numstat(bytes: &[u8]) -> Result<(u64, u64, u64, Vec<Vec<u8>>)> {
    let mut files = 0_u64;
    let mut added = 0_u64;
    let mut removed = 0_u64;
    let mut paths = Vec::new();

    for record in bytes.split(|byte| *byte == 0).filter(|r| !r.is_empty()) {
        let mut fields = record.splitn(3, |byte| *byte == b'\t');
        let added_field = fields.next().context("invalid Git numstat output")?;
        let removed_field = fields.next().context("invalid Git numstat output")?;
        let path = fields.next().context("invalid Git numstat output")?;
        ensure!(!path.is_empty(), "invalid empty path in Git numstat output");

        files += 1;
        added += parse_numstat_number(added_field)?;
        removed += parse_numstat_number(removed_field)?;
        paths.push(path.to_vec());
    }
    Ok((files, added, removed, paths))
}

fn parse_numstat_number(value: &[u8]) -> Result<u64> {
    if value == b"-" {
        return Ok(0);
    }
    let value = std::str::from_utf8(value).context("invalid Git numstat number")?;
    value
        .parse::<u64>()
        .with_context(|| format!("invalid Git numstat number: {value}"))
}

fn ensure_safe_patch_path(path: &Path) -> Result<()> {
    ensure!(
        !path.is_absolute(),
        "candidate diff contains an absolute path"
    );
    for component in path.components() {
        match component {
            Component::Normal(value) => ensure!(
                !is_excluded_name(value),
                "candidate diff attempts to modify Dispatch or Git state: {}",
                path.display()
            ),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("candidate diff contains an unsafe path: {}", path.display())
            }
        }
    }
    Ok(())
}

fn dispatch_exclusion_pathspecs() -> [&'static str; 2] {
    [
        ":(exclude,glob).dispatch/**",
        ":(exclude,glob)**/.dispatch/**",
    ]
}

pub(crate) fn include_entry(entry: &DirEntry) -> bool {
    entry.depth() == 0 || !is_excluded_name(entry.file_name())
}

pub(crate) fn is_excluded_name(name: &OsStr) -> bool {
    EXCLUDED_DIRECTORIES
        .iter()
        .any(|excluded| name == OsStr::new(excluded))
}

fn remove_created_directory(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            let _ = fs::remove_dir_all(path);
        } else {
            let _ = fs::remove_file(path);
        }
    }
}

fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to resolve current directory")?
            .join(path)
    };

    let mut cursor = absolute.as_path();
    let mut missing = Vec::<OsString>::new();
    loop {
        match fs::canonicalize(cursor) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = cursor
                    .file_name()
                    .with_context(|| format!("could not resolve destination {}", path.display()))?;
                missing.push(name.to_os_string());
                cursor = cursor
                    .parent()
                    .with_context(|| format!("could not resolve destination {}", path.display()))?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to resolve destination {}", path.display()));
            }
        }
    }
}

pub(crate) fn plain_git_command() -> Command {
    #[cfg(target_os = "macos")]
    let git = {
        let command_line_tools = PathBuf::from("/Library/Developer/CommandLineTools/usr/bin/git");
        if command_line_tools.is_file() {
            command_line_tools
        } else {
            trusted_host_executable("git").unwrap_or_else(|_| PathBuf::from("/usr/bin/git"))
        }
    };
    #[cfg(not(target_os = "macos"))]
    let git = trusted_host_executable("git").unwrap_or_else(|_| PathBuf::from("/usr/bin/git"));
    let mut command = Command::new(git);
    command.env_clear();
    for name in ["TMPDIR", "SYSTEMROOT"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("PATH", trusted_host_path())
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

pub(crate) fn git_command(directory: &Path) -> Command {
    let mut command = plain_git_command();
    command.arg("-C").arg(directory);
    command
}

/// Compare a candidate worktree using only the trusted baseline's repository
/// metadata. Candidate-controlled `.git/config`, hooks, objects, and info
/// attributes must never influence a host-side Git process.
pub(crate) fn comparison_git_command(baseline_path: &Path, workspace: &Path) -> Command {
    let mut command = plain_git_command();
    command
        .arg("--git-dir")
        .arg(baseline_path.join(".git"))
        .arg("--work-tree")
        .arg(workspace)
        .arg("-c")
        .arg("core.hooksPath=/dev/null");
    command
}

pub(crate) fn checked_output(command: Command, context: &str) -> Result<Output> {
    let output = run_git(command, None, context)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            bail!("{context} (Git exited with {})", output.status);
        }
        bail!("{context}: {stderr}");
    }
    Ok(output)
}

/// Run Git under the same timeout and output limits as `checked_output`, but
/// leave the exit status to the caller and optionally feed standard input.
pub(crate) fn run_git(mut command: Command, input: Option<&[u8]>, context: &str) -> Result<Output> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().with_context(|| context.to_owned())?;
    let child_id = child.id();
    let stdin_writer = match input {
        Some(bytes) => {
            let mut stdin = child.stdin.take().context("Git stdin was not captured")?;
            let bytes = bytes.to_vec();
            // A write error means Git exited early; its exit status reports why.
            Some(thread::spawn(move || stdin.write_all(&bytes)))
        }
        None => None,
    };
    let stdout = child.stdout.take().context("Git stdout was not captured")?;
    let stderr = child.stderr.take().context("Git stderr was not captured")?;
    let stdout_reader = thread::spawn(move || read_limited(stdout));
    let stderr_reader = thread::spawn(move || read_limited(stderr));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().with_context(|| context.to_owned())? {
            break status;
        }
        if started.elapsed() >= GIT_COMMAND_TIMEOUT {
            kill_command_group(child_id);
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            bail!(
                "{context}: Git exceeded the {} second safety timeout",
                GIT_COMMAND_TIMEOUT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(20));
    };
    kill_command_group(child_id);
    if let Some(writer) = stdin_writer {
        let _ = writer.join();
    }
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("{context}: stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("{context}: stderr reader panicked"))??;
    ensure!(
        stdout.total <= MAX_GIT_OUTPUT_BYTES as u64,
        "{context}: Git output exceeded the {} MiB safety limit",
        MAX_GIT_OUTPUT_BYTES / (1024 * 1024)
    );
    ensure!(
        stderr.total <= MAX_GIT_OUTPUT_BYTES as u64,
        "{context}: Git error output exceeded the {} MiB safety limit",
        MAX_GIT_OUTPUT_BYTES / (1024 * 1024)
    );
    Ok(Output {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
}

struct LimitedOutput {
    bytes: Vec<u8>,
    total: u64,
}

fn read_limited(mut reader: impl Read) -> std::io::Result<LimitedOutput> {
    let mut bytes = Vec::with_capacity(MAX_GIT_OUTPUT_BYTES.min(64 * 1024));
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        let remaining = MAX_GIT_OUTPUT_BYTES.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(LimitedOutput { bytes, total })
}

fn kill_command_group(child_id: u32) {
    #[cfg(unix)]
    if let Ok(pgid) = i32::try_from(child_id) {
        // SAFETY: the command was placed in its own process group immediately
        // before spawn; a negative PID targets that group only.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

pub(crate) fn hash_sized_bytes(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

#[cfg(unix)]
fn hash_mode(hasher: &mut Sha256, metadata: &Metadata) {
    use std::os::unix::fs::MetadataExt;
    hasher.update((metadata.mode() & 0o7777).to_le_bytes());
}

#[cfg(not(unix))]
fn hash_mode(hasher: &mut Sha256, metadata: &Metadata) {
    hasher.update([u8::from(metadata.permissions().readonly())]);
}

#[cfg(unix)]
pub(crate) fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
pub(crate) fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(unix)]
pub(crate) fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[cfg(windows)]
pub(crate) fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(unix)]
fn create_symlink(target: &Path, destination: &Path, _source_link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, destination).map_err(Into::into)
}

#[cfg(windows)]
fn create_symlink(target: &Path, destination: &Path, source_link: &Path) -> Result<()> {
    let resolved_target = source_link
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(target);
    if resolved_target.is_dir() {
        std::os::windows::fs::symlink_dir(target, destination).map_err(Into::into)
    } else {
        std::os::windows::fs::symlink_file(target, destination).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use chrono::Utc;
    use tempfile::TempDir;

    use super::*;
    use crate::{CandidateRecord, CandidateStatus, EnvironmentRecord, RunStatus};

    fn write(path: &Path, contents: impl AsRef<[u8]>) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn run_git(path: &Path, args: &[&str]) -> String {
        let output = plain_git_command()
            .arg("-C")
            .arg(path)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn initialize_user_repository(path: &Path) {
        run_git(path, &["init", "--quiet"]);
        run_git(path, &["config", "user.name", "Test User"]);
        run_git(path, &["config", "user.email", "test@example.invalid"]);
        run_git(path, &["add", "-A"]);
        run_git(path, &["commit", "--quiet", "-m", "initial"]);
    }

    fn candidate_record(
        label: &str,
        workspace: PathBuf,
        diff: PathBuf,
        stats: DiffStats,
    ) -> CandidateRecord {
        CandidateRecord {
            id: label.to_lowercase(),
            label: label.to_owned(),
            harness_id: "fake".into(),
            harness_version: None,
            model: None,
            status: CandidateStatus::Completed,
            workspace_path: workspace.clone(),
            prompt_path: workspace.join("prompt.txt"),
            stdout_path: workspace.join("stdout.log"),
            stderr_path: workspace.join("stderr.log"),
            diff_path: diff,
            duration_ms: 1,
            exit_code: Some(0),
            timed_out: false,
            tokens: None,
            token_semantics: None,
            cost_usd: None,
            error: None,
            diff_stats: stats,
            checks: Vec::new(),
        }
    }

    fn run_record(
        source: &Path,
        snapshot: &SourceSnapshot,
        candidate: CandidateRecord,
    ) -> RunRecord {
        RunRecord {
            execution: None,
            id: "test-run".into(),
            task: "test task".into(),
            exact_prompt: "test prompt".into(),
            source_path: source.to_path_buf(),
            source_kind: snapshot.kind.clone(),
            source_git_head: snapshot.git_head.clone(),
            source_fingerprint: snapshot.fingerprint.clone(),
            baseline_path: snapshot.baseline_path.clone(),
            baseline_commit: snapshot.baseline_commit.clone(),
            status: RunStatus::Evaluated,
            mode: crate::RunMode::Native,
            state_revision: 0,
            outcome: crate::RunOutcome::default(),
            created_at: Utc::now(),
            completed_at: None,
            environment: EnvironmentRecord {
                dispatch_version: "test".into(),
                os: "test".into(),
                architecture: "test".into(),
                execution_backend: "local".into(),
                timeout_secs: 1,
                cpus: 1.0,
                memory: "1g".into(),
                max_parallel: 1,
                docker_image: None,
                resource_limits_enforced: false,
                unsafe_local: false,
                forwarded_env: Vec::new(),
            },
            baseline_checks: Vec::new(),
            candidates: vec![candidate],
            attempts: Vec::new(),
            allocation: None,
            coherence: None,
            applied_candidate: None,
            attachment: None,
            historical: Default::default(),
        }
    }

    #[test]
    fn resolves_and_validates_source_directories() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        assert_eq!(
            resolve_source(Some(&source)).unwrap(),
            fs::canonicalize(&source).unwrap()
        );

        let file = temp.path().join("file");
        write(&file, "not a directory");
        assert!(resolve_source(Some(&file)).is_err());
    }

    #[test]
    fn identifies_plain_git_and_linked_worktree_sources() {
        let temp = TempDir::new().unwrap();
        let plain = temp.path().join("plain");
        fs::create_dir(&plain).unwrap();
        write(&plain.join("file.txt"), "plain\n");
        assert_eq!(
            inspect_source(&plain).unwrap(),
            (SourceKind::Directory, None)
        );

        let repository = temp.path().join("repository");
        fs::create_dir(&repository).unwrap();
        write(&repository.join("file.txt"), "git\n");
        initialize_user_repository(&repository);
        let expected_head = run_git(&repository, &["rev-parse", "HEAD"]);
        assert_eq!(
            inspect_source(&repository).unwrap(),
            (SourceKind::Git, Some(expected_head))
        );

        let worktree = temp.path().join("worktree");
        run_git(
            &repository,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "dispatch-source-test",
                worktree.to_str().unwrap(),
            ],
        );
        assert_eq!(
            inspect_source(&worktree).unwrap().0,
            SourceKind::GitWorktree
        );
    }

    #[test]
    fn snapshot_is_exact_private_and_excludes_state() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let run = temp.path().join("run");
        fs::create_dir(&source).unwrap();
        write(&source.join("src/main.rs"), "fn main() {}\n");
        write(&source.join("empty/.keep"), "");
        fs::create_dir_all(source.join("actually-empty")).unwrap();
        write(&source.join(".git/private"), "do not copy");
        write(&source.join(".dispatch/state"), "do not copy");

        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let script = source.join("script.sh");
            write(&script, "#!/bin/sh\n");
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            symlink("src/main.rs", source.join("main-link")).unwrap();
        }

        let original_fingerprint = fingerprint_tree(&source).unwrap();
        let snapshot = create_snapshot(&source, &run).unwrap();
        assert_eq!(snapshot.kind, SourceKind::Directory);
        assert_eq!(snapshot.fingerprint, original_fingerprint);
        assert_eq!(fingerprint_tree(&source).unwrap(), original_fingerprint);
        assert_eq!(
            fingerprint_tree(&snapshot.baseline_path).unwrap(),
            original_fingerprint
        );
        assert!(!snapshot.baseline_path.join(".git/private").exists());
        assert!(!snapshot.baseline_path.join(".dispatch").exists());
        assert!(snapshot.baseline_path.join(".git").is_dir());
        assert!(snapshot.baseline_path.join("actually-empty").is_dir());
        assert_eq!(
            run_git(&snapshot.baseline_path, &["status", "--porcelain"]),
            ""
        );
        assert_eq!(snapshot.baseline_commit.len(), 40);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(snapshot.baseline_path.join("script.sh"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0o111
            );
            assert_eq!(
                fs::read_link(snapshot.baseline_path.join("main-link")).unwrap(),
                Path::new("src/main.rs")
            );
        }
    }

    #[test]
    fn rejects_a_run_directory_nested_in_the_source() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("file.txt"), "source\n");
        let nested_run = source.join("state/runs/test");

        let error = create_snapshot(&source, &nested_run)
            .unwrap_err()
            .to_string();
        assert!(error.contains("outside the source tree"));
        assert!(!nested_run.exists());
    }

    #[test]
    fn snapshot_of_git_source_uses_dirty_worktree_without_touching_original() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("tracked.txt"), "committed\n");
        initialize_user_repository(&source);
        let head = run_git(&source, &["rev-parse", "HEAD"]);
        write(&source.join("tracked.txt"), "dirty baseline\n");
        write(&source.join("untracked.txt"), "also baseline\n");
        let status_before = run_git(&source, &["status", "--porcelain"]);

        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        assert_eq!(snapshot.kind, SourceKind::Git);
        assert_eq!(snapshot.git_head.as_deref(), Some(head.as_str()));
        assert_eq!(
            fs::read_to_string(snapshot.baseline_path.join("tracked.txt")).unwrap(),
            "dirty baseline\n"
        );
        assert_eq!(
            fs::read_to_string(snapshot.baseline_path.join("untracked.txt")).unwrap(),
            "also baseline\n"
        );
        assert_eq!(run_git(&source, &["status", "--porcelain"]), status_before);
    }

    #[test]
    fn internal_git_keeps_literal_bytes_despite_source_attributes() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join(".gitattributes"), "*.txt text\n");
        write(&source.join("line-endings.txt"), b"one\r\ntwo\r\n");

        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        assert_eq!(
            fs::read(snapshot.baseline_path.join("line-endings.txt")).unwrap(),
            b"one\r\ntwo\r\n"
        );
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        assert_eq!(
            fs::read(workspace.join("line-endings.txt")).unwrap(),
            b"one\r\ntwo\r\n"
        );

        write(&workspace.join("line-endings.txt"), b"one\r\nchanged\r\n");
        let diff_path = temp.path().join("candidate.patch");
        let stats = collect_diff(&snapshot.baseline_path, &workspace, &diff_path).unwrap();
        let run = run_record(
            &source,
            &snapshot,
            candidate_record("A", workspace, diff_path, stats),
        );
        safe_apply(&run, "A").unwrap();
        assert_eq!(
            fs::read(source.join("line-endings.txt")).unwrap(),
            b"one\r\nchanged\r\n"
        );
    }

    #[test]
    fn diff_collection_ignores_candidate_controlled_git_configuration() {
        use std::io::Write as _;

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("tracked.txt"), "baseline\n");
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();

        let sentinel = temp.path().join("candidate-config-executed");
        let mut config = fs::OpenOptions::new()
            .append(true)
            .open(workspace.join(".git/config"))
            .unwrap();
        writeln!(
            config,
            "[filter \"dispatch-owned\"]\n\tclean = touch {}\n\tsmudge = cat\n\trequired = true",
            sentinel.display()
        )
        .unwrap();
        fs::write(
            workspace.join(".git/info/attributes"),
            "*.txt filter=dispatch-owned\n",
        )
        .unwrap();
        write(&workspace.join("tracked.txt"), "candidate\n");

        let diff_path = temp.path().join("candidate.patch");
        let stats = collect_diff(&snapshot.baseline_path, &workspace, &diff_path).unwrap();
        assert_eq!(stats.files_changed, 1);
        assert!(!sentinel.exists());
        assert!(
            fs::read_to_string(diff_path)
                .unwrap()
                .contains("+candidate")
        );
    }

    #[test]
    fn candidate_and_diff_include_untracked_text_binary_and_mode_changes() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("tracked.txt"), "one\ntwo\n");
        write(&source.join(".gitignore"), "ignored.log\n");
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let candidate = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &candidate).unwrap();

        write(&candidate.join("tracked.txt"), "one\nchanged\nthree\n");
        write(&candidate.join("new.txt"), "new line\n");
        write(&candidate.join("image.bin"), [0_u8, 1, 2, 0, 255]);
        write(&candidate.join("ignored.log"), "build output\n");
        write(&candidate.join(".dispatch/internal"), "state\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let executable = candidate.join("new.txt");
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
            symlink("new.txt", candidate.join("new-link")).unwrap();
        }

        let diff_path = temp.path().join("candidate.patch");
        let stats = collect_diff(&snapshot.baseline_path, &candidate, &diff_path).unwrap();
        let expected_files = if cfg!(unix) { 4 } else { 3 };
        assert_eq!(stats.files_changed, expected_files);
        assert!(stats.lines_added >= 3);
        assert!(stats.lines_removed >= 1);
        assert_eq!(
            stats.untracked_files,
            if cfg!(unix) {
                vec!["image.bin", "new-link", "new.txt"]
            } else {
                vec!["image.bin", "new.txt"]
            }
        );
        let patch = fs::read(&diff_path).unwrap();
        assert!(
            patch
                .windows(b"GIT binary patch".len())
                .any(|w| w == b"GIT binary patch")
        );
        assert!(!String::from_utf8_lossy(&patch).contains("ignored.log"));
        assert!(!String::from_utf8_lossy(&patch).contains(".dispatch"));
        assert_eq!(fingerprint_tree(&source).unwrap(), snapshot.fingerprint);
        assert_eq!(
            run_git(&snapshot.baseline_path, &["status", "--porcelain"]),
            ""
        );
    }

    #[test]
    fn safe_apply_updates_a_plain_source_only_when_it_has_not_drifted() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("old.txt"), "before\n");
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        write(&workspace.join("old.txt"), "after\n");
        write(&workspace.join("new.txt"), "created\n");
        let diff_path = temp.path().join("candidate.patch");
        let stats = collect_diff(&snapshot.baseline_path, &workspace, &diff_path).unwrap();
        let run = run_record(
            &source,
            &snapshot,
            candidate_record("A", workspace, diff_path, stats),
        );

        let report = safe_apply(&run, "A").unwrap();
        assert_eq!(report.candidate_label, "A");
        assert_eq!(report.files_changed, 2);
        assert_eq!(
            fs::read_to_string(source.join("old.txt")).unwrap(),
            "after\n"
        );
        assert_eq!(
            fs::read_to_string(source.join("new.txt")).unwrap(),
            "created\n"
        );
    }

    #[test]
    fn safe_apply_rejects_source_drift_without_partial_changes() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("one.txt"), "baseline one\n");
        write(&source.join("two.txt"), "baseline two\n");
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        write(&workspace.join("one.txt"), "candidate one\n");
        write(&workspace.join("two.txt"), "candidate two\n");
        let diff_path = temp.path().join("candidate.patch");
        let stats = collect_diff(&snapshot.baseline_path, &workspace, &diff_path).unwrap();
        let run = run_record(
            &source,
            &snapshot,
            candidate_record("B", workspace, diff_path, stats),
        );

        write(&source.join("two.txt"), "user edit\n");
        let error = safe_apply(&run, "B").unwrap_err().to_string();
        assert!(error.contains("source has changed"));
        assert_eq!(
            fs::read_to_string(source.join("one.txt")).unwrap(),
            "baseline one\n"
        );
        assert_eq!(
            fs::read_to_string(source.join("two.txt")).unwrap(),
            "user edit\n"
        );
    }

    #[test]
    fn git_apply_check_prevents_partial_application_of_a_conflicting_patch() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("one.txt"), "baseline one\n");
        write(&source.join("two.txt"), "baseline two\n");
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        write(&workspace.join("one.txt"), "candidate one\n");
        write(&workspace.join("two.txt"), "candidate two\n");
        let diff_path = temp.path().join("candidate.patch");
        let stats = collect_diff(&snapshot.baseline_path, &workspace, &diff_path).unwrap();
        let mut run = run_record(
            &source,
            &snapshot,
            candidate_record("C", workspace, diff_path, stats),
        );

        // Simulate a run record whose content fingerprint matches a different
        // baseline. This gets past the drift guard and directly exercises the
        // all-files apply check.
        write(&source.join("two.txt"), "incompatible contents\n");
        run.source_fingerprint = fingerprint_tree(&source).unwrap();
        let error = safe_apply(&run, "C").unwrap_err().to_string();
        assert!(error.contains("cannot be applied cleanly"));
        assert_eq!(
            fs::read_to_string(source.join("one.txt")).unwrap(),
            "baseline one\n"
        );
        assert_eq!(
            fs::read_to_string(source.join("two.txt")).unwrap(),
            "incompatible contents\n"
        );
    }

    #[test]
    fn fingerprints_ignore_state_but_detect_content_symlinks_and_execute_bits() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("file"), "value");
        let initial = fingerprint_tree(&source).unwrap();
        write(&source.join(".dispatch/state"), "ignored");
        write(&source.join(".git/index"), "ignored");
        assert_eq!(fingerprint_tree(&source).unwrap(), initial);

        write(&source.join("file"), "changed");
        assert_ne!(fingerprint_tree(&source).unwrap(), initial);

        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            write(&source.join("file"), "value");
            let normal = fingerprint_tree(&source).unwrap();
            fs::set_permissions(source.join("file"), fs::Permissions::from_mode(0o755)).unwrap();
            assert_ne!(fingerprint_tree(&source).unwrap(), normal);

            symlink("file", source.join("link")).unwrap();
            let first_link = fingerprint_tree(&source).unwrap();
            fs::remove_file(source.join("link")).unwrap();
            symlink("other", source.join("link")).unwrap();
            assert_ne!(fingerprint_tree(&source).unwrap(), first_link);
        }
    }

    #[test]
    fn an_entry_deleted_mid_walk_is_a_difference_not_an_error() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        write(&source.join("src/lib.rs"), "pub fn f() {}\n");
        write(&source.join("target/debug/a.o"), "object\n");
        write(&source.join("target/debug/b.o"), "object\n");
        let root = resolve_source(Some(&source)).unwrap();
        let before = fingerprint_tree(&root).unwrap();

        // Listed while present, gone by the time it is read.
        let listed = list_tree(&root).unwrap();
        fs::remove_file(root.join("target/debug/a.o")).unwrap();
        let moving = hash_tree(&root, listed).unwrap();
        let after = fingerprint_tree(&root).unwrap();

        assert_ne!(moving, before);
        assert_ne!(moving, after);
        assert_ne!(before, after);
    }

    #[test]
    fn world_commit_never_accepts_a_pair_rewritten_during_capture() {
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        };

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let staging = temp.path().join("staging");
        fs::create_dir(&staging).unwrap();
        let generation = |value: u64| format!("generation {value}\n").repeat(10_000);
        write(&source.join("a.txt"), generation(0));
        write(&source.join("b.txt"), generation(0));
        initialize_user_repository(&source);

        // The writer replaces a.txt, then b.txt, each whole by rename, so the
        // pair is mixed only between the two renames. It records each such
        // window: if it is descheduled there, the workspace itself holds the
        // mixed pair for that long, and a capture inside the window is honest.
        let stop = Arc::new(AtomicBool::new(false));
        let windows = Arc::new(Mutex::new(Vec::new()));
        let writer = thread::spawn({
            let (stop, windows) = (Arc::clone(&stop), Arc::clone(&windows));
            let source = source.clone();
            move || {
                let mut value = 0;
                while !stop.load(Ordering::Relaxed) {
                    value += 1;
                    for file in ["a.txt", "b.txt"] {
                        fs::write(staging.join(file), generation(value)).unwrap();
                    }
                    fs::rename(staging.join("a.txt"), source.join("a.txt")).unwrap();
                    let opened = Instant::now();
                    let closed = Instant::now();
                    fs::rename(staging.join("b.txt"), source.join("b.txt")).unwrap();
                    windows.lock().unwrap().push((opened, closed));
                    // Quiet now and then for long enough to capture.
                    let pause = if value % 4 == 0 { 200 } else { value % 3 };
                    thread::sleep(Duration::from_millis(pause));
                }
                value
            }
        });

        let mut accepted = Vec::new();
        let mut refused = 0;
        for _ in 0..40 {
            let started = Instant::now();
            let result = world_commit(&source);
            let finished = Instant::now();
            match result {
                Ok(commit) => accepted.push((commit, started, finished)),
                Err(error) => {
                    assert!(
                        error.to_string().contains(
                            "the workspace changed while its starting state was being captured"
                        ),
                        "{error:#}"
                    );
                    refused += 1;
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        let last = writer.join().unwrap();
        let windows = windows.lock().unwrap();

        // A file's generation, which must also be the whole of it.
        let read = |commit: &str, file: &str| -> u64 {
            let text = run_git(&source, &["show", &format!("{commit}:{file}")]);
            let value = text
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("generation "))
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("{file} was captured torn"));
            assert_eq!(
                text,
                generation(value).trim_end(),
                "{file} was captured torn"
            );
            value
        };
        let mut stalled = 0;
        for (commit, started, finished) in &accepted {
            let (a, b) = (read(commit, "a.txt"), read(commit, "b.txt"));
            if a == b {
                continue;
            }
            let held_throughout = a == b + 1 && {
                let (opened, closed) = windows[a as usize - 1];
                opened <= *started && *finished <= closed
            };
            assert!(held_throughout, "a mixed pair was accepted: a {a}, b {b}");
            stalled += 1;
        }
        eprintln!(
            "accepted {} ({stalled} inside a stalled writer's window), refused {refused}",
            accepted.len()
        );

        // Quiet again: the capture is the last generation in both files.
        let commit = world_commit(&source).unwrap();
        assert_eq!(
            (read(&commit, "a.txt"), read(&commit, "b.txt")),
            (last, last)
        );
    }

    #[test]
    fn a_capture_that_moved_is_made_once_more_then_refused() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        write(&source.join("a.txt"), "0\n");
        initialize_user_repository(&source);
        let source = resolve_source(Some(&source)).unwrap();

        // Moved during the first capture only: the second is accepted.
        let mut captures = 0;
        let commit = capture_settled(&source, || {
            captures += 1;
            let commit = capture_world(&source)?;
            if captures == 1 {
                write(&source.join("a.txt"), "moved\n");
            }
            Ok(commit)
        })
        .unwrap();
        assert_eq!(captures, 2);
        assert_eq!(
            run_git(&source, &["show", &format!("{commit}:a.txt")]),
            "moved"
        );

        // Moved during both: refused, and no third capture.
        let mut captures = 0;
        let error = capture_settled(&source, || {
            captures += 1;
            let commit = capture_world(&source)?;
            write(&source.join("a.txt"), "moved\n".repeat(captures + 1));
            Ok(commit)
        })
        .unwrap_err();
        assert_eq!(captures, 2);
        assert!(
            error
                .to_string()
                .contains("the workspace changed while its starting state was being captured"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_that_escape_candidate_isolation() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        symlink("../../outside", source.join("nested/escape")).unwrap();

        let error = fingerprint_tree(&source).unwrap_err().to_string();
        assert!(error.contains("escapes candidate isolation"));

        fs::remove_file(source.join("nested/escape")).unwrap();
        symlink("/tmp/outside", source.join("absolute")).unwrap();
        let error = fingerprint_tree(&source).unwrap_err().to_string();
        assert!(error.contains("absolute symlink"));
    }

    #[test]
    fn world_commit_records_exactly_the_world_and_leaves_the_checkout_alone() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join(".gitignore"), "target/\n");
        write(&source.join("a.txt"), "committed\n");
        write(&source.join("b.txt"), "to be deleted\n");
        write(&source.join("src/keep.rs"), "pub fn keep() {}\n");
        initialize_user_repository(&source);
        let head = run_git(&source, &["rev-parse", "HEAD"]);

        write(&source.join("a.txt"), "dirty\n");
        fs::remove_file(source.join("b.txt")).unwrap();
        write(&source.join("new.txt"), "untracked\n");
        write(&source.join("target/out.bin"), "build output\n");
        let nested = source.join("vendor/dep");
        write(&nested.join("lib.rs"), "nested\n");
        run_git(&nested, &["init", "--quiet"]);
        let status_before = run_git(&source, &["status", "--porcelain"]);
        let index_before = fs::read(source.join(".git/index")).unwrap();

        let commit = world_commit(&source).unwrap();

        assert_eq!(run_git(&source, &["status", "--porcelain"]), status_before);
        assert_eq!(fs::read(source.join(".git/index")).unwrap(), index_before);
        assert_eq!(
            run_git(&source, &["rev-parse", &format!("{commit}^")]),
            head
        );
        let mut files: Vec<String> = run_git(&source, &["ls-tree", "-r", "--name-only", &commit])
            .lines()
            .map(str::to_owned)
            .collect();
        files.sort();
        assert_eq!(files, [".gitignore", "a.txt", "new.txt", "src/keep.rs"]);

        let baseline = materialize_baseline_from_commit(&source, &commit, &temp.path().join("run"))
            .unwrap()
            .baseline_path;
        assert_eq!(
            fs::read_to_string(baseline.join("a.txt")).unwrap(),
            "dirty\n"
        );
        assert!(!baseline.join("b.txt").exists() && !baseline.join("target").exists());

        // A repository with no commit yet has a world too.
        let fresh = temp.path().join("fresh");
        fs::create_dir(&fresh).unwrap();
        run_git(&fresh, &["init", "--quiet"]);
        write(&fresh.join("first.txt"), "first\n");
        let commit = world_commit(&fresh).unwrap();
        assert_eq!(
            run_git(&fresh, &["ls-tree", "-r", "--name-only", &commit]),
            "first.txt"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ignored_build_output_never_limits_or_refuses_the_delta() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join(".gitignore"), "target/\nnode_modules/\n");
        write(&source.join("src/lib.rs"), "pub fn f() {}\n");
        initialize_user_repository(&source);
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        let diff = temp.path().join("delta.patch");
        let collect = || collect_diff(&snapshot.baseline_path, &workspace, &diff);

        // What a build and an install leave behind: a file over the per-file
        // limit (sparse, so it costs nothing) and links out of the tree.
        fs::create_dir_all(workspace.join("target")).unwrap();
        fs::File::create(workspace.join("target/huge.bin"))
            .unwrap()
            .set_len(MAX_CANDIDATE_FILE_BYTES + 1)
            .unwrap();
        fs::create_dir_all(workspace.join("node_modules/.bin")).unwrap();
        symlink("/usr/bin/env", workspace.join("node_modules/.bin/env")).unwrap();
        write(&workspace.join("src/lib.rs"), "pub fn f() { 1 }\n");
        assert_eq!(collect().unwrap().changed_files, vec!["src/lib.rs"]);

        // Whatever can enter the delta is still held to the same limits.
        symlink("/usr/bin/env", workspace.join("src/escape")).unwrap();
        let error = collect().unwrap_err().to_string();
        assert!(error.contains("absolute symlink"), "{error}");
        fs::remove_file(workspace.join("src/escape")).unwrap();

        fs::File::create(workspace.join("src/huge.bin"))
            .unwrap()
            .set_len(MAX_CANDIDATE_FILE_BYTES + 1)
            .unwrap();
        let error = collect().unwrap_err().to_string();
        assert!(error.contains("safety limit"), "{error}");
        fs::remove_file(workspace.join("src/huge.bin")).unwrap();

        // A tracked directory replaced by a link out of the tree.
        fs::remove_dir_all(workspace.join("src")).unwrap();
        symlink("/tmp", workspace.join("src")).unwrap();
        let error = collect().unwrap_err().to_string();
        assert!(error.contains("absolute symlink"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn scratch_tree_copies_listed_files_only_and_never_follows_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        write(&source.join(".gitignore"), "target/\n");
        write(&source.join("src/lib.rs"), "pub fn f() {}\n");
        write(&source.join("gone.txt"), "deleted later\n");
        write(&source.join("run.sh"), "#!/bin/sh\n");
        fs::set_permissions(source.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("run.sh", source.join("link")).unwrap();
        initialize_user_repository(&source);
        fs::remove_file(source.join("gone.txt")).unwrap();
        write(&source.join("target/out.bin"), "ignored\n");
        write(&source.join("untracked.txt"), "u\n");
        write(&source.join("dep/lib.rs"), "nested\n");
        run_git(&source.join("dep"), &["init", "--quiet"]);

        let scratch = temp.path().join("scratch");
        create_scratch_tree(&source, &SourceKind::Git, &scratch).unwrap();

        assert!(!scratch.join("target").exists() && !scratch.join("gone.txt").exists());
        assert!(!scratch.join(".git").exists() && !scratch.join("dep/.git").exists());
        assert_eq!(
            fs::read_to_string(scratch.join("untracked.txt")).unwrap(),
            "u\n"
        );
        assert!(scratch.join("dep/lib.rs").is_file());
        assert_eq!(
            fs::metadata(scratch.join("run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0o111
        );
        assert_eq!(
            fs::read_link(scratch.join("link")).unwrap(),
            Path::new("run.sh")
        );

        // A symlink that leaves the tree is refused, and a parent swapped for a
        // symlink is not read through.
        symlink("../outside", source.join("escape")).unwrap();
        let error = create_scratch_tree(&source, &SourceKind::Git, &temp.path().join("s2"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("escapes candidate isolation"), "{error}");
        fs::remove_file(source.join("escape")).unwrap();
        fs::remove_dir_all(source.join("src")).unwrap();
        symlink("dep", source.join("src")).unwrap();
        let scratch = temp.path().join("s3");
        create_scratch_tree(&source, &SourceKind::Git, &scratch).unwrap();
        assert!(
            fs::symlink_metadata(scratch.join("src"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn refuses_oversized_sparse_candidate_files_before_git() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let run_dir = temp.path().join("run");
        fs::create_dir(&source).unwrap();
        write(&source.join("small.txt"), "baseline\n");
        let snapshot = create_snapshot(&source, &run_dir).unwrap();
        let candidate = create_candidate_workspace(
            &snapshot.baseline_path,
            &run_dir.join("candidate/workspace"),
        )
        .unwrap();
        let sparse = File::create(candidate.join("huge.bin")).unwrap();
        sparse.set_len(MAX_CANDIDATE_FILE_BYTES + 1).unwrap();

        let error = collect_diff(
            &snapshot.baseline_path,
            &candidate,
            &run_dir.join("candidate/diff.patch"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("safety limit"));
    }

    #[test]
    fn git_snapshot_does_not_track_ignored_files() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join(".gitignore"), "build/\n");
        write(&source.join("src/lib.rs"), "pub fn f() {}\n");
        initialize_user_repository(&source);
        write(&source.join("build/out.bin"), "stale build output\n");

        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        assert_eq!(snapshot.kind, SourceKind::Git);

        let tracked = run_git(
            &snapshot.baseline_path,
            &["ls-tree", "-r", "--name-only", "HEAD"],
        );
        let tracked: Vec<&str> = tracked.lines().collect();
        assert!(tracked.contains(&".gitignore"));
        assert!(tracked.contains(&"src/lib.rs"));
        assert!(!tracked.iter().any(|path| path.starts_with("build/")));

        // The ignored file is still present on disk in the baseline directory,
        // so a candidate workspace inherits it as a build cache.
        assert_eq!(
            fs::read_to_string(snapshot.baseline_path.join("build/out.bin")).unwrap(),
            "stale build output\n"
        );

        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        assert_eq!(
            fs::read_to_string(workspace.join("build/out.bin")).unwrap(),
            "stale build output\n"
        );
        assert_eq!(
            fingerprint_tree(&snapshot.baseline_path).unwrap(),
            fingerprint_tree(&workspace).unwrap()
        );
    }

    #[test]
    fn regenerated_ignored_artifact_is_not_part_of_the_delta() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join(".gitignore"), "build/\n");
        write(&source.join("src/lib.rs"), "pub fn f() {}\n");
        initialize_user_repository(&source);
        write(&source.join("build/out.bin"), "stale build output\n");

        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();

        // A verify check regenerates the build artifact and the agent edits a
        // tracked source file; only the tracked edit should reach Δ.
        write(&workspace.join("build/out.bin"), "freshly rebuilt output\n");
        write(&workspace.join("src/lib.rs"), "pub fn f() { 1 }\n");

        let diff_path = temp.path().join("candidate.patch");
        let stats = collect_diff(&snapshot.baseline_path, &workspace, &diff_path).unwrap();
        assert_eq!(stats.files_changed, 1);
        assert_eq!(stats.changed_files, vec!["src/lib.rs".to_string()]);
        let patch = fs::read_to_string(&diff_path).unwrap();
        assert!(patch.contains("src/lib.rs"));
        assert!(!patch.contains("build/"));

        let delta_path = temp.path().join("delta.patch");
        snapshot_delta(&snapshot.baseline_path, &workspace, &delta_path).unwrap();
        let delta = fs::read_to_string(&delta_path).unwrap();
        assert!(delta.contains("src/lib.rs"));
        assert!(!delta.contains("build/"));
    }

    #[test]
    fn a_kept_index_snapshots_exactly_what_a_fresh_one_does() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("src/lib.rs"), "pub fn f() {}\n");
        write(&source.join("README.md"), "readme\n");
        initialize_user_repository(&source);
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        let index = temp.path().join("kept.index");
        let (kept, fresh) = (
            temp.path().join("kept.patch"),
            temp.path().join("fresh.patch"),
        );
        let same = || {
            snapshot_delta_indexed(&snapshot.baseline_path, &workspace, &kept, &index).unwrap();
            snapshot_delta(&snapshot.baseline_path, &workspace, &fresh).unwrap();
            let kept = fs::read_to_string(&kept).unwrap();
            assert_eq!(kept, fs::read_to_string(&fresh).unwrap());
            kept
        };

        assert!(same().is_empty());
        write(&workspace.join("src/lib.rs"), "pub fn f() { 1 }\n");
        write(&workspace.join("new.txt"), "new\n");
        assert!(same().contains("new.txt"));
        // Same size, rewritten: the stat cache must not hide it.
        write(&workspace.join("src/lib.rs"), "pub fn f() { 2 }\n");
        assert!(same().contains("{ 2 }"));
        // Reverted and deleted work leaves the patch.
        write(&workspace.join("src/lib.rs"), "pub fn f() {}\n");
        fs::remove_file(workspace.join("new.txt")).unwrap();
        fs::remove_file(workspace.join("README.md")).unwrap();
        let patch = same();
        assert!(!patch.contains("src/lib.rs") && !patch.contains("new.txt"));
        assert!(patch.contains("deleted file mode"));
    }

    #[test]
    fn a_live_patch_is_never_seen_half_written() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("README.md"), "readme\n");
        initialize_user_repository(&source);
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        // Two workspaces whose patches are large and different, each with its
        // own kept index, published alternately into one live patch.
        let workspaces = ["a", "b"].map(|name| {
            let workspace = temp.path().join(name);
            create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
            let lines = (0..80_000).map(|line| format!("{name} line {line}\n"));
            write(&workspace.join("big.txt"), lines.collect::<String>());
            let index = temp.path().join(format!("{name}.index"));
            let patch = temp.path().join(format!("{name}.patch"));
            snapshot_delta_indexed(&snapshot.baseline_path, &workspace, &patch, &index).unwrap();
            (workspace, index, fs::read(&patch).unwrap())
        });
        assert!(
            workspaces
                .iter()
                .all(|(_, _, patch)| patch.len() >= 1 << 20)
        );
        let live = temp.path().join("live/delta-live.patch");
        write(&live, &workspaces[0].2);

        let done = AtomicBool::new(false);
        let (reads, torn) = thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let (mut reads, mut torn) = (0, 0);
                while !done.load(Ordering::Relaxed) {
                    let Ok(seen) = fs::read(&live) else { continue };
                    reads += 1;
                    if workspaces.iter().all(|(_, _, patch)| *patch != seen) {
                        torn += 1;
                    }
                }
                (reads, torn)
            });
            for (workspace, index, _) in workspaces.iter().cycle().take(60) {
                snapshot_delta_indexed(&snapshot.baseline_path, workspace, &live, index).unwrap();
            }
            done.store(true, Ordering::Relaxed);
            reader.join().unwrap()
        });
        assert!(reads > 0);
        assert_eq!(torn, 0, "{torn} of {reads} reads saw a partial patch");
    }

    #[test]
    fn a_failed_live_snapshot_keeps_the_last_patch() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("README.md"), "readme\n");
        initialize_user_repository(&source);
        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let live = temp.path().join("live/delta-live.patch");
        write(&live, "last patch\n");

        let missing = temp.path().join("missing");
        let index = temp.path().join("missing.index");
        assert!(snapshot_delta_indexed(&snapshot.baseline_path, &missing, &live, &index).is_err());
        assert!(snapshot_delta(&snapshot.baseline_path, &missing, &live).is_err());

        assert_eq!(fs::read_to_string(&live).unwrap(), "last patch\n");
        let names = |directory: &Path| {
            fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(live.parent().unwrap()), ["delta-live.patch"]);

        // A publish that fails after staging (here the rename, onto a
        // directory) leaves no temporary file behind either.
        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();
        write(&workspace.join("new.txt"), "new\n");
        let blocked = temp.path().join("blocked/delta-live.patch");
        fs::create_dir_all(blocked.join("kept")).unwrap();
        assert!(snapshot_delta(&snapshot.baseline_path, &workspace, &blocked).is_err());
        assert_eq!(names(blocked.parent().unwrap()), ["delta-live.patch"]);
        assert_eq!(names(&blocked), ["kept"]);
    }

    #[test]
    fn info_exclude_is_honored() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join("src/lib.rs"), "pub fn f() {}\n");
        initialize_user_repository(&source);
        write(&source.join(".git/info/exclude"), "scratch.txt\n");
        write(&source.join("scratch.txt"), "local scratch notes\n");

        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        let tracked = run_git(
            &snapshot.baseline_path,
            &["ls-tree", "-r", "--name-only", "HEAD"],
        );
        let tracked: Vec<&str> = tracked.lines().collect();
        assert!(tracked.contains(&"src/lib.rs"));
        assert!(!tracked.contains(&"scratch.txt"));
        assert!(snapshot.baseline_path.join("scratch.txt").exists());
    }

    #[test]
    fn directory_snapshot_still_tracks_everything() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write(&source.join(".gitignore"), "build/\n");
        write(&source.join("src/lib.rs"), "pub fn f() {}\n");
        write(&source.join("build/out.bin"), "stale build output\n");

        let snapshot = create_snapshot(&source, &temp.path().join("run")).unwrap();
        assert_eq!(snapshot.kind, SourceKind::Directory);
        let tracked = run_git(
            &snapshot.baseline_path,
            &["ls-tree", "-r", "--name-only", "HEAD"],
        );
        let tracked: Vec<&str> = tracked.lines().collect();
        assert!(tracked.contains(&"build/out.bin"));
    }

    #[test]
    fn repo_identity_of_a_linked_worktree_names_the_main_worktree() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        write(&repo.join("file.txt"), "content\n");
        initialize_user_repository(&repo);

        let worktree = temp.path().join("worktree");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "dispatch-identity-test",
                worktree.to_str().unwrap(),
            ],
        );

        let from_root = repo_identity(&repo).unwrap().unwrap();
        let from_worktree = repo_identity(&worktree).unwrap().unwrap();
        let canonical_repo = fs::canonicalize(&repo).unwrap();
        assert_eq!(from_root.key, from_worktree.key);
        assert_eq!(from_root.main_worktree, canonical_repo);
        assert_eq!(from_worktree.main_worktree, canonical_repo);
        assert_eq!(from_root.common_dir, from_worktree.common_dir);

        let plain = temp.path().join("plain");
        fs::create_dir(&plain).unwrap();
        assert!(repo_identity(&plain).unwrap().is_none());
    }

    #[test]
    fn merge_base_finds_the_fork_point_and_none_without_history() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        write(&repo.join("file.txt"), "one\n");
        initialize_user_repository(&repo);
        let fork_commit = run_git(&repo, &["rev-parse", "HEAD"]);

        let workspace = temp.path().join("workspace");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "dispatch-merge-base-test",
                workspace.to_str().unwrap(),
            ],
        );

        // Diverge both sides from the fork point.
        write(&repo.join("file.txt"), "one\nroot change\n");
        run_git(&repo, &["add", "-A"]);
        run_git(&repo, &["commit", "--quiet", "-m", "root change"]);

        write(&workspace.join("workspace.txt"), "workspace change\n");
        run_git(&workspace, &["add", "-A"]);
        run_git(&workspace, &["commit", "--quiet", "-m", "workspace change"]);

        assert_eq!(merge_base(&repo, &workspace).unwrap(), Some(fork_commit));

        // An unrelated repository is refused rather than compared.
        let unrelated = temp.path().join("unrelated");
        fs::create_dir(&unrelated).unwrap();
        write(&unrelated.join("other.txt"), "other\n");
        initialize_user_repository(&unrelated);
        let error = merge_base(&repo, &unrelated).unwrap_err().to_string();
        assert!(error.contains("different repository"), "{error}");

        // An orphan branch in the workspace shares no history with root.
        run_git(
            &workspace,
            &["checkout", "--quiet", "--orphan", "dispatch-orphan-test"],
        );
        write(&workspace.join("orphan.txt"), "orphan\n");
        run_git(&workspace, &["add", "-A"]);
        run_git(&workspace, &["commit", "--quiet", "-m", "orphan commit"]);
        assert_eq!(merge_base(&repo, &workspace).unwrap(), None);
    }

    #[test]
    fn materialized_baseline_matches_the_commit_tree() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        write(&repo.join("tracked.txt"), "tracked\n");
        write(&repo.join("sub/nested.txt"), "nested\n");
        write(&repo.join(".gitignore"), "build/\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let script = repo.join("run.sh");
            write(&script, "#!/bin/sh\n");
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            symlink("run.sh", repo.join("link.sh")).unwrap();
        }
        initialize_user_repository(&repo);
        let commit = run_git(&repo, &["rev-parse", "HEAD"]);

        let snapshot =
            materialize_baseline_from_commit(&repo, &commit, &temp.path().join("run")).unwrap();
        assert_eq!(snapshot.kind, SourceKind::Git);
        assert_eq!(snapshot.git_head.as_deref(), Some(commit.as_str()));

        let commit_listing = run_git(&repo, &["ls-tree", "-r", &commit]);
        let baseline_listing = run_git(&snapshot.baseline_path, &["ls-tree", "-r", "HEAD"]);
        assert_eq!(commit_listing, baseline_listing);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::read_link(snapshot.baseline_path.join("link.sh")).unwrap(),
                Path::new("run.sh")
            );
            assert_eq!(
                fs::metadata(snapshot.baseline_path.join("run.sh"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0o111
            );
        }

        let workspace = temp.path().join("candidate");
        create_candidate_workspace(&snapshot.baseline_path, &workspace).unwrap();

        let observation = crate::coherence::world::observe(
            &repo,
            &snapshot.baseline_path,
            &snapshot.baseline_commit,
            &SourceKind::Git,
        )
        .unwrap();
        assert!(observation.changes.is_empty(), "{:?}", observation.changes);
    }

    #[test]
    fn delta_from_a_linked_worktree_excludes_git_file_and_ignored_paths() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        write(&root.join("tracked.txt"), "one\n");
        write(&root.join(".gitignore"), "build/\n");
        initialize_user_repository(&root);
        let fork_commit = run_git(&root, &["rev-parse", "HEAD"]);

        let workspace = temp.path().join("workspace");
        run_git(
            &root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "dispatch-delta-test",
                workspace.to_str().unwrap(),
            ],
        );
        assert!(
            workspace.join(".git").is_file(),
            "a linked worktree names its git directory through a file, not a directory"
        );

        // Edits since S0: a tracked-file change, a new untracked file, and
        // ignored build output that must never appear in Δ.
        write(&workspace.join("tracked.txt"), "one\nedited\n");
        write(&workspace.join("new.txt"), "new\n");
        write(&workspace.join("build/out.bin"), "artifact\n");

        let baseline =
            materialize_baseline_from_commit(&root, &fork_commit, &temp.path().join("run"))
                .unwrap();

        let live_patch = temp.path().join("live.patch");
        snapshot_delta(&baseline.baseline_path, &workspace, &live_patch).unwrap();
        let live_text = fs::read_to_string(&live_patch).unwrap();
        assert!(live_text.contains("tracked.txt"));
        assert!(live_text.contains("edited"));
        assert!(!live_text.contains("out.bin"));
        assert!(!live_text.contains(".git"));

        let diff_path = temp.path().join("finish.patch");
        let stats = collect_diff(&baseline.baseline_path, &workspace, &diff_path).unwrap();
        assert_eq!(
            stats.changed_files,
            vec!["new.txt".to_string(), "tracked.txt".to_string()]
        );
        assert_eq!(stats.untracked_files, vec!["new.txt".to_string()]);
        assert!(
            !stats
                .changed_files
                .iter()
                .any(|path| path.contains("build"))
        );
        assert!(!stats.changed_files.iter().any(|path| path.contains(".git")));
        let finish_text = fs::read_to_string(&diff_path).unwrap();
        assert!(!finish_text.contains("out.bin"));
        assert!(!finish_text.contains(".git"));
    }
}
