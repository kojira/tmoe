//! 合意プロトコルの投票・前進判定 (スタブ)。
//!
//! 前進条件: plane_ok (3 者全員 approve) ∧ confidence_sum 閾値 ∧ triangle_balance 閾値
//!         ∧ z_thrust > 0 (ユーザー由来の推進)
//! どれか欠ければ park 状態に入る。

#[derive(Debug, Clone, Copy)]
pub struct Vote {
    pub approve: bool,
    pub confidence: f32,
}
