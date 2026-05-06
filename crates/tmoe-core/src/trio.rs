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

use crate::agent::{parse_proposal, single_agent_loop, AgentRole};
use crate::proposal::Proposal;
use crate::thrust::{ThrustReceiver, UserThrust};
use crate::vote::{triangle_balance, Vote};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
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

/// 3 エージェント = 3 LLM クライアントを同梱する。3 LLM が同一インスタンスでも、
/// プロンプトを変えれば「3 つの異なる方向性ベクトル」を背負える (= 平面が縮退しない)。
pub struct Trio {
    pub worker_llm: Arc<dyn LlmClient>,
    pub supervisor_llm: Arc<dyn LlmClient>,
    pub observer_llm: Arc<dyn LlmClient>,
    pub thresholds: ConsensusThresholds,
}

impl Trio {
    pub fn new(
        worker_llm: Arc<dyn LlmClient>,
        supervisor_llm: Arc<dyn LlmClient>,
        observer_llm: Arc<dyn LlmClient>,
    ) -> Self {
        Self {
            worker_llm,
            supervisor_llm,
            observer_llm,
            thresholds: ConsensusThresholds::default(),
        }
    }

    pub fn with_thresholds(mut self, thresholds: ConsensusThresholds) -> Self {
        self.thresholds = thresholds;
        self
    }

    /// 1 ステップ (= 1 つの proposal が commit/park/escalate 等になるまで) 進める。
    /// `thrust_rx` から Z 軸推進シグナルを drain する。`Pause` または `Go strength<=0` は park。
    /// `Redirect` は破棄して再ループ、`Stop` は中断。
    pub async fn run_step(
        &self,
        worker_prompt: &str,
        supervisor_prompt: &str,
        observer_prompt: &str,
        user_messages: &[ChatMessage],
        tools: &ToolRegistry,
        thrust_rx: &mut ThrustReceiver,
    ) -> anyhow::Result<TrioOutcome> {
        let mut steps = 0u32;
        let mut last_proposal = Proposal::empty();

        loop {
            steps += 1;
            if steps > self.thresholds.max_iter_per_step {
                return Ok(TrioOutcome {
                    steps,
                    last: ConsensusOutcome::Escalated { last_proposal },
                });
            }
            // 1) Worker が提案を作る + (権限内で) ツール実行。
            let pm = single_agent_loop(
                AgentRole::Worker,
                worker_prompt,
                user_messages.to_vec(),
                self.worker_llm.as_ref(),
                tools,
            )
            .await?;
            let proposal = pm.proposal;
            last_proposal = proposal.clone();

            // 2) Supervisor / Observer / Worker 自己評価で 3 票を集める。
            let s_vote = ask_vote(
                self.supervisor_llm.as_ref(),
                supervisor_prompt,
                &proposal,
                user_messages,
            )
            .await?;
            let o_vote = ask_vote(
                self.observer_llm.as_ref(),
                observer_prompt,
                &proposal,
                user_messages,
            )
            .await?;
            // Worker 自己評価は Worker LLM に「自分の提案を 0..1 で confidence する」よう問い直す。
            let w_self = ask_vote(
                self.worker_llm.as_ref(),
                worker_prompt,
                &proposal,
                user_messages,
            )
            .await?;

            // 3) 平面合意の判定。
            let plane_ok = s_vote.approve && o_vote.approve && w_self.approve;
            let conf_sum = s_vote.confidence + o_vote.confidence + w_self.confidence;
            let balance = triangle_balance(s_vote.confidence, o_vote.confidence, w_self.confidence);
            let plane_passes =
                plane_ok && conf_sum >= self.thresholds.confidence_sum_min
                    && balance >= self.thresholds.triangle_balance_min;

            if !plane_passes {
                // 平面が歪んでいるので再試行 (Worker への次の試行は同 prompt + 票の note を吸収して再生成)。
                continue;
            }

            // 4) Z 軸推進を確認。drain して最新のシグナルを採用。
            let z = drain_thrust(thrust_rx);
            match z {
                ZNet::GoPositive(_) => {
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

#[allow(dead_code)]
enum ZNet {
    None,
    GoPositive(f32),
    Pause,
    Redirect(String),
    Stop,
}

/// 受信キューから最新の thrust を取り出す。複数あれば最後のものが勝つ。
fn drain_thrust(rx: &mut ThrustReceiver) -> ZNet {
    let mut latest = ZNet::None;
    while let Ok(t) = rx.0.try_recv() {
        latest = match t {
            UserThrust::Go { strength } if strength > 0.0 => ZNet::GoPositive(strength),
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
) -> anyhow::Result<Vote> {
    let mut messages = vec![ChatMessage::system(system)];
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

fn parse_vote(text: &str) -> Option<Vote> {
    // 簡易: JSON object を 1 つ取り出す。
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    let payload = &text[start..=end];
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let approve = v.get("approve")?.as_bool()?;
    let confidence = v.get("confidence")?.as_f64()? as f32;
    let note = v
        .get("note")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    Some(Vote { approve, confidence, note })
}

// proposal を再パースしたい場合の薄いヘルパ (Phase 5 以降で使用予定)。
#[allow(dead_code)]
fn refresh_proposal(text: &str) -> Proposal {
    parse_proposal(text)
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
        let trio = Trio::new(worker.clone(), sup.clone(), obs.clone());

        let (tx, mut rx) = ThrustChannel::new();
        tx.send(UserThrust::Go { strength: 1.0 }).unwrap();

        let outcome = trio
            .run_step(
                "worker-system",
                "supervisor-system",
                "observer-system",
                &[ChatMessage::user("hello.rs を作って")],
                &reg,
                &mut rx,
            )
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
        let trio = Trio::new(worker.clone(), sup.clone(), obs.clone());

        // Z 軸推進を流さない → park されるはず。
        let (_tx, mut rx) = ThrustChannel::new();

        let outcome = trio
            .run_step(
                "worker",
                "sup",
                "obs",
                &[ChatMessage::user("noop")],
                &reg,
                &mut rx,
            )
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
        let trio = Trio::new(worker.clone(), sup.clone(), obs.clone())
            .with_thresholds(ConsensusThresholds {
                max_iter_per_step: 3,
                ..Default::default()
            });

        let (tx, mut rx) = ThrustChannel::new();
        tx.send(UserThrust::Go { strength: 1.0 }).unwrap();

        let outcome = trio
            .run_step(
                "worker",
                "sup",
                "obs",
                &[ChatMessage::user("noop")],
                &reg,
                &mut rx,
            )
            .await
            .unwrap();
        match outcome.last {
            ConsensusOutcome::Escalated { .. } => {}
            other => panic!("expected Escalated, got {other:?}"),
        }
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
        let trio = Trio::new(worker.clone(), sup.clone(), obs.clone());

        let (tx, mut rx) = ThrustChannel::new();
        tx.send(UserThrust::Redirect { instruction: "ファイル名を変更".into() })
            .unwrap();

        let outcome = trio
            .run_step(
                "worker",
                "sup",
                "obs",
                &[ChatMessage::user("noop")],
                &reg,
                &mut rx,
            )
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
