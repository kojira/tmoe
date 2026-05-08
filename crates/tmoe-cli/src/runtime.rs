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
    ConsensusOutcome, ConsensusThresholds, DeltaSink, ThrustReceiver, Trio, UserThrust,
};
use tmoe_history::{
    compact_turn_for_all, AgentLens, AgentView, AppendRaw, HistoryStore, HistoryViewProvider,
    LlmLens, RawKind,
};
use tmoe_llm::{ChatMessage, LlmClient, OpenAiCompatClient};
use tmoe_tools::{
    carve_worktree, cleanup_worktree, default_blocklist, git_commit, stage_all, ApplyPatchTool,
    EditFileTool, GrepTextTool, ListFilesTool, PatchFileTool, ReadFileTool, RunCmdTool,
    ToolRegistry, WebFetchTool, WebSearchTool, WorktreeHandle,
};

use crate::agents_md;
use crate::history_tool::SearchHistoryTool;
use crate::plan_tool::{PlanEnterTool, PlanExitTool};
use crate::question_tool::{HeadlessAsker, QuestionAsker, QuestionTool};
use crate::skill_tool::{SkillRegistry, SkillTool};
use crate::source_tool::SearchSourceTool;
use tokio::sync::mpsc;

use crate::config::Config;

/// runtime → TUI に流す進捗イベント。
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    Status(String),
    TrioLog(String),
    Warning(String),
    /// 左 Concierge ペインに "(tmoe) ..." として表示する短い返事。
    /// 第一防壁 (LLM classifier の chat ルート) と第二防壁 (`tool_calls.is_empty()`) の
    /// 両方から流れる。Trio Worker streaming sink (= TrioLog) と混線しないよう専用バリアント。
    ConciergeReply(String),
    /// classifier task の判定結果を main loop に届ける制御信号。
    /// `Route::Task` なら main loop が `spawn_session` を呼ぶ。`Route::Chat` の reply 自体は
    /// 別途 ConciergeReply で流れるので、ここでは route のタグだけ運ぶ。
    Routed { task: bool, user_input: String },
    Done { ok: bool, message: String },
}

#[derive(Clone)]
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
    /// 既存 feature を再開する場合はその id を入れる。新規 feature を作りたいときは None。
    /// resume 時は HistoryStore からタイトルと 3 view brief を読んで Worker に prepend する。
    pub resume_feature_id: Option<String>,
    /// Worker の `question` ツールが user に問い合わせるための asker。
    /// None なら Headless asker (= 即エラー) を使う。
    pub question_asker: Option<Arc<dyn QuestionAsker>>,
}

impl std::fmt::Debug for RunOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunOptions")
            .field("task", &self.task)
            .field("workdir", &self.workdir)
            .field("use_worktree", &self.use_worktree)
            .field("open_pr", &self.open_pr)
            .field("max_rounds", &self.max_rounds)
            .field("cleanup_worktree", &self.cleanup_worktree)
            .field("gh_bin", &self.gh_bin)
            .field("resume_feature_id", &self.resume_feature_id)
            .field("question_asker", &self.question_asker.as_ref().map(|_| "<asker>"))
            .finish()
    }
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
            resume_feature_id: None,
            question_asker: None,
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

    // --- 0) Preflight: LLM が立っているかを最初に確認する。
    //         初見ユーザーが reqwest スタックトレースに直面しないようにするための gate。
    //         失敗時は History/Worktree を作る前に exit するので、後始末も不要。
    let preflight_client = OpenAiCompatClient::new(cfg.llm.clone())
        .context("build OpenAI-compat LLM client (preflight)")?;
    match preflight_client.health_check().await {
        Ok(hs) if hs.ok => {
            status(format!(
                "LLM ok: {} ({}status={})",
                hs.url,
                if hs.main_model_visible {
                    "model visible, "
                } else {
                    ""
                },
                hs.status_code
            ));
        }
        Ok(hs) => {
            let msg = format!(
                "LLM at {} responded with HTTP {}. Is the right server running?",
                hs.url, hs.status_code
            );
            warn(msg.clone());
            warn(friendly_llm_setup_hint(&cfg));
            done(&event_tx, false, msg);
            return Ok(());
        }
        Err(e) => {
            let msg = format!(
                "Cannot reach LLM at {}: {}",
                cfg.llm.base_url, e
            );
            warn(msg.clone());
            warn(friendly_llm_setup_hint(&cfg));
            done(&event_tx, false, msg);
            return Ok(());
        }
    }

    // --- 1) History (新規 or resume)
    std::fs::create_dir_all(&cfg.history_root)
        .with_context(|| format!("create history root: {}", cfg.history_root.display()))?;
    let store = HistoryStore::open(&cfg.history_root)
        .with_context(|| format!("open history store: {}", cfg.history_root.display()))?;
    let feature = if let Some(fid) = &opts.resume_feature_id {
        let f = store
            .get_feature(fid)
            .with_context(|| format!("resume feature {fid}: not found"))?;
        status(format!("resuming feature_id={} title={}", f.id, f.title));
        f
    } else {
        let f = store.create_feature(&opts.task).context("create feature row")?;
        status(format!("feature_id={} task={}", f.id, opts.task));
        f
    };

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

    // --- 3) Tools (search_history は HistoryStore + 現 feature_id を握る必要があるので、
    //    feature 行が確定した後にここで上書き登録する)
    let mut tools = build_tool_registry(work_root.clone(), llm.clone());
    let history_arc = std::sync::Arc::new(
        HistoryStore::open(&cfg.history_root)
            .with_context(|| "open history for search_history tool")?,
    );
    tools.register(std::sync::Arc::new(
        SearchHistoryTool::new(history_arc, llm.clone()).with_current_feature(feature.id.clone()),
    ));
    // question ツールは asker を runtime オプションから受け取る。未指定なら Headless = 即エラー。
    let asker: Arc<dyn QuestionAsker> = opts
        .question_asker
        .clone()
        .unwrap_or_else(|| Arc::new(HeadlessAsker));
    tools.register(std::sync::Arc::new(QuestionTool::new(asker.clone())));

    // plan_enter / plan_exit: plan モードの入口と出口。Worker が複雑タスクで使う。
    // plan_enter は markdown を `<work_root>/.tmoe/plans/<feature_id>.md` に保存し、
    // plan_exit は同じ asker (TUI ChannelAsker / Headless / Scripted) で user に承認を取る。
    tools.register(std::sync::Arc::new(PlanEnterTool {
        workdir: work_root.clone(),
        feature_id: feature.id.clone(),
    }));
    tools.register(std::sync::Arc::new(PlanExitTool {
        workdir: work_root.clone(),
        feature_id: feature.id.clone(),
        asker: asker.clone(),
    }));

    // skill ツール: workdir / .claude / .agents / global の 4 階層を 1 度走査して登録。
    // 同名は workdir 系で global を上書きする。空でも tool 自体は registry に居る
    // (= worker が誤って呼んだ際の "available: (none)" を返すため)。
    let home_dir = std::env::var("HOME").ok().map(PathBuf::from);
    let skill_registry = Arc::new(SkillRegistry::scan(&work_root, home_dir.as_deref()));
    tools.register(std::sync::Arc::new(SkillTool {
        registry: skill_registry.clone(),
    }));

    // --- 5) Trio + ViewProvider
    let trio = Trio::from_shared_llm(llm.clone()).with_thresholds(ConsensusThresholds {
        confidence_sum_min: cfg.trio.confidence_sum_min,
        triangle_balance_min: cfg.trio.triangle_balance_min,
        max_iter_per_step: cfg.trio.max_iter_per_step,
    });

    // AGENTS.md (および TMOE.md) を work_root から git ルート (or HOME) まで集める。
    // 浅い階層が先に来るので Worker は「プロジェクト全体ルール → サブディレクトリ固有」
    // の順に重ね合わせて読める。
    let agents_ctx = agents_md::collect(&work_root);
    if !agents_ctx.is_empty() {
        let names: Vec<String> = agents_ctx
            .files
            .iter()
            .map(|f| f.path.display().to_string())
            .collect();
        status(format!("project instructions loaded: {} file(s)", names.len()));
        for n in &names {
            say(format!("  - {n}"));
        }
    }
    let agents_block = agents_ctx.render_for_prompt();

    if !skill_registry.is_empty() {
        let names = skill_registry.names();
        status(format!("skills loaded: {} ({})", names.len(), names.join(", ")));
    }

    // Resume の場合は前回の 3 view brief を Worker に手渡す (= 「前回ここまでやった」)。
    let resume_block = if opts.resume_feature_id.is_some() {
        let mut s = String::from("RESUMING FEATURE — prior progress (3 personality views):\n\n");
        for view in [AgentView::Worker, AgentView::Supervisor, AgentView::Observer] {
            let label = view.as_str();
            let brief = match store.latest_level0(&feature.id, view) {
                Ok(Some(n)) => n.summary,
                _ => "(no view yet)".into(),
            };
            s.push_str(&format!("--- {label} ---\n{brief}\n\n"));
        }
        s.push_str("Continue from this state. Do NOT redo work that the views indicate is already complete.\n\n");
        s
    } else {
        String::new()
    };

    let task_line = if opts.resume_feature_id.is_some() {
        format!("Original feature title: {}\nFollow-up instruction: {}", feature.title, opts.task)
    } else {
        opts.task.clone()
    };

    // 利用可能な skill 一覧を 1 行ずつ "name — description" でプロンプトに埋め込む。
    // 名前と要約だけ見せ、本文は worker が `{"tool":"skill","args":{"name":...}}` で
    // 呼び出して始めて取り込む。これは Anthropic の "progressive disclosure" 思想と同じで、
    // 全 SKILL.md 本文を最初から context に焼き付けると 1 機能あたりのトークンが膨れる。
    let skills_block = if skill_registry.is_empty() {
        String::new()
    } else {
        let mut s = String::from("\nAvailable skills (load on demand via the `skill` tool):\n");
        for info in skill_registry.list() {
            s.push_str(&format!("  - {} — {}\n", info.name, info.description));
        }
        s
    };

    let initial_prompt = format!(
        "{agents}{resume}{task}\n\n\
         Available tools (call as a fenced ```json block with {{\"tool\":\"name\",\"args\":{{...}}}}):\n\
         - edit_file / read_file: full-file write or read\n\
         - patch_file: search/replace inside an existing file\n\
         - apply_patch: multi-file structural change (Add/Update/Delete/Move) — \
            args {{\"text\":\"*** Begin Patch\\n*** Update File: a.rs\\n@@\\n-old\\n+new\\n*** End Patch\"}}\n\
         - list_files: list workspace files (supports glob)\n\
         - grep_text: literal/regex search across files\n\
         - search_source: PageIndex-style AST search (LLM walks the source tree)\n\
         - search_history: Agentic RAG over past tmoe features (3-view summaries) — \
            args {{\"query\":\"...\",\"agent\":\"worker|supervisor|observer|any\",\"scope\":\"all|current\"}}\n\
         - question: ask the user a clarifying question — \
            args {{\"questions\":[{{\"question\":\"...\",\"options\":[\"a\",\"b\"]}}]}} \
            (errors in --headless mode)\n\
         - plan_enter / plan_exit: enter plan mode (write .tmoe/plans/<feature_id>.md) and \
            exit by asking the user to approve. Use for multi-file or architectural work.\n\
         - skill: load a project-defined SKILL.md by name — \
            args {{\"name\":\"<skill name>\"}} (see the skills list below)\n\
         - run_cmd: run a shell command (denylist enforced)\n\
         - web_search / web_fetch: optional, requires obscura on PATH\n\
         {skills}\n\
         When you finish, emit a single line containing only DONE.",
        agents = agents_block,
        resume = resume_block,
        task = task_line,
        skills = skills_block,
    );
    let mut messages: Vec<ChatMessage> = vec![ChatMessage::user(initial_prompt)];
    say("Trio starting (Worker / Supervisor / Observer)...".into());

    // Worker の応答 token を **改行ごと** にまとめて RuntimeEvent::TrioLog に流す sink。
    // event_tx が無ければ作らない (= headless で stderr 出力だけのとき streaming 不要)。
    let worker_delta_sink: Option<DeltaSink> = event_tx.as_ref().map(|tx| {
        let tx = tx.clone();
        let buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let buf2 = buf.clone();
        let cb = move |piece: String| {
            let mut b = buf2.lock().unwrap();
            b.push_str(&piece);
            while let Some(idx) = b.find('\n') {
                let line: String = b.drain(..=idx).collect();
                let line = line.trim_end_matches('\n');
                if !line.trim().is_empty() {
                    let _ = tx.try_send(RuntimeEvent::TrioLog(format!("worker: {line}")));
                }
            }
        };
        std::sync::Arc::new(cb) as DeltaSink
    });

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
        let outcome = if let Some(sink) = worker_delta_sink.clone() {
            trio.run_step_streaming(&messages, &tools, &mut thrust_rx, Some(&provider), sink)
                .await
                .context("Trio.run_step_streaming failed")?
        } else {
            trio.run_step_with_views(&messages, &tools, &mut thrust_rx, Some(&provider))
                .await
                .context("Trio.run_step_with_views failed")?
        };

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
                let no_op = proposal.tool_calls.is_empty();
                last_committed = Some((
                    AppendRaw {
                        feature_id: feature.id.clone(),
                        parent_id: None,
                        kind: RawKind::Turn,
                        body: raw_body.clone(),
                    },
                    raw_body,
                ));
                if no_op {
                    // ③ 防壁: Worker が proposal で 1 つもツールを呼ばなかった = chat に近い回答。
                    // worktree commit / PR / 3 view compaction は全部スキップ。Worker の prose
                    // (= proposal.note の中身) をそのまま左 Concierge ペインに返事として流す。
                    let reply = proposal.note.trim().to_string();
                    if !reply.is_empty() {
                        if let Some(tx) = &event_tx {
                            let _ = tx.try_send(RuntimeEvent::ConciergeReply(reply.clone()));
                        }
                    }
                    break SessionOutcome::CommittedNoOp { note: reply };
                }
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

    // --- 6) Persist + compact (Commit のみ; CommittedNoOp は raw 追記だけして compaction はスキップ)
    let is_real_commit = matches!(session_outcome, SessionOutcome::Committed);
    let is_no_op = matches!(session_outcome, SessionOutcome::CommittedNoOp { .. });
    if is_real_commit || is_no_op {
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
                        false,
                    );
                    done(&event_tx, false, "raw append failed".into());
                    return Ok(());
                }
            };
            // 3 view 並走逐次コンパクションは「実装が進んだ」場合のみ走らせる。
            // CommittedNoOp は空 view を 3 つ作るだけで価値ゼロなのでスキップ。
            if is_real_commit {
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
    }

    // --- 7) Worktree commit + (optional) PR + (optional) cleanup
    // committed=true は git commit + (optional) PR を走らせる。CommittedNoOp は git commit せず、
    // ただし worktree branch は強制削除して `git worktree list` を汚さない。
    let committed = is_real_commit;
    let force_cleanup = is_no_op;
    finalize_worktree(
        &worktree_handle,
        &opts,
        &feature.id,
        &say,
        &warn,
        committed,
        force_cleanup,
    );

    let (ok, msg) = match session_outcome {
        SessionOutcome::Committed => (true, format!("feature {} committed in {} round(s)", feature.id, rounds_used)),
        SessionOutcome::CommittedNoOp { note } => {
            let preview = short(&note, 80);
            (true, format!("no file changes ({preview})"))
        }
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
    /// Worker が tool を 1 つも呼ばなかった (= chat に近い回答だけだった) ケース。
    /// `note` は Worker の prose (返事本文)。worktree も commit も作らず、
    /// `ConciergeReply` で左ペインに返事を流して終わる。
    CommittedNoOp {
        note: String,
    },
    Stopped,
    Escalated,
    Aborted(String),
}

// `force_cleanup` は `CommittedNoOp` 経路から「コミットしないが branch は消す」を要求するフラグ。
// `opts.cleanup_worktree` が false でも、これが true なら worktree prune を実行する。
fn finalize_worktree<F1, F2>(
    handle: &Option<WorktreeHandle>,
    opts: &RunOptions,
    feature_id: &str,
    say: &F1,
    warn: &F2,
    committed: bool,
    force_cleanup: bool,
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
        warn("session ended with no file changes; skipping --pr".into());
    }

    if opts.cleanup_worktree || force_cleanup {
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

/// LLM が落ちている / 設定が空のときに出す、初見ユーザー向けの 1 文セットアップヒント。
/// `cfg` を読んで現在指している URL/モデルを言いつつ、起動コマンド例を Apple Silicon /
/// Linux で 1 つずつ示す。
pub fn friendly_llm_setup_hint(cfg: &Config) -> String {
    format!(
        "Hint: tmoe expects an OpenAI-compatible LLM at {url} (model={model}).\n\
         - Apple Silicon: rapid-mlx serve qwen3-coder-30b --port 8081\n\
         - Linux/CUDA:    llama-server -m <model.gguf> --port 8081 --host 127.0.0.1\n\
         - Or override:   TMOE_LLM_URL=http://your-host:port/v1 TMOE_LLM_MODEL=<id> tmoe \"<task>\"\n\
         Run `tmoe doctor` to print a one-shot diagnostic.",
        url = cfg.llm.base_url,
        model = cfg.llm.main_model,
    )
}

/// `tmoe doctor`: 設定 + LLM 接続性 + オプショナルバイナリの 1 ショット診断。
/// 標準出力に印字する形で、初見ユーザーが「自分の環境がどこまで整っているか」を確認する。
pub async fn doctor(cfg: &Config) -> Result<bool> {
    println!("--- tmoe doctor ---");
    println!("history_root: {}", cfg.history_root.display());
    let client = OpenAiCompatClient::new(cfg.llm.clone())
        .context("build OpenAI-compat LLM client")?;
    let desc = client.describe();
    println!("backend:      {}", desc.backend);
    println!("base_url:     {}", desc.base_url);
    println!("main_model:   {}", desc.main_model);
    println!(
        "draft_model:  {} (speculative_enabled={})",
        desc.draft_model.as_deref().unwrap_or("<none>"),
        desc.speculative_enabled
    );

    // Backend::Codex の場合、auth.json の有無 + 期限を健診に含める。
    if cfg.llm.backend == tmoe_llm::Backend::Codex {
        let auth_path = cfg
            .llm
            .codex_auth_path
            .clone()
            .unwrap_or_else(tmoe_llm::default_auth_path);
        match tmoe_llm::load_codex_auth(&auth_path) {
            Ok(Some(a)) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let remaining = a.expires_at_unix.saturating_sub(now);
                println!(
                    "codex auth:   ok (account_id={}, access_expires_in={}s, file={})",
                    a.account_id.as_deref().unwrap_or("<none>"),
                    remaining,
                    auth_path.display()
                );
            }
            Ok(None) => {
                println!(
                    "codex auth:   MISSING — run `tmoe codex login` (file: {})",
                    auth_path.display()
                );
            }
            Err(e) => {
                println!("codex auth:   ERROR loading {}: {e}", auth_path.display());
            }
        }
    }

    let mut all_green = true;
    print!("LLM /v1/models: ");
    match client.health_check().await {
        Ok(hs) if hs.ok => {
            println!(
                "OK (status={}, main_model_visible={})",
                hs.status_code, hs.main_model_visible
            );
            if !hs.main_model_visible {
                println!("  note: main_model '{}' is not listed by the backend; it may still work via fallback model loading", desc.main_model);
            }
        }
        Ok(hs) => {
            println!("FAIL (HTTP {})", hs.status_code);
            all_green = false;
        }
        Err(e) => {
            println!("FAIL ({e})");
            all_green = false;
        }
    }

    let gh_ok = which_gh();
    println!("gh CLI:       {}", if gh_ok { "found" } else { "missing (--pr will warn and skip)" });
    let obscura_path = std::env::var("TMOE_OBSCURA_BIN")
        .ok()
        .or_else(|| {
            std::process::Command::new("which")
                .arg("obscura")
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                    } else {
                        None
                    }
                })
        });
    println!(
        "obscura:      {}",
        obscura_path.as_deref().unwrap_or("missing (web_search/web_fetch will fail at call time)")
    );

    if !all_green {
        println!();
        println!("{}", friendly_llm_setup_hint(cfg));
    } else {
        println!();
        println!("All required components reachable. Try: tmoe \"<task>\"");
    }
    Ok(all_green)
}

fn which_gh() -> bool {
    std::process::Command::new("gh")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `tmoe merge <feature_id>`: feature の worktree ブランチ (`tmoe/feature/<id>`) を
/// 現在のチェックアウト (= ユーザの cwd の git リポジトリ) に `git merge --no-ff` でマージする。
///
/// HistoryStore の get_feature でタイトルを引いてコミットメッセージに使う。merge 自体は
/// `std::process::Command::new("git")` で実行する。conflict が出た場合は git の出力を素通しして
/// ユーザが手動解決できるようにする (= ここで rollback しない)。
pub fn merge_feature(cfg: &Config, workdir: &Path, feature_id: &str) -> Result<()> {
    let store = HistoryStore::open(&cfg.history_root)
        .with_context(|| format!("open history at {}", cfg.history_root.display()))?;
    let feature = store
        .get_feature(feature_id)
        .with_context(|| format!("feature {feature_id} not found in history"))?;
    let branch = format!("tmoe/feature/{}", feature_id);
    if !is_git_repo(workdir) {
        anyhow::bail!(
            "current workdir ({}) is not inside a git repo; cd into the project root first",
            workdir.display()
        );
    }
    // ブランチ存在確認: `git rev-parse --verify <branch>` の終了コードで判定。
    let exists = std::process::Command::new("git")
        .arg("rev-parse")
        .arg("--verify")
        .arg(&branch)
        .current_dir(workdir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !exists {
        anyhow::bail!(
            "branch {branch} does not exist in this repo. \
             Was this feature run with --no-worktree, or has the branch been pruned?"
        );
    }
    let msg = format!("merge tmoe feature {} ({})", feature_id, feature.title);
    println!("merging {branch} into HEAD with --no-ff");
    let status = std::process::Command::new("git")
        .arg("merge")
        .arg("--no-ff")
        .arg("-m")
        .arg(&msg)
        .arg(&branch)
        .current_dir(workdir)
        .status()
        .context("spawn git merge")?;
    if !status.success() {
        anyhow::bail!(
            "git merge exited with {}. Resolve conflicts and run `git merge --continue`, \
             or `git merge --abort` to back out.",
            status
        );
    }
    println!("merge complete");
    Ok(())
}

/// `tmoe history list`: HistoryStore の全 feature を新しい順に印字する。
pub fn history_list(cfg: &Config) -> Result<()> {
    let store = HistoryStore::open(&cfg.history_root)
        .with_context(|| format!("open history at {}", cfg.history_root.display()))?;
    let features = store.list_features().context("list features")?;
    if features.is_empty() {
        println!("(no features yet at {})", cfg.history_root.display());
        return Ok(());
    }
    println!("history root: {}", cfg.history_root.display());
    println!(
        "{:<28} {:<14} {:<20} {}",
        "feature_id", "status", "created_at", "title"
    );
    for f in features {
        let dt = chrono_like(f.created_at);
        let title_short = short(&f.title, 60);
        println!(
            "{:<28} {:<14} {:<20} {}",
            f.id,
            format!("{:?}", f.status),
            dt,
            title_short
        );
    }
    Ok(())
}

/// `tmoe history show <feature_id>`: feature の 3 view brief と raw_node 数を印字する。
pub fn history_show(cfg: &Config, feature_id: &str) -> Result<()> {
    let store = HistoryStore::open(&cfg.history_root)
        .with_context(|| format!("open history at {}", cfg.history_root.display()))?;
    let feature = store
        .get_feature(feature_id)
        .with_context(|| format!("feature {feature_id} not found"))?;
    let raws = store.list_raw(&feature.id).unwrap_or_default();
    println!("feature_id:  {}", feature.id);
    println!("title:       {}", feature.title);
    println!("status:      {:?}", feature.status);
    println!("created_at:  {}", chrono_like(feature.created_at));
    println!("raw_nodes:   {}", raws.len());
    println!();
    for view in [AgentView::Worker, AgentView::Supervisor, AgentView::Observer] {
        println!("--- {} view (latest level=0) ---", view.as_str());
        match store.latest_level0(&feature.id, view) {
            Ok(Some(node)) => {
                println!("{}", node.summary.trim());
            }
            Ok(None) => println!("(empty)"),
            Err(e) => println!("(error: {e})"),
        }
        println!();
    }
    Ok(())
}

fn chrono_like(unix_secs: i64) -> String {
    // 外部 crate を増やさないため SystemTime ベースの簡易フォーマット。
    // YYYY-MM-DD HH:MM:SS (UTC)。`chrono` を入れる価値はあるが履歴表示の数行のためだけに入れない。
    let secs = unix_secs.max(0) as u64;
    let days = secs / 86_400;
    let hms = secs % 86_400;
    // unix epoch: 1970-01-01。簡易にうるう年計算。
    let mut y: i64 = 1970;
    let mut d_left = days as i64;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
        let yd = if leap { 366 } else { 365 };
        if d_left < yd {
            break;
        }
        d_left -= yd;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
    let mdays = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0usize;
    while m < 12 && d_left >= mdays[m] {
        d_left -= mdays[m];
        m += 1;
    }
    let day = (d_left + 1) as u32;
    let h = (hms / 3600) as u32;
    let mi = ((hms % 3600) / 60) as u32;
    let s = (hms % 60) as u32;
    format!("{y:04}-{:02}-{day:02} {h:02}:{mi:02}:{s:02}", m + 1)
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
    reg.register(Arc::new(ApplyPatchTool { root: root.clone() }));
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
        // 注: search_history は run_feature が feature_id を確定したあとに register するので、
        // build_tool_registry には含まれない。プロンプトに search_history が出ているのに
        // registry に居ない、という嘘を防ぐため、search_history については別 e2e
        // (history_tool::tests / integration) で覆う。
        let dir = tempfile::tempdir().unwrap();
        let llm: Arc<dyn LlmClient> = Arc::new(tmoe_llm::MockLlmClient::new("dummy"));
        let reg = build_tool_registry(dir.path().to_path_buf(), llm);
        let names = reg.names();
        for name in [
            "read_file", "edit_file", "patch_file", "apply_patch", "list_files",
            "grep_text", "run_cmd", "web_search", "web_fetch", "search_source",
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
