//! tmoe-core: Trio オーケストレータ。
//!
//! 3 エージェント (Worker / Supervisor / Observer) は線形独立な方向性ベクトルを背負い、
//! 1 枚の合意平面 (2D) を張る。前進は **平面合意 × Z 軸推進 (ユーザー)** の積で決まる:
//! どちらかがゼロなら前進量はゼロ。Concierge は 4 人目のエージェントではなく、ユーザーの
//! Z 軸推進力を平面に伝達する I/O チャネルとして位置付ける。
//!
//! 「3」という個数は数学的安定性に基づいて固定: ①同一直線上にない 3 点が平面を一意に決定
//! ②三角形は最小の剛体多角形 ③3+1 で 3 次元空間が完成する。社会・宗教アナロジーは用いない。

pub mod agent;
pub mod proposal;
pub mod self_review;
pub mod thrust;
pub mod trio;
pub mod vote;

pub use agent::{
    run_worker_until_verified, single_agent_loop, AgentRole, ParsedToolCall, ProgressVerifier,
    ProposalMessage, VerifierOutcome, WorkerRunResult,
};
pub(crate) use agent::{extract_bool_field, extract_number_field, extract_simple_string_field, lenient_jsonify};
pub use proposal::Proposal;
pub use self_review::{supervisor_review_diff, SelfReviewOutcome};
pub use thrust::{ThrustChannel, ThrustReceiver, ThrustSender, UserThrust};
pub use trio::{Agent, ConsensusOutcome, ConsensusThresholds, Trio, TrioOutcome};
pub use vote::{triangle_balance, Vote};
