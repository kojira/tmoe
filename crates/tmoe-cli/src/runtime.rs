//! tmoe ランタイム: Trio + History + ViewProvider + Tools + Worktree を 1 機能ぶんに結線する。
//!
//! このファイルが「個別 crate は動くが結線層が空」という残存ギャップを埋める。
//! `run_feature` が機能 (Feature) のライフサイクル全体を駆動する:
//!
//! 1. HistoryStore を開き Feature 行を新規作成
//! 2. (オプション) git worktree を切り出し、Worker のツール作業ディレクトリにする
//! 3. ToolRegistry を組み立て (read/edit/run/patch/list/grep/web_search/web_fetch を全て登録)
//! 4. OpenAI 互換 LLM クライアントを構築
//! 5. Trio を構成し、HistoryViewProvider を噛ませて run_step_with_views を回す
//! 6. ConsensusOutcome::Commit に至れば: raw_node 追記 → 3 view 並走で逐次コンパクション
//! 7. worktree なら stage + commit。`open_pr` かつ gh が PATH にあれば `gh pr create --draft`
//!
//! 設計準拠ポイント:
//! - **Trio が ViewProvider 経由で 3 view brief を読む**ので Worker view が「書かれて読まれない」
//!   状態にならない (前セッションで埋めたギャップを CLI 経由でも実際に通る経路にする)
//! - **逐次コンパクション**: コミット直後に LlmLens で 3 view を 1 ノードずつ延伸
//! - **`Concierge は 4 人目ではない`**: Concierge は z_thrust シグナルを TUI から runtime に
//!   流す I/O チャネルとしてだけ存在し、エージェント本体には影響しない

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tmoe_core::{ConsensusOutcome, ConsensusThresholds, ThrustReceiver, Trio};
use tmoe_history::{
    compact_turn_for_all, AgentLens, AgentView, AppendRaw, HistoryStore, HistoryViewProvider,
    LlmLens, RawKind,
};
use tmoe_llm::{ChatMessage, LlmClient, OpenAiCompatClient};
use tmoe_tools::{
    carve_worktree, default_blocklist, git_commit, stage_all, EditFileTool, GrepTextTool,
    ListFilesTool, PatchFileTool, ReadFileTool, RunCmdTool, ToolRegistry, WebFetchTool,
    WebSearchTool, WorktreeHandle,
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
}

/// 機能 1 件を最後まで駆動する。`thrust_rx` が None なら `auto_go` を見て
/// 内部で Go を 1 回送る (headless 起動向け)。`event_tx` は無くても可。
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

    // --- 3) Tools (advertised in tmoe-prompts:WORKER_SYSTEM are ALL registered here)
    let tools = build_tool_registry(work_root.clone(), llm.clone());

    // --- 5) Trio + ViewProvider
    let trio = Trio::from_shared_llm(llm.clone()).with_thresholds(ConsensusThresholds {
        confidence_sum_min: cfg.trio.confidence_sum_min,
        triangle_balance_min: cfg.trio.triangle_balance_min,
        max_iter_per_step: cfg.trio.max_iter_per_step,
    });

    let messages = vec![ChatMessage::user(format!(
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
    ))];
    let provider = HistoryViewProvider::new(&store, feature.id.clone());

    say("Trio starting (Worker / Supervisor / Observer)...".into());
    let outcome = trio
        .run_step_with_views(&messages, &tools, &mut thrust_rx, Some(&provider))
        .await
        .context("Trio.run_step_with_views failed")?;

    let raw_body = match &outcome.last {
        ConsensusOutcome::Commit { proposal, votes } => {
            for (i, v) in votes.iter().enumerate() {
                say(format!(
                    "vote[{i}] approve={} confidence={:.2} note={}",
                    v.approve,
                    v.confidence,
                    short(&v.note, 120)
                ));
            }
            format!(
                "TASK: {task}\n\nPROPOSAL:\n{raw}\n",
                task = opts.task,
                raw = proposal.raw_text
            )
        }
        ConsensusOutcome::Parked { .. } => {
            warn("Trio parked: plane formed but no Z thrust yet".into());
            done(&event_tx, false, "parked".into());
            return Ok(());
        }
        ConsensusOutcome::Redirected { instruction } => {
            warn(format!("Trio redirected by user: {instruction}"));
            done(&event_tx, false, format!("redirected: {instruction}"));
            return Ok(());
        }
        ConsensusOutcome::Stopped => {
            warn("Trio stopped by user".into());
            done(&event_tx, false, "stopped".into());
            return Ok(());
        }
        ConsensusOutcome::Escalated { last_proposal: _ } => {
            warn(format!(
                "Trio escalated after {} iterations: plane never formed",
                outcome.steps
            ));
            done(&event_tx, false, "escalated".into());
            return Ok(());
        }
    };

    // --- 6) raw + 逐次コンパクション
    let raw = store
        .append_raw(AppendRaw {
            feature_id: feature.id.clone(),
            parent_id: None,
            kind: RawKind::Turn,
            body: raw_body.clone(),
        })
        .context("append raw_node")?;

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
    if let Err(e) = compact_turn_for_all(&store, &feature.id, &raw, &raw_body, &lenses).await {
        warn(format!("compaction error (history not updated): {e}"));
    } else {
        status("3 views updated by personality compaction".into());
    }

    // --- 7) Worktree commit + (optional) PR
    if let Some(handle) = &worktree_handle {
        if let Err(e) = stage_all(handle) {
            warn(format!("git stage failed: {e}"));
        }
        let msg = format!("tmoe[{}]: {}", &feature.id, opts.task);
        match git_commit(handle, "tmoe", "tmoe@local", &msg) {
            Ok(oid) => say(format!(
                "committed {} on {}",
                short(&oid.to_string(), 10),
                handle.branch_name
            )),
            Err(e) => warn(format!("git commit failed: {e}")),
        }
        if opts.open_pr {
            if which_gh() {
                let out = std::process::Command::new("gh")
                    .arg("pr")
                    .arg("create")
                    .arg("--draft")
                    .arg("--title")
                    .arg(&opts.task)
                    .arg("--body")
                    .arg(format!("Auto-generated by tmoe (feature {}).", feature.id))
                    .arg("--head")
                    .arg(&handle.branch_name)
                    .current_dir(&handle.worktree_path)
                    .output();
                match out {
                    Ok(o) if o.status.success() => say(format!(
                        "PR draft opened: {}",
                        String::from_utf8_lossy(&o.stdout).trim()
                    )),
                    Ok(o) => warn(format!(
                        "gh pr create failed: {}",
                        String::from_utf8_lossy(&o.stderr).trim()
                    )),
                    Err(e) => warn(format!("gh invocation error: {e}")),
                }
            } else {
                warn("--pr requested but `gh` not on PATH; skipping".into());
            }
        }
    } else if opts.open_pr {
        warn("--pr requested but not in a git repo / worktree disabled; skipping".into());
    }

    done(&event_tx, true, format!("feature {} committed", feature.id));
    Ok(())
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

fn which_gh() -> bool {
    std::process::Command::new("gh")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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
        // build_tool_registry が Worker プロンプトで宣伝しているツールを **全部**
        // 登録していることを保証する (= プロンプトが嘘をつかないことの最低保証)。
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
        // 空ディレクトリは repo ではない
        assert!(!is_git_repo(path));
        git2::Repository::init(path).unwrap();
        assert!(is_git_repo(path));
    }
}
