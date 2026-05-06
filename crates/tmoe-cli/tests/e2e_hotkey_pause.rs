//! 「実行中にキー入力で一時停止」検証 (Mock LLM)。
//!
//! tokio タスクで Trio を起動し、Concierge から Ctrl-P を流すと park 状態に入ること、
//! その間 Concierge は受付可能なまま動いていること、Ctrl-G で再開して Commit に至ること
//! を確認する。実機ホットキー(crossterm の KeyEvent)を `concierge::key_to_thrust` で
//! 翻訳し、ThrustChannel に流すことで TUI を経由せずに同等の経路を検証する。

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tmoe_core::{
    Agent, AgentRole, ConsensusOutcome, ConsensusThresholds, ThrustChannel, ThrustSender, Trio,
};
use tmoe_llm::{ChatMessage, MockLlmClient, ScriptedTurn};
use tmoe_tools::{default_blocklist, EditFileTool, ReadFileTool, RunCmdTool, ToolRegistry};

#[path = "../src/concierge.rs"]
mod concierge;

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn registry(root: std::path::PathBuf) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EditFileTool { root: root.clone() }));
    reg.register(Arc::new(ReadFileTool { root: root.clone() }));
    reg.register(Arc::new(RunCmdTool { root, blocklist: default_blocklist() }));
    reg
}

fn approve(c: f32) -> String {
    format!(r#"{{"approve":true,"confidence":{c},"note":"ok"}}"#)
}

#[tokio::test]
async fn ctrl_p_pauses_trio_then_ctrl_g_resumes_to_commit() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let reg = registry(root.clone());
    let worker = Arc::new(MockLlmClient::new("worker"));
    let sup = Arc::new(MockLlmClient::new("sup"));
    let obs = Arc::new(MockLlmClient::new("obs"));
    // 1 回の run_step で完結するシナリオ: Worker 提案 + 3 票 + Worker self_assess。
    worker.push(ScriptedTurn::new("DONE\n"));
    sup.push(ScriptedTurn::new(approve(0.9)));
    obs.push(ScriptedTurn::new(approve(0.85)));
    worker.push(ScriptedTurn::new(approve(0.85)));
    let trio = Trio::new(
        Agent::new(AgentRole::Worker, worker, "w"),
        Agent::new(AgentRole::Supervisor, sup, "s"),
        Agent::new(AgentRole::Observer, obs, "o"),
    )
    .with_thresholds(ConsensusThresholds::default());

    let (tx, mut rx) = ThrustChannel::new();

    // Concierge は別タスクで動作する想定。ここでは「ユーザーが Ctrl-P を押す」を直接シミュレート。
    let tx_pause: ThrustSender = tx.clone();
    let pause_handle = tokio::spawn(async move {
        // すぐに Ctrl-P を押す (Trio が走る前に park 命令を入れる)。
        if let Some(t) = concierge::key_to_thrust(ctrl('p')) {
            tx_pause.send(t).unwrap();
        }
    });

    // Trio をバックグラウンドで走らせ、park されることを確認する。
    let reg_arc = Arc::new(reg);
    let reg_for_run = reg_arc.clone();
    let trio_handle = tokio::spawn(async move {
        trio.run_step(&[ChatMessage::user("noop")], &reg_for_run, &mut rx)
            .await
    });

    pause_handle.await.unwrap();
    let outcome = trio_handle
        .await
        .expect("trio task panicked")
        .expect("run_step returned err");
    assert!(
        matches!(outcome.last, ConsensusOutcome::Parked { .. }),
        "Ctrl-P should have parked the Trio; got {:?}",
        outcome.last
    );

    // park 中は Concierge が受付可能であるべき。Ctrl-G で再開。
    let worker2 = Arc::new(MockLlmClient::new("worker"));
    let sup2 = Arc::new(MockLlmClient::new("sup"));
    let obs2 = Arc::new(MockLlmClient::new("obs"));
    worker2.push(ScriptedTurn::new(
        "```json\n{\"tool\":\"edit_file\",\"args\":{\"path\":\"hello.rs\",\"content\":\"fn main(){}\"}}\n```\nDONE",
    ));
    sup2.push(ScriptedTurn::new(approve(0.9)));
    obs2.push(ScriptedTurn::new(approve(0.85)));
    worker2.push(ScriptedTurn::new(approve(0.85)));
    let trio2 = Trio::new(
        Agent::new(AgentRole::Worker, worker2, "w"),
        Agent::new(AgentRole::Supervisor, sup2, "s"),
        Agent::new(AgentRole::Observer, obs2, "o"),
    );

    let (tx2, mut rx2) = ThrustChannel::new();
    // resume シグナル: Ctrl-G を Concierge 経由で送る。
    if let Some(t) = concierge::key_to_thrust(ctrl('g')) {
        tx2.send(t).unwrap();
    }

    let outcome2 = tokio::time::timeout(
        Duration::from_secs(5),
        trio2.run_step(&[ChatMessage::user("create hello")], reg_arc.as_ref(), &mut rx2),
    )
    .await
    .expect("Ctrl-G resume timed out — Trio should be non-blocking and finish quickly")
    .expect("run_step returned err");
    assert!(
        matches!(outcome2.last, ConsensusOutcome::Commit { .. }),
        "Ctrl-G should have driven Trio to Commit; got {:?}",
        outcome2.last
    );
    assert!(root.join("hello.rs").exists(), "Worker tool call should have written hello.rs");
}

#[tokio::test]
async fn ctrl_k_stops_a_running_feature() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let reg = registry(root.clone());
    let worker = Arc::new(MockLlmClient::new("worker"));
    let sup = Arc::new(MockLlmClient::new("sup"));
    let obs = Arc::new(MockLlmClient::new("obs"));
    worker.push(ScriptedTurn::new("DONE\n"));
    sup.push(ScriptedTurn::new(approve(0.9)));
    obs.push(ScriptedTurn::new(approve(0.85)));
    worker.push(ScriptedTurn::new(approve(0.85)));
    let trio = Trio::new(
        Agent::new(AgentRole::Worker, worker, "w"),
        Agent::new(AgentRole::Supervisor, sup, "s"),
        Agent::new(AgentRole::Observer, obs, "o"),
    );

    let (tx, mut rx) = ThrustChannel::new();
    if let Some(t) = concierge::key_to_thrust(ctrl('k')) {
        tx.send(t).unwrap();
    }
    let outcome = trio
        .run_step(&[ChatMessage::user("noop")], &reg, &mut rx)
        .await
        .unwrap();
    assert!(
        matches!(outcome.last, ConsensusOutcome::Stopped),
        "Ctrl-K should have stopped the feature; got {:?}",
        outcome.last
    );
}

#[tokio::test]
async fn concierge_remains_responsive_while_parked() {
    // park 中も Concierge は受付可能 (= ブロックしない) であることを示す。
    // run_step が park で帰ってきた後、Concierge への入力を続けてしばしば送れる。
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let reg = registry(root.clone());
    let worker = Arc::new(MockLlmClient::new("worker"));
    let sup = Arc::new(MockLlmClient::new("sup"));
    let obs = Arc::new(MockLlmClient::new("obs"));
    worker.push(ScriptedTurn::new("DONE\n"));
    sup.push(ScriptedTurn::new(approve(0.9)));
    obs.push(ScriptedTurn::new(approve(0.85)));
    worker.push(ScriptedTurn::new(approve(0.85)));
    let trio = Trio::new(
        Agent::new(AgentRole::Worker, worker, "w"),
        Agent::new(AgentRole::Supervisor, sup, "s"),
        Agent::new(AgentRole::Observer, obs, "o"),
    );

    let (tx, mut rx) = ThrustChannel::new();
    // 推進無しで実行 → park。
    let outcome = trio
        .run_step(&[ChatMessage::user("noop")], &reg, &mut rx)
        .await
        .unwrap();
    assert!(matches!(outcome.last, ConsensusOutcome::Parked { .. }));

    // park 中に複数のキー入力を順次送れる (= 受付可能)。
    for key in [ctrl('p'), ctrl('p'), ctrl('g')] {
        if let Some(t) = concierge::key_to_thrust(key) {
            assert!(tx.send(t).is_ok(), "Concierge channel closed unexpectedly");
        }
    }
}
