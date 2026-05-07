//! tmoe ランタイム: Trio + History + ViewProvider + Tools + Worktree を 1 機能ぶんに結線する。
//!
//! `run_feature` が機能 (Feature) のライフサイクル全体を駆動する:
//!
//! 1. HistoryStore を開き Feature 行を新規作成
//! 2. (オプション) git worktree を切り出し、Worker のツール作業ディレクトリにする
//! 3. ToolRegistry を組み立て (read/edit/run/patch/list/grep/search_source/web_*/run_cmd)
//! 4. OpenAI 互換 LLM クライアントを構築
//! 5. Trio を構成し、HistoryViewProvider を噛ませて run_step_with_views を回す
//!    **複数ラウンドのセッションループ** で動く:
//!      - Commit       → raw 追記 + 3 view 並走逐次コンパクション + (worktree なら) git_commit → 終了
//!      - Redirected   → 受け取った instruction を user メッセージとして追記し再ループ
//!      - Parked       → ThrustReceiver から次の Z 軸推進が来るまで非ブロッキング待機 → 再ループ
//!      - Stopped      → 終了
//!      - Escalated    → 終了
//!    上限は `RunOptions::max_rounds` (既定 4)。redirect の連鎖や park 復帰は同一ラウンド境界
//!    で数える。
//! 6. Worktree を切っていれば、`cleanup_worktree=true` のとき prune して跡片付け。
//! 7. open_pr かつ gh が PATH にあれば `gh pr create --draft`
//!
//! 設計準拠ポイント:
//! - **Trio が ViewProvider 経由で 3 view brief を読む**ので Worker view が読まれる
//! - **逐次コンパクション**: コミット直後に LlmLens で 3 view を 1 ノードずつ延伸
//! - **Concierge は 4 人目ではない**: z_thrust シグナル + Redirect テキストを TUI から runtime
//!   に流す I/O チャネル。Redirect は新しい user メッセージとして Worker へフィードバックされる

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tmoe_core::{
    ConsensusOutcome, ConsensusThresholds, ThrustReceiver, Trio, UserThrust,
};
use tmoe_history::{
    compact_turn_for_all, AgentLens, AgentView, AppendRaw, HistoryStore, HistoryViewProvider,
    LlmLens, RawKind,
};
use tmoe_llm::{ChatMessage, LlmClient, OpenAiCompatClient};
use tmoe_tools::{
    carve_worktree, cleanup_worktree, default_blocklist, git_commit, stage_all, EditFileTool,
    GrepTextTool, ListFilesTool, PatchFileTool, ReadFileTool, RunCmdTool, ToolRegistry,
    WebFetchTool, WebSearchTool, WorktreeHandle,
};

use crate::source_tool::SearchSourceTool;
use tokio::sync::mpsc;

use crate::config::Config;

/// runtime → TUI に流す進捗イベント。
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    Status(String),
    TrioLog(String),
    Warning(String),
    Done { ok: bool, message: String },
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub task: String,
    pub workdir: PathBuf,
    /// Git リポジトリ内なら worktree を切ってその中で作業する。
    pub use_worktree: bool,
    /// Commit 後に `gh pr create --draft` を試みる。gh が無ければスキップ + 警告。
    pub open_pr: bool,
    /// セッション内で許容する Trio ラウンド上限。Redirected の再投入や Parked からの復帰も
    /// 1 ラウンドとして数える。Commit に至れば即終了 (= 早期完了は上限に達しない)。
    pub max_rounds: u32,
    /// Commit 成功後に worktree を prune する。デフォルトは false (= 跡を残す) で安全側。
    pub cleanup_worktree: bool,
    /// gh バイナリの上書き。テストでスタブを差し込むときに使う。None の場合は PATH の `gh`。
    pub gh_bin: Option<PathBuf>,
}

impl RunOptions {
    pub fn new(task: impl Into<String>, workdir: PathBuf) -> Self {
        Self {
            task: task.into(),
            workdir,
            use_worktree: true,
            open_pr: false,
            max_rounds: 4,
            cleanup_worktree: false,
            gh_bin: None,
        }
    }
}

/// 機能 1 件をセッションループで駆動する。`thrust_rx` から Z 軸推進シグナル/Redirect が来る。
/// `event_tx` は無くても可 (= headless 用に進捗イベントを stderr に流すだけ)。
pub async fn run_feature(
    cfg: Config,
    opts: RunOptions,
    mut thrust_rx: ThrustReceiver,
    event_tx: Option<mpsc::Sender<RuntimeEvent>>,
) -> Result<()> {
    let say = |line: String| {
        if let Some(tx) = &event_tx {
            let _ = tx.try_send(RuntimeEvent::TrioLog(line.clone()));
        }
        tracing::info!("{line}");
    };
    let status = |line: String| {
        if let Some(tx) = &event_tx {
            let _ = tx.try_send(RuntimeEvent::Status(line.clone()));
        }
        tracing::info!("{line}");
    };
    let warn = |line: String| {
        if let Some(tx) = &event_tx {
            let _ = tx.try_send(RuntimeEvent::Warning(line.clone()));
        }
        tracing::warn!("{line}");
    };

    // --- 1) History
    std::fs::create_dir_all(&cfg.history_root)
        .with_context(|| format!("create history root: {}", cfg.history_root.display()))?;
    let store = HistoryStore::open(&cfg.history_root)
        .with_context(|| format!("open history store: {}", cfg.history_root.display()))?;
    let feature = store
        .create_feature(&opts.task)
        .context("create feature row")?;
    status(format!("feature_id={} task={}", feature.id, opts.task));

    // --- 2) Worktree
    let mut worktree_handle: Option<WorktreeHandle> = None;
    let work_root: PathBuf = if opts.use_worktree && is_git_repo(&opts.workdir) {
        match carve_worktree(&opts.workdir, &feature.id, None) {
            Ok(h) => {
                say(format!(
                    "worktree carved: branch={} path={}",
                    h.branch_name,
                    h.worktree_path.display()
                ));
                let p = h.worktree_path.clone();
                worktree_handle = Some(h);
                p
            }
            Err(e) => {
                warn(format!("worktree carve failed, falling back to workdir: {e}"));
                opts.workdir.clone()
            }
        }
    } else {
        opts.workdir.clone()
    };

    // --- 4) LLM (LLM 駆動の検索ツールが LlmClient を保持するので tools 構築の前に作る)
    let llm: Arc<dyn LlmClient> = Arc::new(
        OpenAiCompatClient::new(cfg.llm.clone()).context("build OpenAI-compat LLM client")?,
    );

    // --- 3) Tools
    let tools = build_tool_registry(work_root.clone(), llm.clone());

    // --- 5) Trio + ViewProvider
    let trio = Trio::from_shared_llm(llm.clone()).with_thresholds(ConsensusThresholds {
        confidence_sum_min: cfg.trio.confidence_sum_min,
        triangle_balance_min: cfg.trio.triangle_balance_min,
        max_iter_per_step: cfg.trio.max_iter_per_step,
    });

    let initial_prompt = format!(
        "{task}\n\n\
         Available tools (call as a fenced ```json block with {{\"tool\":\"name\",\"args\":{{...}}}}):\n\
         - edit_file / read_file: full-file write or read\n\
         - patch_file: search/replace inside an existing file\n\
         - list_files: list workspace files (supports glob)\n\
         - grep_text: literal/regex search across files\n\
         - search_source: PageIndex-style AST search (LLM walks the source tree)\n\
         - run_cmd: run a shell command (denylist enforced)\n\
         - web_search / web_fetch: optional, requires obscura on PATH\n\
         When you finish, emit a single line containing only DONE.",
        task = opts.task,
    );
    let mut messages: Vec<ChatMessage> = vec![ChatMessage::user(initial_prompt)];
    say("Trio starting (Worker / Supervisor / Observer)...".into());

    // --- セッションループ ---
    let mut rounds_used: u32 = 0;
    let mut last_committed: Option<(AppendRaw, String)> = None;
    let session_outcome: SessionOutcome = loop {
        if rounds_used >= opts.max_rounds {
            warn(format!("max_rounds ({}) exhausted without Commit", opts.max_rounds));
            break SessionOutcome::Aborted("max rounds exhausted".into());
        }
        rounds_used += 1;
        let provider = HistoryViewProvider::new(&store, feature.id.clone());
        let outcome = trio
            .run_step_with_views(&messages, &tools, &mut thrust_rx, Some(&provider))
            .await
            .context("Trio.run_step_with_views failed")?;

        match outcome.last {
            ConsensusOutcome::Commit { proposal, votes } => {
                for (i, v) in votes.iter().enumerate() {
                    say(format!(
                        "vote[{i}] approve={} confidence={:.2} note={}",
                        v.approve,
                        v.confidence,
                        short(&v.note, 120)
                    ));
                }
                let raw_body = format!(
                    "TASK: {task}\n\nPROPOSAL:\n{raw}\n",
                    task = opts.task,
                    raw = proposal.raw_text
                );
                last_committed = Some((
                    AppendRaw {
                        feature_id: feature.id.clone(),
                        parent_id: None,
                        kind: RawKind::Turn,
                        body: raw_body.clone(),
                    },
                    raw_body,
                ));
                break SessionOutcome::Committed;
            }
            ConsensusOutcome::Redirected { instruction } => {
                warn(format!(
                    "round {}/{}: redirect from user — {}",
                    rounds_used, opts.max_rounds, instruction
                ));
                messages.push(ChatMessage::user(format!(
                    "USER REDIRECT (mid-task): {instruction}\nAdjust your plan and re-emit a complete proposal."
                )));
                continue;
            }
            ConsensusOutcome::Parked { .. } => {
                say(format!(
                    "round {}/{}: parked — awaiting Z thrust",
                    rounds_used, opts.max_rounds
                ));
                let next = trio.await_thrust(&mut thrust_rx).await;
                match next {
                    Some(UserThrust::Go { strength }) if strength > 0.0 => continue,
                    Some(UserThrust::Redirect { instruction }) => {
                        messages.push(ChatMessage::user(format!(
                            "USER REDIRECT (after park): {instruction}\nAdjust and proceed."
                        )));
                        continue;
                    }
                    Some(UserThrust::Stop) => break SessionOutcome::Stopped,
                    Some(UserThrust::Pause) | Some(UserThrust::Go { .. }) | None => {
                        break SessionOutcome::Aborted("park: thrust closed or non-positive".into())
                    }
                }
            }
            ConsensusOutcome::Stopped => break SessionOutcome::Stopped,
            ConsensusOutcome::Escalated { .. } => {
                warn(format!(
                    "Trio escalated after {} internal iterations on round {}",
                    outcome.steps, rounds_used
                ));
                break SessionOutcome::Escalated;
            }
        }
    };

    // --- 6) Persist + compact (Commit のみ)
    if let SessionOutcome::Committed = &session_outcome {
        if let Some((append, raw_body)) = last_committed.clone() {
            let raw = match store.append_raw(append) {
                Ok(r) => r,
                Err(e) => {
                    warn(format!("append raw failed: {e}"));
                    finalize_worktree(
                        &worktree_handle,
                        &opts,
                        &feature.id,
                        &say,
                        &warn,
                        false,
                    );
                    done(&event_tx, false, "raw append failed".into());
                    return Ok(());
                }
            };
            let lenses: Vec<Box<dyn AgentLens>> = vec![
                Box::new(LlmLens::new(
                    AgentView::Worker,
                    tmoe_prompts::WORKER_SYSTEM,
                    llm.clone(),
                )),
                Box::new(LlmLens::new(
                    AgentView::Supervisor,
                    tmoe_prompts::SUPERVISOR_SYSTEM,
                    llm.clone(),
                )),
                Box::new(LlmLens::new(
                    AgentView::Observer,
                    tmoe_prompts::OBSERVER_SYSTEM,
                    llm.clone(),
                )),
            ];
            if let Err(e) =
                compact_turn_for_all(&store, &feature.id, &raw, &raw_body, &lenses).await
            {
                warn(format!("compaction error (history not updated): {e}"));
            } else {
                status("3 views updated by personality compaction".into());
            }
        }
    }

    // --- 7) Worktree commit + (optional) PR + (optional) cleanup
    let committed = matches!(session_outcome, SessionOutcome::Committed);
    finalize_worktree(&worktree_handle, &opts, &feature.id, &say, &warn, committed);

    let (ok, msg) = match session_outcome {
        SessionOutcome::Committed => (true, format!("feature {} committed in {} round(s)", feature.id, rounds_used)),
        SessionOutcome::Stopped => (false, "stopped by user".into()),
        SessionOutcome::Escalated => (false, "escalated (Trio could not form plane)".into()),
        SessionOutcome::Aborted(reason) => (false, format!("aborted: {reason}")),
    };
    done(&event_tx, ok, msg);
    Ok(())
}

#[derive(Debug, Clone)]
enum SessionOutcome {
    Committed,
    Stopped,
    Escalated,
    Aborted(String),
}

fn finalize_worktree<F1, F2>(
    handle: &Option<WorktreeHandle>,
    opts: &RunOptions,
    feature_id: &str,
    say: &F1,
    warn: &F2,
    committed: bool,
) where
    F1: Fn(String),
    F2: Fn(String),
{
    let Some(h) = handle else {
        if opts.open_pr {
            warn("--pr requested but not in a git repo / worktree disabled; skipping".into());
        }
        return;
    };
    if committed {
        if let Err(e) = stage_all(h) {
            warn(format!("git stage failed: {e}"));
        }
        let msg = format!("tmoe[{feature_id}]: {}", opts.task);
        match git_commit(h, "tmoe", "tmoe@local", &msg) {
            Ok(oid) => say(format!(
                "committed {} on {}",
                short(&oid.to_string(), 10),
                h.branch_name
            )),
            Err(e) => warn(format!("git commit failed: {e}")),
        }
        if opts.open_pr {
            run_gh_pr_create(h, opts, feature_id, say, warn);
        }
    } else if opts.open_pr {
        warn("session did not commit; skipping --pr".into());
    }

    if opts.cleanup_worktree {
        let cloned = (*h).clone();
        match cleanup_worktree(cloned) {
            Ok(()) => say(format!("worktree pruned: {}", h.worktree_path.display())),
            Err(e) => warn(format!("worktree cleanup failed: {e}")),
        }
    }
}

fn run_gh_pr_create<F1, F2>(
    handle: &WorktreeHandle,
    opts: &RunOptions,
    feature_id: &str,
    say: &F1,
    warn: &F2,
) where
    F1: Fn(String),
    F2: Fn(String),
{
    let bin = opts
        .gh_bin
        .clone()
        .unwrap_or_else(|| PathBuf::from("gh"));
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("pr")
        .arg("create")
        .arg("--draft")
        .arg("--title")
        .arg(&opts.task)
        .arg("--body")
        .arg(format!("Auto-generated by tmoe (feature {feature_id})."))
        .arg("--head")
        .arg(&handle.branch_name)
        .current_dir(&handle.worktree_path);
    let out = cmd.output();
    match out {
        Ok(o) if o.status.success() => say(format!(
            "PR draft opened: {}",
            String::from_utf8_lossy(&o.stdout).trim()
        )),
        Ok(o) => warn(format!(
            "{} pr create failed: {}",
            bin.display(),
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => warn(format!("{} invocation error: {e}", bin.display())),
    }
}

fn done(tx: &Option<mpsc::Sender<RuntimeEvent>>, ok: bool, message: String) {
    if let Some(tx) = tx {
        let _ = tx.try_send(RuntimeEvent::Done { ok, message });
    }
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.replace('\n', " ")
    } else {
        let head: String = s.chars().take(n).collect();
        format!("{head}…")
    }
}

fn is_git_repo(p: &Path) -> bool {
    git2::Repository::discover(p).is_ok()
}

/// プロンプトが Worker に告知している全ツールを 1 箇所で登録する。
/// ここに登録されていないものは Worker が `{"tool": ...}` で呼んでも `unknown tool` で弾かれる。
///
/// `search_source` は tmoe-tree (AST 木) + tmoe-rag (LLM 駆動の木探索) の組合わせで動くので
/// LlmClient を要求する。Phase 5 のライブラリが long unused にならないようここで結線する。
pub fn build_tool_registry(root: PathBuf, llm: Arc<dyn LlmClient>) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ReadFileTool { root: root.clone() }));
    reg.register(Arc::new(EditFileTool { root: root.clone() }));
    reg.register(Arc::new(PatchFileTool { root: root.clone() }));
    reg.register(Arc::new(ListFilesTool { root: root.clone() }));
    reg.register(Arc::new(GrepTextTool { root: root.clone() }));
    reg.register(Arc::new(RunCmdTool {
        root: root.clone(),
        blocklist: default_blocklist(),
    }));
    reg.register(Arc::new(WebSearchTool::default()));
    reg.register(Arc::new(WebFetchTool::new()));
    reg.register(Arc::new(SearchSourceTool::new(root, llm)));
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_includes_all_advertised_tools() {
        let dir = tempfile::tempdir().unwrap();
        let llm: Arc<dyn LlmClient> = Arc::new(tmoe_llm::MockLlmClient::new("dummy"));
        let reg = build_tool_registry(dir.path().to_path_buf(), llm);
        let names = reg.names();
        for name in [
            "read_file", "edit_file", "patch_file", "list_files", "grep_text",
            "run_cmd", "web_search", "web_fetch", "search_source",
        ] {
            assert!(
                names.iter().any(|n| *n == name),
                "tool '{name}' missing from CLI ToolRegistry — prompt is lying. registered: {names:?}"
            );
        }
    }

    #[test]
    fn is_git_repo_recognizes_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        assert!(!is_git_repo(path));
        git2::Repository::init(path).unwrap();
        assert!(is_git_repo(path));
    }

    #[test]
    fn run_options_defaults_match_documented() {
        let o = RunOptions::new("hello", PathBuf::from("/tmp"));
        assert_eq!(o.max_rounds, 4);
        assert!(!o.cleanup_worktree);
        assert!(o.use_worktree);
        assert!(!o.open_pr);
    }

    /// `--pr` 経路で gh コマンドが構築されるかを、PATH に gh 実物を要求せずに確かめる。
    /// 1) 現在のリポジトリを `git2::Repository::init` した一時ディレクトリで開き、
    /// 2) 初期コミットを作って worktree を切り、
    /// 3) gh の代わりに argv を /tmp/argv.txt に書き出すだけのスタブシェルスクリプトを用意し、
    /// 4) `RunOptions::gh_bin` でそのスタブを差し込んで `run_gh_pr_create` を直叩きする。
    /// 期待引数: `pr create --draft --title <task> --body ... --head <branch>`
    #[test]
    #[cfg(unix)]
    fn gh_pr_create_invocation_uses_expected_argv_via_stub() {
        use git2::Repository;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use tmoe_tools::carve_worktree;

        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join("repo");
        fs::create_dir_all(&repo_path).unwrap();
        let repo = Repository::init(&repo_path).unwrap();

        // 初期コミット (worktree 切り出しに HEAD が必要)。
        fs::write(repo_path.join("README.md"), "init\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(std::path::Path::new("README.md")).unwrap();
        idx.write().unwrap();
        let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("test", "test@local").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let handle = carve_worktree(&repo_path, "FEAT01", None).unwrap();

        // 引数記録用ファイルとスタブスクリプト。
        let argv_log = dir.path().join("gh_argv.txt");
        let stub_path = dir.path().join("gh_stub.sh");
        fs::write(
            &stub_path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\necho https://example/pr/1\nexit 0\n",
                argv_log.display()
            ),
        )
        .unwrap();
        let mut perm = fs::metadata(&stub_path).unwrap().permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&stub_path, perm).unwrap();

        let mut opts = RunOptions::new("rename old to new", repo_path.clone());
        opts.gh_bin = Some(stub_path.clone());
        opts.open_pr = true;

        // closures capture by &mut but our run_gh_pr_create takes &F1: Fn(String).
        // クロージャは外側の Vec を `&mut` でキャプチャできない (Fn は &self)。
        // 代わりに RefCell でラップする。
        let said_cell = std::cell::RefCell::new(Vec::<String>::new());
        let warned_cell = std::cell::RefCell::new(Vec::<String>::new());
        let say = |s: String| said_cell.borrow_mut().push(s);
        let warn = |s: String| warned_cell.borrow_mut().push(s);

        run_gh_pr_create(&handle, &opts, "FEAT01", &say, &warn);

        let said: Vec<String> = said_cell.into_inner();
        let warned: Vec<String> = warned_cell.into_inner();

        // スタブが起動した記録があるか
        assert!(argv_log.exists(), "stub gh did not run; said={said:?} warned={warned:?}");
        let recorded = fs::read_to_string(&argv_log).unwrap();
        let lines: Vec<&str> = recorded.lines().collect();
        // 期待: pr / create / --draft / --title / <task> / --body / <body> / --head / <branch>
        assert_eq!(lines[0], "pr");
        assert_eq!(lines[1], "create");
        assert_eq!(lines[2], "--draft");
        assert_eq!(lines[3], "--title");
        assert_eq!(lines[4], "rename old to new");
        assert_eq!(lines[5], "--body");
        assert!(
            lines[6].contains("FEAT01"),
            "body should mention feature id: {}",
            lines[6]
        );
        assert_eq!(lines[7], "--head");
        assert_eq!(lines[8], &handle.branch_name);

        // 結果が success なので say に "PR draft opened" が含まれるはず。
        assert!(
            said.iter().any(|s| s.contains("PR draft opened")),
            "expected success log: {said:?}"
        );
        assert!(warned.is_empty(), "unexpected warnings: {warned:?}");
    }

    /// `--pr` で gh が PATH に居らず、`gh_bin` も無いとき (= デフォルトの "gh" が
    /// 起動失敗するとき) は warn に縮退すること。`open_pr=true` でも runtime は落ちない。
    #[test]
    #[cfg(unix)]
    fn gh_pr_create_warns_when_gh_missing() {
        use git2::Repository;
        use std::fs;
        use tmoe_tools::carve_worktree;

        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join("repo");
        fs::create_dir_all(&repo_path).unwrap();
        let repo = Repository::init(&repo_path).unwrap();
        fs::write(repo_path.join("README.md"), "init\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(std::path::Path::new("README.md")).unwrap();
        idx.write().unwrap();
        let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("test", "test@local").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        let handle = carve_worktree(&repo_path, "FEAT02", None).unwrap();

        let mut opts = RunOptions::new("noop", repo_path.clone());
        opts.gh_bin = Some(PathBuf::from("/nonexistent/this/path/does/not/exist/gh"));
        opts.open_pr = true;

        let said_cell = std::cell::RefCell::new(Vec::<String>::new());
        let warned_cell = std::cell::RefCell::new(Vec::<String>::new());
        let say = |s: String| said_cell.borrow_mut().push(s);
        let warn = |s: String| warned_cell.borrow_mut().push(s);

        run_gh_pr_create(&handle, &opts, "FEAT02", &say, &warn);

        let warned = warned_cell.into_inner();
        assert!(
            warned.iter().any(|w| w.contains("invocation error") || w.contains("failed")),
            "expected a warn line, got: {warned:?}"
        );
    }
}
