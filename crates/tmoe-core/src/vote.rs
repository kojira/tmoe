//! 合意プロトコルの投票・前進判定。
//!
//! 前進条件: plane_ok (3 者全員 approve) ∧ confidence_sum 閾値 ∧ triangle_balance 閾値
//!         ∧ z_thrust > 0 (ユーザー由来の推進)
//! どれか欠ければ park 状態に入る。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Vote {
    pub approve: bool,
    pub confidence: f32,
    pub note: String,
}

impl Vote {
    pub fn approve(confidence: f32, note: impl Into<String>) -> Self {
        Self { approve: true, confidence, note: note.into() }
    }
    pub fn reject(confidence: f32, note: impl Into<String>) -> Self {
        Self { approve: false, confidence, note: note.into() }
    }
}

/// 三角形の歪み: min(a,b,c) / max(a,b,c)。
/// 1.0 = 完全に均整 (正三角形)。0 = 縮退。
pub fn triangle_balance(a: f32, b: f32, c: f32) -> f32 {
    let arr = [a, b, c];
    let min = arr.iter().cloned().fold(f32::INFINITY, f32::min);
    let max = arr.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    if max <= 0.0 {
        return 0.0;
    }
    (min / max).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_equal_is_one() {
        assert!((triangle_balance(0.8, 0.8, 0.8) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn balance_skewed_is_low() {
        assert!(triangle_balance(0.9, 0.9, 0.1) < 0.2);
    }

    #[test]
    fn balance_zero_max_returns_zero() {
        assert_eq!(triangle_balance(0.0, 0.0, 0.0), 0.0);
    }
}
