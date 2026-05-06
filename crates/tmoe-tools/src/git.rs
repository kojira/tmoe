//! Git ヘルパ: worktree 切り出し・コミット・差分取得。
//!
//! tmoe は機能 (feature) ごとに独立した worktree を切り、そこで Worker が編集する。
//! Supervisor は commit 前に diff を読んで自己レビューを行う。

use git2::{IndexAddOption, ObjectType, Repository, Signature, WorktreePruneOptions};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("git2 error: {0}")]
    Git(#[from] git2::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid: {0}")]
    Invalid(String),
}

pub type GitResult<T> = std::result::Result<T, GitError>;

#[derive(Debug, Clone)]
pub struct WorktreeHandle {
    pub repo_path: PathBuf,
    pub worktree_path: PathBuf,
    pub branch_name: String,
    pub worktree_name: String,
}

/// 既存の git リポジトリから feature 用 worktree を切り出す。
/// `worktree_root` は worktree のディレクトリの親を指定する (なければ repo の隣に作る)。
pub fn carve_worktree(
    repo_path: &Path,
    feature_id: &str,
    worktree_root: Option<&Path>,
) -> GitResult<WorktreeHandle> {
    let repo = Repository::open(repo_path)?;
    let head = repo.head()?;
    let head_commit = head.peel_to_commit()?;
    let branch_name = format!("tmoe/feature/{feature_id}");
    let worktree_name = format!("tmoe-feature-{feature_id}");

    if repo.find_branch(&branch_name, git2::BranchType::Local).is_err() {
        repo.branch(&branch_name, &head_commit, false)?;
    }

    let parent = worktree_root
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| {
            repo_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".tmoe-worktrees")
        });
    std::fs::create_dir_all(&parent)?;
    let wt_path = parent.join(&worktree_name);

    if wt_path.exists() {
        return Err(GitError::Invalid(format!(
            "worktree path already exists: {}",
            wt_path.display()
        )));
    }

    let reference = repo.find_reference(&format!("refs/heads/{branch_name}"))?;
    let mut opts = git2::WorktreeAddOptions::new();
    opts.reference(Some(&reference));
    repo.worktree(&worktree_name, &wt_path, Some(&opts))?;

    Ok(WorktreeHandle {
        repo_path: repo_path.to_path_buf(),
        worktree_path: wt_path,
        branch_name,
        worktree_name,
    })
}

pub fn stage_all(handle: &WorktreeHandle) -> GitResult<()> {
    let repo = Repository::open(&handle.worktree_path)?;
    let mut index = repo.index()?;
    index.add_all(["*"].iter(), IndexAddOption::DEFAULT, None)?;
    index.write()?;
    Ok(())
}

pub fn commit(
    handle: &WorktreeHandle,
    author_name: &str,
    author_email: &str,
    message: &str,
) -> GitResult<git2::Oid> {
    let repo = Repository::open(&handle.worktree_path)?;
    let mut index = repo.index()?;
    let tree_oid = index.write_tree()?;
    let tree = repo.find_tree(tree_oid)?;
    let sig = Signature::now(author_name, author_email)?;
    let parent_commit = repo.head()?.peel_to_commit()?;
    let oid = repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        message,
        &tree,
        &[&parent_commit],
    )?;
    Ok(oid)
}

/// HEAD と作業ツリー (index 経由) の差分を patch 形式で取得する。
/// untracked / 変更済み双方を含めるため、内部で `add_all` 相当を index に対して行う
/// (作業ツリーには影響しない)。
pub fn working_diff_text(handle: &WorktreeHandle) -> GitResult<String> {
    let repo = Repository::open(&handle.worktree_path)?;
    let head_tree = repo
        .head()
        .and_then(|h| h.peel(ObjectType::Tree))
        .map(|o| o.into_tree().unwrap())
        .ok();
    // diff 用に index を一時的に full-stage する。書き戻しはしない。
    let mut index = repo.index()?;
    index.add_all(["*"].iter(), IndexAddOption::DEFAULT, None)?;
    let diff = repo.diff_tree_to_index(head_tree.as_ref(), Some(&index), None)?;
    let mut text = String::new();
    diff.print(git2::DiffFormat::Patch, |_d, _h, line| {
        let prefix = match line.origin() {
            '+' | '-' | ' ' => Some(line.origin()),
            _ => None,
        };
        if let Some(p) = prefix {
            text.push(p);
        }
        text.push_str(std::str::from_utf8(line.content()).unwrap_or(""));
        true
    })?;
    Ok(text)
}

pub fn cleanup_worktree(handle: WorktreeHandle) -> GitResult<()> {
    let repo = Repository::open(&handle.repo_path)?;
    let wt = repo.find_worktree(&handle.worktree_name)?;
    let mut prune_opts = WorktreePruneOptions::new();
    prune_opts.valid(true).working_tree(true).locked(true);
    wt.prune(Some(&mut prune_opts))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn init_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("repo");
        fs::create_dir_all(&path).unwrap();
        let repo = Repository::init(&path).unwrap();
        // 初期コミットを作る (worktree carve は HEAD が必要)。
        fs::write(path.join("README.md"), "init\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("README.md")).unwrap();
        let tree_oid = idx.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("tmoe", "tmoe@example").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
        (dir, path)
    }

    #[test]
    fn carve_creates_branch_and_worktree() {
        let (_d, repo_path) = init_repo();
        let handle = carve_worktree(&repo_path, "abc123", None).unwrap();
        assert!(handle.worktree_path.exists());
        assert_eq!(handle.branch_name, "tmoe/feature/abc123");
        let repo = Repository::open(&repo_path).unwrap();
        repo.find_branch(&handle.branch_name, git2::BranchType::Local).unwrap();
    }

    #[test]
    fn diff_then_commit_in_worktree() {
        let (_d, repo_path) = init_repo();
        let handle = carve_worktree(&repo_path, "f1", None).unwrap();
        // worktree 内にファイルを書き込む。
        fs::write(handle.worktree_path.join("hello.rs"), "fn main(){}\n").unwrap();
        let diff = working_diff_text(&handle).unwrap();
        assert!(diff.contains("hello.rs"));
        stage_all(&handle).unwrap();
        let oid = commit(&handle, "tmoe", "tmoe@example", "feat: add hello.rs").unwrap();
        let repo = Repository::open(&handle.worktree_path).unwrap();
        let last = repo.find_commit(oid).unwrap();
        assert_eq!(last.message().unwrap_or(""), "feat: add hello.rs");
    }
}
