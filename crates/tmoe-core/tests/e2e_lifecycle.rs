//! e2e: scripted MockLlmClient で feature の通しシナリオを駆動する。
//!
//! 検証範囲:
//! - 平面合意 + Z 軸推進が揃って commit する happy path
//! - Z 軸が来ないあいだ park し、Concierge から Go が届くと再開する
//! - 各 raw コミットで 3 並走 index が独立に伸びる
//! - self_review が diff を読んで承認する
//! - worktree 切り出し → ファイル書き込み → diff → commit のフル運転

use std::sync::Arc;
use tempfile::tempdir;
use tmoe_core::{
    self_review::{supervisor_review_diff, SelfReviewOutcome},
    Agent, AgentRole, ConsensusOutcome, ThrustChannel, Trio, UserThrust,
};
use tmoe_history::{
    compact_turn_for_all, AgentLens, AgentView, AppendRaw, FeatureStatus, HistoryStore,
    LabeledLens, RawKind,
};
use tmoe_llm::{ChatMessage, MockLlmClient, ScriptedTurn};
use tmoe_tools::{
    carve_worktree, default_blocklist, git_commit, stage_all, working_diff_text, EditFileTool,
    ReadFileTool, RunCmdTool, ToolRegistry,
};

fn approve_json(conf: f32, note: &str) -> String {
    format!(r#"{{"approve":true,"confidence":{conf},"note":"{note}"}}"#)
}
fn reject_json(conf: f32, note: &str) -> String {
    format!(r#"{{"approve":false,"confidence":{conf},"note":"{note}"}}"#)
}

fn registry(root: std::path::PathBuf) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EditFileTool { root: root.clone() }));
    reg.register(Arc::new(ReadFileTool { root: root.clone() }));
    reg.register(Arc::new(RunCmdTool { root, blocklist: default_blocklist() }));
    reg
}

fn lenses() -> Vec<Box<dyn AgentLens>> {
    vec![
        Box::new(LabeledLens { agent: AgentView::Worker, label: "build" }),
        Box::new(LabeledLens { agent: AgentView::Supervisor, label: "critique" }),
        Box::new(LabeledLens { agent: AgentView::Observer, label: "witness" }),
    ]
}

#[tokio::test]
async fn happy_path_commit_with_history_and_three_views() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let reg = registry(root.clone());

    let worker = Arc::new(MockLlmClient::new("worker"));
    let sup = Arc::new(MockLlmClient::new("sup"));
    let obs = Arc::new(MockLlmClient::new("obs"));

    worker.push(ScriptedTurn::new(
        r#"提案
```json
{"tool":"edit_file","args":{"path":"src/util.rs","content":"pub fn gcd(a:u64,b:u64)->u64{if b==0{a}else{gcd(b,a%b)}}\n"}}
```
DONE"#,
    ));
    sup.push(ScriptedTurn::new(approve_json(0.9, "looks fine")));
    obs.push(ScriptedTurn::new(approve_json(0.85, "intent matches")));
    worker.push(ScriptedTurn::new(approve_json(0.85, "implementation complete")));

    let trio = Trio::new(
        Agent::new(AgentRole::Worker, worker, "worker-system"),
        Agent::new(AgentRole::Supervisor, sup, "supervisor-system"),
        Agent::new(AgentRole::Observer, obs, "observer-system"),
    );
    let (tx, mut rx) = ThrustChannel::new();
    tx.send(UserThrust::Go { strength: 1.0 }).unwrap();

    let outcome = trio
        .run_step(
            &[ChatMessage::user("src/util.rs に gcd を追加して")],
            &reg,
            &mut rx,
        )
        .await
        .unwrap();
    assert!(matches!(outcome.last, ConsensusOutcome::Commit { .. }));
    let written = std::fs::read_to_string(root.join("src/util.rs")).unwrap();
    assert!(written.contains("pub fn gcd"));

    // 履歴: raw + 3 並走 index に書き込めるか。
    let store_dir = tempdir().unwrap();
    let store = HistoryStore::open(store_dir.path()).unwrap();
    let f = store.create_feature("add gcd").unwrap();
    let raw_body =
        "user: gcd 追加\nworker: implement gcd via euclid\nsupervisor: must check overflow\nobserver: intent: math util add";
    let raw = store
        .append_raw(AppendRaw {
            feature_id: f.id.clone(),
            parent_id: Some(f.root_node_id.clone()),
            kind: RawKind::Turn,
            body: raw_body.into(),
        })
        .unwrap();
    let lenses = lenses();
    let updated = compact_turn_for_all(&store, &f.id, &raw, raw_body, &lenses).unwrap();
    assert_eq!(updated.len(), 3);
    let w = store.latest_level0(&f.id, AgentView::Worker).unwrap().unwrap();
    let p = store.latest_level0(&f.id, AgentView::Supervisor).unwrap().unwrap();
    let o = store.latest_level0(&f.id, AgentView::Observer).unwrap().unwrap();
    // 3 view が異なる粒度・内容を持つ (= 三角形が縮退していない)。
    let mut summaries = vec![w.summary.clone(), p.summary.clone(), o.summary.clone()];
    summaries.sort();
    summaries.dedup();
    assert_eq!(summaries.len(), 3);

    store
        .set_feature_status(&f.id, FeatureStatus::Done)
        .unwrap();
    assert_eq!(
        store.get_feature(&f.id).unwrap().status,
        FeatureStatus::Done
    );
}

#[tokio::test]
async fn parks_then_resumes_after_user_thrust() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let reg = registry(root.clone());
    let worker = Arc::new(MockLlmClient::new("worker"));
    let sup = Arc::new(MockLlmClient::new("sup"));
    let obs = Arc::new(MockLlmClient::new("obs"));

    // 1 ターン目: Z 軸が来ないので park 想定。
    worker.push(ScriptedTurn::new("DONE\n"));
    sup.push(ScriptedTurn::new(approve_json(0.9, "")));
    obs.push(ScriptedTurn::new(approve_json(0.9, "")));
    worker.push(ScriptedTurn::new(approve_json(0.9, "")));
    // 2 ターン目: Z 軸 Go が来て commit。
    worker.push(ScriptedTurn::new("DONE\n"));
    sup.push(ScriptedTurn::new(approve_json(0.9, "")));
    obs.push(ScriptedTurn::new(approve_json(0.9, "")));
    worker.push(ScriptedTurn::new(approve_json(0.9, "")));

    let trio = Trio::new(
        Agent::new(AgentRole::Worker, worker, "worker-system"),
        Agent::new(AgentRole::Supervisor, sup, "supervisor-system"),
        Agent::new(AgentRole::Observer, obs, "observer-system"),
    );
    let (tx, mut rx) = ThrustChannel::new();

    // Concierge は何も送っていない → park。
    let parked = trio
        .run_step(&[ChatMessage::user("noop")], &reg, &mut rx)
        .await
        .unwrap();
    assert!(matches!(parked.last, ConsensusOutcome::Parked { .. }));

    // ユーザー Z 軸 Go → 次の run_step で commit。
    tx.send(UserThrust::Go { strength: 1.0 }).unwrap();
    let committed = trio
        .run_step(&[ChatMessage::user("noop")], &reg, &mut rx)
        .await
        .unwrap();
    assert!(matches!(committed.last, ConsensusOutcome::Commit { .. }));
}

#[tokio::test]
async fn supervisor_initial_reject_then_eventual_approve() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let reg = registry(root.clone());
    let worker = Arc::new(MockLlmClient::new("worker"));
    let sup = Arc::new(MockLlmClient::new("sup"));
    let obs = Arc::new(MockLlmClient::new("obs"));

    // ループ 1 回目: Supervisor reject → 平面合意せず継続。
    worker.push(ScriptedTurn::new("DONE\n"));
    sup.push(ScriptedTurn::new(reject_json(0.9, "needs error handling")));
    obs.push(ScriptedTurn::new(approve_json(0.7, "")));
    worker.push(ScriptedTurn::new(approve_json(0.6, "")));
    // ループ 2 回目: Supervisor approve → 平面合意成立 → Z 軸 Go で commit。
    worker.push(ScriptedTurn::new("DONE\n"));
    sup.push(ScriptedTurn::new(approve_json(0.95, "fixed")));
    obs.push(ScriptedTurn::new(approve_json(0.8, "")));
    worker.push(ScriptedTurn::new(approve_json(0.8, "")));

    let trio = Trio::new(
        Agent::new(AgentRole::Worker, worker, "worker-system"),
        Agent::new(AgentRole::Supervisor, sup, "supervisor-system"),
        Agent::new(AgentRole::Observer, obs, "observer-system"),
    );
    let (tx, mut rx) = ThrustChannel::new();
    tx.send(UserThrust::Go { strength: 1.0 }).unwrap();

    let outcome = trio
        .run_step(&[ChatMessage::user("retry-task")], &reg, &mut rx)
        .await
        .unwrap();
    assert!(outcome.steps >= 2, "expected at least 2 iterations, got {}", outcome.steps);
    assert!(matches!(outcome.last, ConsensusOutcome::Commit { .. }));
}

#[tokio::test]
async fn worktree_then_self_review_then_commit() {
    // 既存 git repo を準備。
    let repo_dir = tempdir().unwrap();
    let repo_path = repo_dir.path().join("repo");
    std::fs::create_dir_all(&repo_path).unwrap();
    let repo = git2::Repository::init(&repo_path).unwrap();
    std::fs::write(repo_path.join("README.md"), "init\n").unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(std::path::Path::new("README.md")).unwrap();
    let tree_oid = idx.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = git2::Signature::now("tmoe", "tmoe@example").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

    // worktree を切り出して、Worker の代わりにファイルを書き込む (e2e 軸検証)。
    let handle = carve_worktree(&repo_path, "ulid42", None).unwrap();
    let new_path = handle.worktree_path.join("src/util.rs");
    std::fs::create_dir_all(new_path.parent().unwrap()).unwrap();
    std::fs::write(&new_path, "pub fn gcd(a:u64,b:u64)->u64{if b==0{a}else{gcd(b,a%b)}}\n").unwrap();

    let diff = working_diff_text(&handle).unwrap();
    assert!(diff.contains("util.rs"));
    assert!(diff.contains("gcd"));

    // self_review (Supervisor) が approve する。
    let sup = MockLlmClient::new("sup");
    sup.push(ScriptedTurn::new(approve_json(0.92, "diff is on-target and small")));
    let review =
        supervisor_review_diff(&sup, "supervisor-system", &diff, "add gcd to src/util.rs").await.unwrap();
    assert!(matches!(review, SelfReviewOutcome::Approved(_)));

    stage_all(&handle).unwrap();
    let oid = git_commit(&handle, "tmoe", "tmoe@example", "feat: add gcd").unwrap();
    let repo = git2::Repository::open(&handle.worktree_path).unwrap();
    let last = repo.find_commit(oid).unwrap();
    assert_eq!(last.message().unwrap_or(""), "feat: add gcd");
}

#[tokio::test]
async fn observer_can_warn_and_reject_for_loop_detection() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let reg = registry(root.clone());
    let worker = Arc::new(MockLlmClient::new("worker"));
    let sup = Arc::new(MockLlmClient::new("sup"));
    let obs = Arc::new(MockLlmClient::new("obs"));

    // Worker は同じ提案を 3 回繰り返す → Observer がループ判定で reject。
    for _ in 0..6 {
        worker.push(ScriptedTurn::new("DONE\n"));
        sup.push(ScriptedTurn::new(approve_json(0.9, "")));
        obs.push(ScriptedTurn::new(reject_json(0.95, "loop suspected")));
        worker.push(ScriptedTurn::new(approve_json(0.7, "")));
    }
    let trio = Trio::new(
        Agent::new(AgentRole::Worker, worker, "worker-system"),
        Agent::new(AgentRole::Supervisor, sup, "supervisor-system"),
        Agent::new(AgentRole::Observer, obs, "observer-system"),
    )
    .with_thresholds(tmoe_core::ConsensusThresholds {
        max_iter_per_step: 4,
        ..Default::default()
    });

    let (tx, mut rx) = ThrustChannel::new();
    tx.send(UserThrust::Go { strength: 1.0 }).unwrap();
    let outcome = trio
        .run_step(&[ChatMessage::user("loop-y")], &reg, &mut rx)
        .await
        .unwrap();
    // Observer の veto により 平面合意できず escalate。
    assert!(matches!(outcome.last, ConsensusOutcome::Escalated { .. }));
}
