//! 単体エージェントの行動ループ (Phase 3)。
//!
//! LLM が生成した Worker 提案を `Proposal` に解釈し、ツール呼び出しがあれば実行する。
//! Phase 4 でこのループは Trio オーケストレータに組み込まれる。

use crate::proposal::Proposal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tmoe_llm::{ChatMessage, ChatRequest, LlmClient};
use tmoe_tools::{PermissionProfile, ToolCall, ToolError, ToolRegistry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentRole {
    Worker,
    Supervisor,
    Observer,
}

impl AgentRole {
    pub fn permission_profile(self) -> PermissionProfile {
        match self {
            AgentRole::Worker => PermissionProfile::worker(),
            AgentRole::Supervisor => PermissionProfile::supervisor(),
            AgentRole::Observer => PermissionProfile::observer(),
        }
    }
}

/// LLM 出力 1 件分から抽出された (中間表現) ツール呼び出し。
#[derive(Debug, Clone)]
pub struct ParsedToolCall(pub ToolCall);

/// LLM の生テキストから `Proposal` を抽出する。
///
/// 抽出規則:
/// - `DONE` を 1 行で含めば `done = true`
/// - JSON を ```json ... ``` または独立した object として認識し、`{"tool":"name","args":{...}}` の
///   形ならツール呼び出しとして取り込む。複数あれば順序保持で取り込む
/// - その他テキストは `note` に蓄積
pub fn parse_proposal(text: &str) -> Proposal {
    let mut tool_calls = Vec::new();
    let mut note_lines: Vec<String> = Vec::new();
    let mut done = false;

    // 簡易フェンス対応: ```json ... ``` を取り出す。
    let mut chunks: Vec<String> = Vec::new();
    let mut buf = text;
    while let Some(start) = buf.find("```") {
        let after = &buf[start + 3..];
        let lang_end = after.find(['\n', '\r']).unwrap_or(after.len());
        let _lang = &after[..lang_end];
        let rest = &after[lang_end..];
        if let Some(end) = rest.find("```") {
            chunks.push(rest[..end].trim().to_string());
            buf = &rest[end + 3..];
        } else {
            // 閉じフェンスなし → 残りすべてを 1 チャンクとして取り込む。
            chunks.push(rest.trim().to_string());
            buf = "";
        }
    }
    // フェンスのテキストと残テキストの双方からツール呼び出しを抽出する。
    for chunk in chunks {
        if let Some(call) = try_parse_tool_call(&chunk) {
            tool_calls.push(call);
        }
    }

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "DONE" {
            done = true;
            continue;
        }
        if trimmed.starts_with("```") {
            continue;
        }
        if let Some(call) = try_parse_tool_call(trimmed) {
            // インラインで {"tool":...} だけが書かれた行も拾う。
            if !tool_calls.contains(&call) {
                tool_calls.push(call);
            }
            continue;
        }
        note_lines.push(line.to_string());
    }

    Proposal {
        raw_text: text.to_string(),
        tool_calls,
        done,
        note: note_lines.join("\n").trim().to_string(),
    }
}

fn try_parse_tool_call(text: &str) -> Option<ToolCall> {
    // 必要最小限: "tool" と "args" を含むこと。
    if !(text.contains("\"tool\"") && text.contains("\"args\"")) {
        return None;
    }
    // 1) 厳格 JSON
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
        if let (Some(name), Some(args)) = (v.get("tool").and_then(|x| x.as_str()), v.get("args"))
        {
            return Some(ToolCall { name: name.to_string(), args: args.clone() });
        }
    }
    // 2) 文字列値内の生改行・タブのみエスケープした緩い JSON
    let lenient = lenient_jsonify(text);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&lenient) {
        if let (Some(name), Some(args)) = (v.get("tool").and_then(|x| x.as_str()), v.get("args"))
        {
            return Some(ToolCall { name: name.to_string(), args: args.clone() });
        }
    }
    // 3) 構造ベース recovery: tool/path は単純文字列、content は object の閉じから逆推定。
    recover_tool_call(text)
}

/// `"<key>":"<value>"` 形式から、value を抽出する。値内に \" を含む場合は \" として取り扱う。
/// 生の `"` は終端と判断するため、複数行に渡る巨大文字列の抽出には使えない (= path / tool 用)。
pub(crate) fn extract_simple_string_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let mut search_from = 0;
    while let Some(rel) = text[search_from..].find(&needle) {
        let key_end = search_from + rel + needle.len();
        let after_key = &text[key_end..];
        let colon = after_key.find(':')?;
        let after_colon = after_key[colon + 1..].trim_start();
        if !after_colon.starts_with('"') {
            search_from = key_end;
            continue;
        }
        let body = &after_colon[1..];
        // \" をエスケープとして扱いつつ終端 " を探す。
        let mut out = String::new();
        let mut prev_bs = false;
        for c in body.chars() {
            if prev_bs {
                match c {
                    '"' | '\\' | '/' => out.push(c),
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
                prev_bs = false;
                continue;
            }
            if c == '\\' {
                prev_bs = true;
                continue;
            }
            if c == '"' {
                return Some(out);
            }
            out.push(c);
        }
        return None;
    }
    None
}

/// `"content": "<...>"` を、外側の args/object の閉じ波括弧から逆算して切り出す。
/// 値内部に生 `"` を含む LLM 出力 (Rust の `"FizzBuzz"` 等) を救うための仕組み。
fn extract_content_field_lossy(text: &str) -> Option<String> {
    let key_pos = text.find("\"content\"")?;
    let after_key = &text[key_pos + "\"content\"".len()..];
    let colon = after_key.find(':')?;
    let after_colon = after_key[colon + 1..].trim_start();
    if !after_colon.starts_with('"') {
        return None;
    }
    let inner = &after_colon[1..];

    // 末尾フェンス・空白・閉じ括弧を順に剥がし、最後に残る `"` を文字列終端とみなす。
    let trimmed = inner.trim_end();
    let trimmed = trimmed.trim_end_matches('`');
    let trimmed = trimmed.trim_end();
    let mut s = trimmed.to_string();
    // 末尾の `}` を 2 つまで剥がす (= args object と外側 tool object の閉じ)。
    for _ in 0..2 {
        let t = s.trim_end();
        if t.ends_with('}') {
            s = t[..t.len() - 1].to_string();
        } else {
            break;
        }
    }
    let s = s.trim_end();
    if !s.ends_with('"') {
        return None;
    }
    let core = &s[..s.len() - 1];
    // 既に \n / \\ などでエスケープされている場合があるので、JSON エスケープを最低限解く。
    Some(unescape_json_string(core))
}

/// `"<key>"\s*:\s*<bool>` から bool を取り出す。
pub(crate) fn extract_bool_field(text: &str, key: &str) -> Option<bool> {
    let needle = format!("\"{key}\"");
    let mut from = 0;
    while let Some(rel) = text[from..].find(&needle) {
        let key_end = from + rel + needle.len();
        let after = text[key_end..].trim_start_matches(|c: char| c.is_whitespace());
        let after = after.strip_prefix(':')?.trim_start_matches(|c: char| c.is_whitespace());
        if let Some(rest) = after.strip_prefix("true") {
            // ensure word boundary
            if rest.chars().next().map_or(true, |c| !c.is_alphanumeric() && c != '_') {
                return Some(true);
            }
        }
        if let Some(rest) = after.strip_prefix("false") {
            if rest.chars().next().map_or(true, |c| !c.is_alphanumeric() && c != '_') {
                return Some(false);
            }
        }
        from = key_end;
    }
    None
}

/// `"<key>"\s*:\s*<number>` から数値を取り出す。
pub(crate) fn extract_number_field(text: &str, key: &str) -> Option<f64> {
    let needle = format!("\"{key}\"");
    let mut from = 0;
    while let Some(rel) = text[from..].find(&needle) {
        let key_end = from + rel + needle.len();
        let after = text[key_end..].trim_start_matches(|c: char| c.is_whitespace());
        let after = after.strip_prefix(':')?.trim_start_matches(|c: char| c.is_whitespace());
        let mut end = 0usize;
        for (i, c) in after.char_indices() {
            if c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E' {
                end = i + c.len_utf8();
            } else {
                break;
            }
        }
        if end > 0 {
            if let Ok(n) = after[..end].parse::<f64>() {
                return Some(n);
            }
        }
        from = key_end;
    }
    None
}

fn unescape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_bs = false;
    for c in s.chars() {
        if prev_bs {
            match c {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
            prev_bs = false;
            continue;
        }
        if c == '\\' {
            prev_bs = true;
            continue;
        }
        out.push(c);
    }
    out
}

fn recover_tool_call(text: &str) -> Option<ToolCall> {
    let name = extract_simple_string_field(text, "tool")?;
    let path = extract_simple_string_field(text, "path");
    let content = extract_content_field_lossy(text);
    if path.is_none() && content.is_none() {
        return None;
    }
    let mut args = serde_json::Map::new();
    if let Some(p) = path {
        args.insert("path".into(), serde_json::Value::String(p));
    }
    if let Some(c) = content {
        args.insert("content".into(), serde_json::Value::String(c));
    }
    Some(ToolCall { name, args: serde_json::Value::Object(args) })
}

/// 実 LLM の出力に頻出する「文字列値内に生の改行・タブ・制御文字が混じった JSON」を
/// 仕様準拠の JSON に直す lenient 変換。
/// 二重引用符の内側でのみエスケープ処理を行い、外側 (構造) はそのまま保つ。
pub(crate) fn lenient_jsonify(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut in_string = false;
    let mut prev_backslash = false;
    for c in text.chars() {
        if in_string {
            if prev_backslash {
                out.push(c);
                prev_backslash = false;
                continue;
            }
            match c {
                '"' => {
                    out.push('"');
                    in_string = false;
                }
                '\\' => {
                    out.push('\\');
                    prev_backslash = true;
                }
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                ch if (ch as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", ch as u32));
                }
                ch => out.push(ch),
            }
        } else {
            if c == '"' {
                in_string = true;
            }
            out.push(c);
        }
    }
    out
}

#[derive(Debug)]
pub struct ProposalMessage {
    pub proposal: Proposal,
    pub tool_outputs: Vec<Result<tmoe_tools::ToolOutput, ToolError>>,
}

/// 進行確認フック (Concierge / Verifier 役)。
///
/// 各ターン終了後に呼ばれ、機械的事実 (例: ファイルシステム状態, テスト合格判定など)
/// を返す。`Outcome::Done` ならループ脱出、`Outcome::Continue { hint }` なら hint を
/// 次ターンの Worker への user メッセージに注入する。これは tmoe の Concierge =
/// 「ユーザー Z 軸推進力 + 機械的事実伝達」の役割を、Worker 駆動ループに組み込んだもの。
///
/// 用例: リファクタリング完了確認 (grep ヒット 0)、テスト合格 (`cargo test` exit 0)、
/// 必須ファイル存在チェック、等。
#[async_trait::async_trait]
pub trait ProgressVerifier: Send + Sync {
    async fn verify(&self) -> VerifierOutcome;
}

#[derive(Debug, Clone)]
pub enum VerifierOutcome {
    /// 完了。ループ脱出。
    Done,
    /// 未完了。Worker への次ターン user メッセージに `hint` を注入する。
    Continue { hint: String },
}

/// run_worker_until_verified の戻り値。
#[derive(Debug)]
pub struct WorkerRunResult {
    pub turns: u32,
    pub completed: bool,
    pub last: Option<ProposalMessage>,
}

/// Worker を多ターン駆動し、毎ターン後に `verifier` で進行を確認する。
/// Verifier が `Done` を返したら脱出 (= 真の完了)。`Continue { hint }` なら hint を
/// Worker への次ターン user に注入して継続する。`max_turns` で打ち切り。
///
/// この helper は e2e と production で共通に使える「Worker × Concierge 進行確認」の
/// 汎用ループ。Worker への履歴注入戦略 (= summary view など) は呼び出し側が
/// `next_user_messages` クロージャで構築する責任を持つ。
pub async fn run_worker_until_verified<F, V>(
    system: &str,
    llm: &dyn tmoe_llm::LlmClient,
    tools: &tmoe_tools::ToolRegistry,
    verifier: &V,
    max_turns: u32,
    mut next_user_messages: F,
) -> anyhow::Result<WorkerRunResult>
where
    V: ProgressVerifier + ?Sized,
    F: FnMut(u32, Option<&ProposalMessage>, &str) -> Vec<tmoe_llm::ChatMessage>,
{
    let mut last: Option<ProposalMessage> = None;
    let mut last_hint = String::new();
    for t in 0..max_turns {
        let user_messages = next_user_messages(t, last.as_ref(), &last_hint);
        let pm = single_agent_loop(AgentRole::Worker, system, user_messages, llm, tools).await?;
        last = Some(pm);
        match verifier.verify().await {
            VerifierOutcome::Done => {
                return Ok(WorkerRunResult { turns: t + 1, completed: true, last });
            }
            VerifierOutcome::Continue { hint } => {
                last_hint = hint;
            }
        }
    }
    Ok(WorkerRunResult { turns: max_turns, completed: false, last })
}

/// 単体エージェントを 1 ステップだけ回す: LLM へ問い、Proposal を抽出し、Worker 役割なら
/// ツールを実行する。Phase 4 ではこの 1 ステップが Trio の `worker.act` に相当する。
pub async fn single_agent_loop(
    role: AgentRole,
    system: &str,
    user_messages: Vec<ChatMessage>,
    llm: &dyn LlmClient,
    tools: &ToolRegistry,
) -> anyhow::Result<ProposalMessage> {
    let mut messages = Vec::with_capacity(user_messages.len() + 1);
    messages.push(ChatMessage::system(system));
    messages.extend(user_messages);
    let resp = llm.chat(ChatRequest { messages, ..Default::default() }).await?;
    let proposal = parse_proposal(&resp.content);
    let profile = role.permission_profile();
    let mut tool_outputs = Vec::with_capacity(proposal.tool_calls.len());
    if matches!(role, AgentRole::Worker) {
        for call in &proposal.tool_calls {
            let r = tools.invoke(call, &profile).await;
            tool_outputs.push(r);
        }
    }
    let _ = Arc::new(()); // 静的解析: Arc を依存に残しておく (将来の同時呼び出し対応)
    Ok(ProposalMessage { proposal, tool_outputs })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tmoe_llm::{MockLlmClient, ScriptedTurn};
    use tmoe_tools::{EditFileTool, ReadFileTool};

    fn make_registry(root: std::path::PathBuf) -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EditFileTool { root: root.clone() }));
        reg.register(Arc::new(ReadFileTool { root }));
        reg
    }

    #[test]
    fn parse_extracts_tool_call_in_fence() {
        let txt = "ok\n```json\n{\"tool\":\"edit_file\",\"args\":{\"path\":\"a.rs\",\"content\":\"fn main(){}\"}}\n```\n進めます\n";
        let p = parse_proposal(txt);
        assert_eq!(p.tool_calls.len(), 1);
        assert_eq!(p.tool_calls[0].name, "edit_file");
        assert!(!p.done);
    }

    #[test]
    fn parse_detects_done_marker() {
        let p = parse_proposal("作業完了\nDONE\n");
        assert!(p.done);
        assert!(p.tool_calls.is_empty());
    }

    #[test]
    fn parse_inline_tool_json_line() {
        let p = parse_proposal("{\"tool\":\"read_file\",\"args\":{\"path\":\"x.rs\"}}\n");
        assert_eq!(p.tool_calls.len(), 1);
        assert_eq!(p.tool_calls[0].name, "read_file");
    }

    #[test]
    fn parse_recovers_unescaped_inner_quotes() {
        // 実 LLM 出力の典型: content 値内に Rust の "FizzBuzz" のような生引用符が残る。
        let raw = r#"```json
{
  "tool": "edit_file",
  "args": {
    "path": "src/fizzbuzz.rs",
    "content": "pub fn fizzbuzz(n: u32) -> Vec<String> {\n    (1..=n).map(|i| match (i % 3, i % 5) {\n        (0, 0) => "FizzBuzz".to_string(),\n        (0, _) => "Fizz".to_string(),\n        (_, 0) => "Buzz".to_string(),\n        _ => i.to_string(),\n    }).collect()\n}"
  }
}
```
DONE"#;
        let p = parse_proposal(raw);
        assert_eq!(p.tool_calls.len(), 1, "raw text was:\n{raw}");
        assert_eq!(p.tool_calls[0].name, "edit_file");
        let args = &p.tool_calls[0].args;
        assert_eq!(args.get("path").and_then(|v| v.as_str()), Some("src/fizzbuzz.rs"));
        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
        assert!(content.contains("FizzBuzz"));
        assert!(content.contains("pub fn fizzbuzz"));
    }

    #[test]
    fn parse_lenient_string_with_raw_newlines() {
        // 実 LLM が出しがちな「文字列値に生改行を含む JSON」を扱えること。
        let raw = "```json\n{\n  \"tool\": \"edit_file\",\n  \"args\": {\n    \"path\": \"a.rs\",\n    \"content\": \"fn main() {\n    println!(\\\"hi\\\");\n}\"\n  }\n}\n```\nDONE";
        let p = parse_proposal(raw);
        assert_eq!(p.tool_calls.len(), 1, "raw text was:\n{raw}");
        assert_eq!(p.tool_calls[0].name, "edit_file");
        let content = p.tool_calls[0]
            .args
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(content.contains("println!"));
        // 生改行が改行のまま復元される。
        assert!(content.contains('\n'));
    }

    #[tokio::test]
    async fn worker_executes_extracted_tool() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let reg = make_registry(root.clone());
        let llm = MockLlmClient::new("worker");
        llm.push(ScriptedTurn::new(
            "提案します\n```json\n{\"tool\":\"edit_file\",\"args\":{\"path\":\"hello.rs\",\"content\":\"fn main(){println!(\\\"hi\\\");}\"}}\n```\nDONE\n",
        ));
        let out = single_agent_loop(
            AgentRole::Worker,
            "system",
            vec![ChatMessage::user("hello.rs を作って")],
            &llm,
            &reg,
        )
        .await
        .unwrap();
        assert!(out.proposal.done);
        assert_eq!(out.tool_outputs.len(), 1);
        assert!(out.tool_outputs[0].is_ok());
        let written = std::fs::read_to_string(root.join("hello.rs")).unwrap();
        assert!(written.contains("println!"));
    }

    #[tokio::test]
    async fn supervisor_does_not_execute_tools_even_if_extracted() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let reg = make_registry(root.clone());
        let llm = MockLlmClient::new("supervisor");
        llm.push(ScriptedTurn::new(
            "{\"tool\":\"edit_file\",\"args\":{\"path\":\"x.rs\",\"content\":\"!!\"}}\n",
        ));
        let out = single_agent_loop(
            AgentRole::Supervisor,
            "system",
            vec![ChatMessage::user("review")],
            &llm,
            &reg,
        )
        .await
        .unwrap();
        // 抽出はされるが Supervisor は呼ばない。
        assert_eq!(out.proposal.tool_calls.len(), 1);
        assert_eq!(out.tool_outputs.len(), 0);
        assert!(!root.join("x.rs").exists());
    }
}
