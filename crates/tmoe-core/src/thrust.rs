//! Z 軸推進力のメッセージング。
//!
//! Concierge (= ユーザー I/O チャネル) は Trio に対し `UserThrust` を送出する。
//! Trio はこれを `z_thrust` 値として読み、**0 以下なら park** する。
//! park 中も Concierge は常時受付可能。

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// ユーザー由来の推進シグナル。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum UserThrust {
    /// 進めて良い (一定時間/ステップ数だけ前進可)。
    Go { strength: f32 },
    /// 一時停止 (z_thrust = 0)。
    Pause,
    /// 軌道修正指示。proposal を破棄して再ループする。
    Redirect { instruction: String },
    /// feature を中断する。
    Stop,
}

#[derive(Clone)]
pub struct ThrustSender(pub mpsc::UnboundedSender<UserThrust>);
pub struct ThrustReceiver(pub mpsc::UnboundedReceiver<UserThrust>);

impl ThrustSender {
    pub fn send(&self, t: UserThrust) -> Result<(), mpsc::error::SendError<UserThrust>> {
        self.0.send(t)
    }
}

pub struct ThrustChannel;

impl ThrustChannel {
    pub fn new() -> (ThrustSender, ThrustReceiver) {
        let (tx, rx) = mpsc::unbounded_channel();
        (ThrustSender(tx), ThrustReceiver(rx))
    }
}
