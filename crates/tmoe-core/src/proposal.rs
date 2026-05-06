use serde::{Deserialize, Serialize};
use tmoe_tools::ToolCall;

/// Worker が 1 ステップで生成する提案。
/// 完了 (`DONE`) かツール呼び出しの 0..N 件、もしくは note のみのいずれか。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Proposal {
    /// LLM の生テキスト (デバッグ・履歴用)。
    pub raw_text: String,
    /// 抽出されたツール呼び出し (順序保持)。空ベクなら呼び出しなし。
    pub tool_calls: Vec<ToolCall>,
    /// Worker が「完了」を宣言したか。
    pub done: bool,
    /// 自由記述メモ。Supervisor / Observer のレビューに渡す。
    pub note: String,
}

impl Proposal {
    pub fn empty() -> Self {
        Self {
            raw_text: String::new(),
            tool_calls: Vec::new(),
            done: false,
            note: String::new(),
        }
    }
}
