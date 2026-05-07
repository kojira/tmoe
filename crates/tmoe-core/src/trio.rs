//! Trio: 3 エージェント固定の合意制オーケストレータ。
//!
//! 前進条件 (3 + 1 モデル):
//!   plane_ok          (3 者全員 approve)
//! ∧ confidence_sum    >= confidence_sum_min
//! ∧ triangle_balance  >= triangle_balance_min
//! ∧ z_thrust          > 0  (ユーザー由来の Z 軸推進力)
//!
//! 多数決ではない: 1 人でも反対していれば前進しない。
//! 平面が均整していても Z が無ければ park し、Concierge は常時受付可能のまま入力を待つ。

use crate::agent::{single_agent_loop, AgentRole};
use crate::proposal::Proposal;
use crate::thrust::{ThrustReceiver, UserThrust};
use crate::vote::{triangle_balance, Vote};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tmoe_history::{render_prior_views_block, ViewProvider};
use tmoe_llm::{ChatMessage, LlmClient};
use tmoe_tools::ToolRegistry;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct ConsensusThresholds {
    pub confidence_sum_min: f32,
    pub triangle_balance_min: f32,
    pub max_iter_per_step: u32,
}

impl Default for ConsensusThresholds {
    fn default() -> Self {
        Self { confidence_sum_min: 2.4, triangle_balance_min: 0.6, max_iter_per_step: 8 }
    }
}

#[derive(Debug, Clone)]
pub enum ConsensusOutcome {
    Commit { proposal: Proposal, votes: [Vote; 3] },
    Parked { proposal: Proposal, votes: [Vote; 3] },
    Redirected { instruction: String },
    Stopped,
    Escalated { last_proposal: Proposal },
}

#[derive(Debug, Clone)]
pub struct TrioOutcome {
    pub steps: u32,
    pub last: ConsensusOutcome,
}

/// 1 つのエージェントの **パーソナリティ** = 役割 + LLM + システムプロンプト。
///
/// Trio は 3 つの Agent を保持する。プロンプトはエージェントに紐づく恒久的な属性であり、
/// `run_step` のたびに外から差し込むものではない (= ベクトルが時間を超えて保たれる)。
#[derive(Clone)]
pub struct Agent {
    pub role: AgentRole,
    pub llm: Arc<dyn LlmClient>,
    pub system: String,
}

impl Agent {
    pub fn new(role: AgentRole, llm: Arc<dyn LlmClient>, system: impl Into<String>) -> Self {
        Self { role, llm, system: system.into() }
    }

    /// tmoe-prompts の既定プロンプトでエージェントを構築する。
    pub fn with_default_personality(role: AgentRole, llm: Arc<dyn LlmClient>) -> Self {
        let system = match role {
            AgentRole::Worker => tmoe_prompts::WORKER_SYSTEM,
            AgentRole::Supervisor => tmoe_prompts::SUPERVISOR_SYSTEM,
            AgentRole::Observer => tmoe_prompts::OBSERVER_SYSTEM,
        };
        Self::new(role, llm, system)
    }
}

/// 3 つの Agent で構成される三角形。役割は固定 (Worker / Supervisor / Observer)。
pub struct Trio {
    pub worker: Agent,
    pub supervisor: Agent,
    pub observer: Agent,
    pub thresholds: ConsensusThresholds,
}

impl Trio {
    pub fn new(worker: Agent, supervisor: Agent, observer: Agent) -> Self {
        assert_eq!(worker.role, AgentRole::Worker, "Trio.worker must be Worker");
        assert_eq!(supervisor.role, AgentRole::Supervisor, "Trio.supervisor must be Supervisor");
        assert_eq!(observer.role, AgentRole::Observer, "Trio.observer must be Observer");
        Self { worker, supervisor, observer, thresholds: ConsensusThresholds::default() }
    }

    /// すべての Agent が同一 LLM クライアントを共有しつつ、tmoe-prompts の既定パーソナリティで
    /// 構築するショートカット。プロンプトの違いだけで「3 つの異なる方向性ベクトル」を表現する。
    pub fn from_shared_llm(llm: Arc<dyn LlmClient>) -> Self {
        Self::new(
            Agent::with_default_personality(AgentRole::Worker, llm.clone()),
            Agent::with_default_personality(AgentRole::Supervisor, llm.clone()),
            Agent::with_default_personality(AgentRole::Observer, llm),
        )
    }

    pub fn with_thresholds(mut self, thresholds: ConsensusThresholds) -> Self {
        self.thresholds = thresholds;
        self
    }

    /// 1 ステップ (= 1 つの proposal が commit/park/escalate 等になるまで) 進める。
    /// `thrust_rx` から Z 軸推進シグナルを drain する。`Pause` または `Go strength<=0` は park。
    /// `Redirect` は破棄して再ループ、`Stop` は中断。
    ///
    /// `views` が `Some` のとき、Supervisor / Observer / Worker 自己評価の vote プロンプトに
    /// 各エージェントの最新 level=0 view brief (PRIOR VIEWS ブロック) を prepend する。
    /// これにより Worker view が「書かれて誰も読まない」状態を回避し、Supervisor は縦断的な
    /// 進捗主張のサニティを、Observer は 3 view を並べたループ検出を行える。
    ///
    /// 平面合意できなかった場合、直前の Worker 出力 + Supervisor / Observer の差し戻し note を
    /// **次の試行の Worker への入力に追記**する (フィードバックループ)。これにより Worker は
    /// 同じ部分実装を繰り返さず、指摘を吸収して改訂できる。
    pub async fn run_step(
        &self,
        user_messages: &[ChatMessage],
        tools: &ToolRegistry,
        thrust_rx: &mut ThrustReceiver,
    ) -> anyhow::Result<TrioOutcome> {
        self.run_step_with_views(user_messages, tools, thrust_rx, None).await
    }

    /// `run_step` の view 注入版。`views` が None なら `run_step` と等価。
    pub async fn run_step_with_views(
        &self,
        user_messages: &[ChatMessage],
        tools: &ToolRegistry,
        thrust_rx: &mut ThrustReceiver,
        views: Option<&dyn ViewProvider>,
    ) -> anyhow::Result<TrioOutcome> {
        let mut steps = 0u32;
        let mut last_proposal = Proposal::empty();
        // 直前ターンのフィードバック (assistant 提案 + user 差し戻し)。空なら初回。
        let mut feedback: Vec<ChatMessage> = Vec::new();

        loop {
            steps += 1;
            if steps > self.thresholds.max_iter_per_step {
                return Ok(TrioOutcome {
                    steps,
                    last: ConsensusOutcome::Escalated { last_proposal },
                });
            }
            // 1) Worker が提案を作る + (権限内で) ツール実行。
            //    user_messages の後ろに直前ターンのフィードバックを連結する。
            let mut worker_input = user_messages.to_vec();
            worker_input.extend(feedback.clone());
            let pm = single_agent_loop(
                AgentRole::Worker,
                &self.worker.system,
                worker_input,
                self.worker.llm.as_ref(),
                tools,
            )
            .await?;
            let proposal = pm.proposal;
            last_proposal = proposal.clone();

            // 2) Supervisor / Observer / Worker 自己評価で 3 票を集める。
            //    views が指定されていれば各 vote プロンプトに PRIOR VIEWS ブロックを prepend し、
            //    Supervisor は Worker view から縦断的な進捗主張を、Observer は 3 view 並走で
            //    ループ兆候を検知できるようにする。
            let prior_block = views
                .map(render_prior_views_block)
                .filter(|s| !s.is_empty());
            let s_vote = ask_vote(
                self.supervisor.llm.as_ref(),
                &self.supervisor.system,
                &proposal,
                user_messages,
                prior_block.as_deref(),
            )
            .await?;
            let o_vote = ask_vote(
                self.observer.llm.as_ref(),
                &self.observer.system,
                &proposal,
                user_messages,
                prior_block.as_deref(),
            )
            .await?;
            // Worker 自己評価は Worker LLM に「自分の提案を 0..1 で confidence する」よう問い直す。
            let w_self = ask_vote(
                self.worker.llm.as_ref(),
                &self.worker.system,
                &proposal,
                user_messages,
                prior_block.as_deref(),
            )
            .await?;

            // 3) 平面合意の判定。
            //    完了判定は Worker の自己宣言 (proposal.done) ではなく **Supervisor の
            //    REQUIREMENT COVERAGE 判断** に委ねる。実機 LLM は DONE トークンを出し忘れがちで、
            //    かつ「要件全項目を満たしているか」は Worker の自己評価より Supervisor の方が
            //    一貫した基準で判断できるため。Observer も「未完了/逸脱なら reject」を持つ。
            let plane_ok = s_vote.approve && o_vote.approve && w_self.approve;
            let conf_sum = s_vote.confidence + o_vote.confidence + w_self.confidence;
            let balance = triangle_balance(s_vote.confidence, o_vote.confidence, w_self.confidence);
            let plane_passes =
                plane_ok && conf_sum >= self.thresholds.confidence_sum_min
                    && balance >= self.thresholds.triangle_balance_min;

            if !plane_passes {
                // 平面が歪んでいる、もしくは Worker 未完了 → 再試行。
                // 直前の Worker 出力と各エージェントの差し戻し note を次のターンの user 末尾に挿入し、
                // Worker が同じ部分実装を繰り返さないよう誘導する。
                let critique = format!(
                    "PREVIOUS PROPOSAL FAILED THE TRIANGLE. Address every point below and try again.\n\
                     - Supervisor (approve={}, conf={:.2}): {}\n\
                     - Observer   (approve={}, conf={:.2}): {}\n\
                     - Worker self (approve={}, conf={:.2}): {}\n\
                     Re-emit ALL required tool calls (including ones already attempted).",
                    s_vote.approve, s_vote.confidence, s_vote.note,
                    o_vote.approve, o_vote.confidence, o_vote.note,
                    w_self.approve, w_self.confidence, w_self.note,
                );
                feedback = vec![
                    ChatMessage::assistant(&proposal.raw_text),
                    ChatMessage::user(critique),
                ];
                continue;
            }

            // 4) Z 軸推進を確認。drain して最新のシグナルを採用。
            let z = drain_thrust(thrust_rx);
            match z {
                ZNet::GoPositive => {
                    return Ok(TrioOutcome {
                        steps,
                        last: ConsensusOutcome::Commit {
                            proposal,
                            votes: [w_self, s_vote, o_vote],
                        },
                    });
                }
                ZNet::Pause | ZNet::None => {
                    return Ok(TrioOutcome {
                        steps,
                        last: ConsensusOutcome::Parked {
                            proposal,
                            votes: [w_self, s_vote, o_vote],
                        },
                    });
                }
                ZNet::Redirect(instruction) => {
                    return Ok(TrioOutcome {
                        steps,
                        last: ConsensusOutcome::Redirected { instruction },
                    });
                }
                ZNet::Stop => {
                    return Ok(TrioOutcome {
                        steps,
                        last: ConsensusOutcome::Stopped,
                    });
                }
            }
        }
    }

    /// park 状態から、Concierge が次の Z 軸推進シグナルを送ってくるまで blocking で待つ。
    /// 待っている間も Concierge は受付可能なので、ユーザー視点では「ブロックしない」。
    pub async fn await_thrust(&self, thrust_rx: &mut ThrustReceiver) -> Option<UserThrust> {
        thrust_rx.0.recv().await
    }
}

enum ZNet {
    None,
    GoPositive,
    Pause,
    Redirect(String),
    Stop,
}

/// 受信キューから最新の thrust を取り出す。複数あれば最後のものが勝つ。
/// `Go { strength }` は `strength > 0.0` のときだけ前進と扱う (= Z 軸の符号判定)。
/// 大きさ自体は現在の前進判定では使わない (バイナリ go/no-go)。
fn drain_thrust(rx: &mut ThrustReceiver) -> ZNet {
    let mut latest = ZNet::None;
    while let Ok(t) = rx.0.try_recv() {
        latest = match t {
            UserThrust::Go { strength } if strength > 0.0 => ZNet::GoPositive,
            UserThrust::Go { .. } => ZNet::Pause,
            UserThrust::Pause => ZNet::Pause,
            UserThrust::Redirect { instruction } => ZNet::Redirect(instruction),
            UserThrust::Stop => ZNet::Stop,
        };
    }
    latest
}

async fn ask_vote(
    llm: &dyn LlmClient,
    system: &str,
    proposal: &Proposal,
    base_user: &[ChatMessage],
    prior_views: Option<&str>,
) -> anyhow::Result<Vote> {
    let mut messages = vec![ChatMessage::system(system)];
    if let Some(block) = prior_views {
        // PRIOR VIEWS は user メッセージとして base_user の前に置く。
        // system に混ぜないのは、各エージェントのパーソナリティ system は不変属性として
        // 保ちたいため (= ベクトルが時間を超えて保たれる)。
        messages.push(ChatMessage::user(block.to_string()));
    }
    messages.extend(base_user.iter().cloned());
    messages.push(ChatMessage::assistant(&proposal.raw_text));
    messages.push(ChatMessage::user(
        r#"上記の提案に対し、JSON で {"approve": bool, "confidence": 0.0-1.0, "note": "..."} を返してください。"#,
    ));
    let resp = llm
        .chat(tmoe_llm::ChatRequest { messages, ..Default::default() })
        .await?;
    parse_vote(&resp.content).ok_or_else(|| anyhow::anyhow!("vote not parseable: {}", resp.content))
}

/// LLM の vote 出力を 3 段で頑健にパース:
/// 1) JSON object を最初に見つけて strict serde_json
/// 2) 同じ payload を lenient_jsonify してから serde_json
/// 3) 個別フィールドを正規表現的に拾う recovery
/// 確信度が欠落した場合は中立値 0.5 を採用し、生粋の prose しか返さない LLM 向けに
/// 「approve/reject」キーワードと数値を fallback で組み合わせる。
pub(crate) fn parse_vote(text: &str) -> Option<Vote> {
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            if end > start {
                let payload = &text[start..=end];
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
                    if let Some(vote) = vote_from_value(&v) {
                        return Some(vote);
                    }
                }
                let lenient = crate::lenient_jsonify(payload);
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&lenient) {
                    if let Some(vote) = vote_from_value(&v) {
                        return Some(vote);
                    }
                }
            }
        }
    }
    // 3) recovery: 個別フィールド抽出。
    let approve = crate::extract_bool_field(text, "approve")
        .or_else(|| infer_approval_from_prose(text))?;
    let confidence = crate::extract_number_field(text, "confidence").unwrap_or(0.7) as f32;
    let note = crate::extract_simple_string_field(text, "note").unwrap_or_default();
    Some(Vote { approve, confidence, note })
}

fn vote_from_value(v: &serde_json::Value) -> Option<Vote> {
    let approve = v.get("approve").and_then(|x| x.as_bool())?;
    let confidence = v
        .get("confidence")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.7) as f32;
    let note = v
        .get("note")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    Some(Vote { approve, confidence, note })
}

/// 純粋な prose しか返さなかった LLM 用の最終フォールバック。
/// 「approve / yes / accept / ok」「reject / no / block / veto」をキーワードに見て判断する。
fn infer_approval_from_prose(text: &str) -> Option<bool> {
    let lower = text.to_lowercase();
    let positive = ["approve", "yes", "accept", "ok", "looks good", "lgtm"];
    let negative = ["reject", "no,", "block", "veto", "deny", "do not approve"];
    let neg_hit = negative.iter().any(|k| lower.contains(k));
    let pos_hit = positive.iter().any(|k| lower.contains(k));
    if neg_hit && !pos_hit {
        return Some(false);
    }
    if pos_hit && !neg_hit {
        return Some(true);
    }
    None
}

#[cfg(test)]
mod parse_vote_tests {
    use super::parse_vote;

    #[test]
    fn strict_object() {
        let v = parse_vote(r#"{"approve":true,"confidence":0.9,"note":"ok"}"#).unwrap();
        assert!(v.approve);
        assert!((v.confidence - 0.9).abs() < 1e-6);
    }

    #[test]
    fn json_object_inside_prose() {
        let v = parse_vote(r#"Looking good. {"approve": true, "confidence": 0.8, "note": "fine"} done."#).unwrap();
        assert!(v.approve);
    }

    #[test]
    fn lenient_handles_raw_newlines_in_note() {
        let v = parse_vote("{\"approve\": true, \"confidence\": 0.7, \"note\": \"line1\nline2\"}").unwrap();
        assert!(v.approve);
        assert!(v.note.contains("line1"));
    }

    #[test]
    fn recovery_when_object_missing_fields() {
        // approve は object 内に書いてあるが confidence は object 外で散らばる場合
        let text = r#"{"approve": true} confidence: 0.65 note: "ok""#;
        let v = parse_vote(text).unwrap();
        assert!(v.approve);
    }

    #[test]
    fn prose_only_fallback() {
        let v = parse_vote("I approve. Looks good.").unwrap();
        assert!(v.approve);
    }

    #[test]
    fn prose_only_negative() {
        let v = parse_vote("I have to reject this. The diff touches unrelated code.").unwrap();
        assert!(!v.approve);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thrust::ThrustChannel;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tmoe_llm::{MockLlmClient, ScriptedTurn};
    use tmoe_tools::{EditFileTool, ReadFileTool};

    fn registry(root: std::path::PathBuf) -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EditFileTool { root: root.clone() }));
        reg.register(Arc::new(ReadFileTool { root }));
        reg
    }

    fn approve(conf: f32) -> String {
        format!("{{\"approve\":true,\"confidence\":{conf},\"note\":\"ok\"}}")
    }
    fn reject(conf: f32) -> String {
        format!("{{\"approve\":false,\"confidence\":{conf},\"note\":\"no\"}}")
    }

    #[tokio::test]
    async fn trio_consensus_loop_terminates_on_commit() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let reg = registry(root.clone());
        let worker = Arc::new(MockLlmClient::new("worker"));
        let sup = Arc::new(MockLlmClient::new("sup"));
        let obs = Arc::new(MockLlmClient::new("obs"));
        // 1 ターンで commit する応答を仕込む。
        worker.push(ScriptedTurn::new(
            r#"提案
```json
{"tool":"edit_file","args":{"path":"hello.rs","content":"fn main(){}"}}
```
DONE"#,
        ));
        sup.push(ScriptedTurn::new(approve(0.9)));
        obs.push(ScriptedTurn::new(approve(0.85)));
        worker.push(ScriptedTurn::new(approve(0.85))); // worker.self_assess
        let trio = Trio::new(
            Agent::new(AgentRole::Worker, worker.clone(), "worker-system"),
            Agent::new(AgentRole::Supervisor, sup.clone(), "supervisor-system"),
            Agent::new(AgentRole::Observer, obs.clone(), "observer-system"),
        );

        let (tx, mut rx) = ThrustChannel::new();
        tx.send(UserThrust::Go { strength: 1.0 }).unwrap();

        let outcome = trio
            .run_step(&[ChatMessage::user("hello.rs を作って")], &reg, &mut rx)
            .await
            .unwrap();
        match outcome.last {
            ConsensusOutcome::Commit { proposal, .. } => {
                assert!(proposal.done);
                assert_eq!(proposal.tool_calls.len(), 1);
            }
            other => panic!("expected Commit, got {other:?}"),
        }
        // ファイルは Worker のツール呼び出しで実体が書き込まれている。
        assert!(root.join("hello.rs").exists());
    }

    #[tokio::test]
    async fn park_until_user_thrust() {
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
            Agent::new(AgentRole::Worker, worker.clone(), "worker-system"),
            Agent::new(AgentRole::Supervisor, sup.clone(), "supervisor-system"),
            Agent::new(AgentRole::Observer, obs.clone(), "observer-system"),
        );

        // Z 軸推進を流さない → park されるはず。
        let (_tx, mut rx) = ThrustChannel::new();

        let outcome = trio
            .run_step(&[ChatMessage::user("noop")], &reg, &mut rx)
            .await
            .unwrap();
        match outcome.last {
            ConsensusOutcome::Parked { .. } => {}
            other => panic!("expected Parked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn supervisor_veto_blocks_progress_then_escalates() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let reg = registry(root.clone());
        let worker = Arc::new(MockLlmClient::new("worker"));
        let sup = Arc::new(MockLlmClient::new("sup"));
        let obs = Arc::new(MockLlmClient::new("obs"));
        // Worker は何度も同じ提案を出し、Supervisor が常に reject。
        for _ in 0..10 {
            worker.push(ScriptedTurn::new("DONE\n"));
            sup.push(ScriptedTurn::new(reject(0.9)));
            obs.push(ScriptedTurn::new(approve(0.7)));
            worker.push(ScriptedTurn::new(approve(0.5)));
        }
        let trio = Trio::new(
            Agent::new(AgentRole::Worker, worker.clone(), "worker-system"),
            Agent::new(AgentRole::Supervisor, sup.clone(), "supervisor-system"),
            Agent::new(AgentRole::Observer, obs.clone(), "observer-system"),
        )
        .with_thresholds(ConsensusThresholds {
            max_iter_per_step: 3,
            ..Default::default()
        });

        let (tx, mut rx) = ThrustChannel::new();
        tx.send(UserThrust::Go { strength: 1.0 }).unwrap();

        let outcome = trio
            .run_step(&[ChatMessage::user("noop")], &reg, &mut rx)
            .await
            .unwrap();
        match outcome.last {
            ConsensusOutcome::Escalated { .. } => {}
            other => panic!("expected Escalated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn supervisor_and_observer_receive_prior_views_block() {
        use tmoe_history::{AgentView, ViewProvider};

        // 各 view を一意な合言葉で返すフェイク ViewProvider。
        // Trio が PRIOR VIEWS ブロックを各 vote プロンプトに本当に prepend したかは、
        // MockLlmClient の calls() に同合言葉が現れるかで検証する。
        struct FakeViews;
        impl ViewProvider for FakeViews {
            fn brief(&self, a: AgentView) -> Option<String> {
                Some(match a {
                    AgentView::Worker => "WORKER-NARRATIVE-X9".into(),
                    AgentView::Supervisor => "SUPERVISOR-NARRATIVE-Y8".into(),
                    AgentView::Observer => "OBSERVER-NARRATIVE-Z7".into(),
                })
            }
        }

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
            Agent::new(AgentRole::Worker, worker.clone(), "worker-system"),
            Agent::new(AgentRole::Supervisor, sup.clone(), "supervisor-system"),
            Agent::new(AgentRole::Observer, obs.clone(), "observer-system"),
        );

        let (tx, mut rx) = ThrustChannel::new();
        tx.send(UserThrust::Go { strength: 1.0 }).unwrap();

        let views: &dyn ViewProvider = &FakeViews;
        let _ = trio
            .run_step_with_views(
                &[ChatMessage::user("noop")],
                &reg,
                &mut rx,
                Some(views),
            )
            .await
            .unwrap();

        // Supervisor の vote 呼び出しに 3 view 全部の合言葉が現れているか。
        // ここが空なら Worker view は依然「書かれて誰も読まない」状態を意味する。
        let sup_calls = sup.calls();
        assert_eq!(sup_calls.len(), 1, "supervisor should be asked exactly once for vote");
        let sup_user_text: String = sup_calls[0]
            .messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(sup_user_text.contains("WORKER-NARRATIVE-X9"),
            "supervisor vote prompt missing Worker view: {sup_user_text}");
        assert!(sup_user_text.contains("SUPERVISOR-NARRATIVE-Y8"));
        assert!(sup_user_text.contains("OBSERVER-NARRATIVE-Z7"));

        // Observer も同様。
        let obs_calls = obs.calls();
        let obs_user_text: String = obs_calls[0]
            .messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(obs_user_text.contains("WORKER-NARRATIVE-X9"));
        assert!(obs_user_text.contains("SUPERVISOR-NARRATIVE-Y8"));
        assert!(obs_user_text.contains("OBSERVER-NARRATIVE-Z7"));
    }

    #[tokio::test]
    async fn no_prior_views_when_provider_absent() {
        // run_step (= views=None) は PRIOR VIEWS ブロックを混入させてはいけない。
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
            Agent::new(AgentRole::Worker, worker.clone(), "worker-system"),
            Agent::new(AgentRole::Supervisor, sup.clone(), "supervisor-system"),
            Agent::new(AgentRole::Observer, obs.clone(), "observer-system"),
        );

        let (tx, mut rx) = ThrustChannel::new();
        tx.send(UserThrust::Go { strength: 1.0 }).unwrap();
        let _ = trio.run_step(&[ChatMessage::user("noop")], &reg, &mut rx).await.unwrap();

        let sup_text: String = sup.calls()[0]
            .messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!sup_text.contains("PRIOR VIEWS"));
    }

    #[tokio::test]
    async fn redirect_breaks_loop_with_redirected_outcome() {
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
            Agent::new(AgentRole::Worker, worker.clone(), "worker-system"),
            Agent::new(AgentRole::Supervisor, sup.clone(), "supervisor-system"),
            Agent::new(AgentRole::Observer, obs.clone(), "observer-system"),
        );

        let (tx, mut rx) = ThrustChannel::new();
        tx.send(UserThrust::Redirect { instruction: "ファイル名を変更".into() })
            .unwrap();

        let outcome = trio
            .run_step(&[ChatMessage::user("noop")], &reg, &mut rx)
            .await
            .unwrap();
        match outcome.last {
            ConsensusOutcome::Redirected { instruction } => {
                assert_eq!(instruction, "ファイル名を変更");
            }
            other => panic!("expected Redirected, got {other:?}"),
        }
    }
}
