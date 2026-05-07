//! 多ターン Mock LLM e2e: Worker による「探索 → 部分編集 → 検証」サイクル。
//!
//! 現実のコーディングエージェントが行う典型的なリファクタリング:
//!   1. `grep_text` でリネーム対象の出現箇所を全列挙
//!   2. `patch_file` で各ファイルを書き換え
//!   3. もう一度 `grep_text` で残骸が無いことを確認、新名が出現していることを確認
//!
//! ツール出力を次ターンの user メッセージに連結することで、Worker が前ターンの
//! 結果を見て次の手を選ぶ実運用フローを Mock LLM で決定論的に再現する。

use std::sync::Arc;
use tempfile::tempdir;
use tmoe_core::{single_agent_loop, AgentRole};
use tmoe_llm::{ChatMessage, MockLlmClient, ScriptedTurn};
use tmoe_tools::{
    GrepTextTool, ListFilesTool, PatchFileTool, ReadFileTool, ToolRegistry,
};

fn registry(root: std::path::PathBuf) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ReadFileTool { root: root.clone() }));
    reg.register(Arc::new(PatchFileTool { root: root.clone() }));
    reg.register(Arc::new(ListFilesTool { root: root.clone() }));
    reg.register(Arc::new(GrepTextTool { root }));
    reg
}

fn write_fixture() -> tempfile::TempDir {
    let d = tempdir().unwrap();
    let p = d.path();
    std::fs::create_dir_all(p.join("src")).unwrap();
    std::fs::create_dir_all(p.join("tests")).unwrap();
    std::fs::create_dir_all(p.join("target/.junk")).unwrap();
    std::fs::write(
        p.join("src/lib.rs"),
        "pub mod util;\n\npub fn old_name() -> i32 {\n    util::old_name() + 1\n}\n",
    )
    .unwrap();
    std::fs::write(
        p.join("src/util.rs"),
        "pub fn old_name() -> i32 { 41 }\n",
    )
    .unwrap();
    std::fs::write(
        p.join("tests/it.rs"),
        "// integration test for old_name\nuse demo_pkg::old_name;\n#[test]\nfn t() { assert_eq!(old_name(), 42); }\n",
    )
    .unwrap();
    // ノイズ: target 配下にも出現させ、skip されることを暗黙に検証する。
    std::fs::write(p.join("target/.junk/garbage.rs"), "fn old_name() {}\n").unwrap();
    d
}

fn format_tool_outputs(out: &tmoe_core::ProposalMessage) -> String {
    let mut s = String::new();
    for (i, r) in out.tool_outputs.iter().enumerate() {
        let name = out
            .proposal
            .tool_calls
            .get(i)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "?".into());
        match r {
            Ok(o) => {
                s.push_str(&format!("[{name} #{i} ok]\n{}\n", o.stdout));
            }
            Err(e) => {
                s.push_str(&format!("[{name} #{i} err] {e}\n"));
            }
        }
    }
    s
}

#[tokio::test]
async fn worker_renames_identifier_via_grep_then_patch_then_verify() {
    let d = write_fixture();
    let root = d.path().to_path_buf();
    let reg = registry(root.clone());
    let llm = MockLlmClient::new("worker");

    // ── Turn 1: grep で出現箇所を全列挙 ────────────────────────────────
    llm.push(ScriptedTurn::new(
        "```json\n\
         {\"tool\":\"grep_text\",\"args\":{\"pattern\":\"old_name\"}}\n\
         ```\n",
    ));
    let user_task = ChatMessage::user(
        "Rename the identifier `old_name` to `new_name` everywhere it appears under src/ and \
         tests/. Use grep_text first to find all occurrences, then issue one patch_file per \
         file with replace_all=true. Finally re-run grep_text to confirm zero residue. End with DONE.",
    );

    let mut history: Vec<ChatMessage> = vec![user_task.clone()];
    let turn1 = single_agent_loop(
        AgentRole::Worker,
        "tmoe Worker — explore-then-patch refactor.",
        history.clone(),
        &llm,
        &reg,
    )
    .await
    .unwrap();
    assert_eq!(turn1.proposal.tool_calls.len(), 1);
    assert_eq!(turn1.proposal.tool_calls[0].name, "grep_text");
    let grep_out = &turn1.tool_outputs[0].as_ref().unwrap().stdout;
    assert!(grep_out.contains("src/lib.rs"));
    assert!(grep_out.contains("src/util.rs"));
    assert!(grep_out.contains("tests/it.rs"));
    assert!(
        !grep_out.contains("target/"),
        "default skip_dirs must hide target/: {grep_out}"
    );

    history.push(ChatMessage::assistant(&turn1.proposal.raw_text));
    history.push(ChatMessage::user(format_tool_outputs(&turn1)));

    // ── Turn 2: 3 ファイルを 1 ターンで patch_file × 3 ────────────────
    llm.push(ScriptedTurn::new(
        "```json\n\
         {\"tool\":\"patch_file\",\"args\":{\"path\":\"src/lib.rs\",\"search\":\"old_name\",\"replace\":\"new_name\",\"replace_all\":true}}\n\
         ```\n\
         ```json\n\
         {\"tool\":\"patch_file\",\"args\":{\"path\":\"src/util.rs\",\"search\":\"old_name\",\"replace\":\"new_name\",\"replace_all\":true}}\n\
         ```\n\
         ```json\n\
         {\"tool\":\"patch_file\",\"args\":{\"path\":\"tests/it.rs\",\"search\":\"old_name\",\"replace\":\"new_name\",\"replace_all\":true}}\n\
         ```\n",
    ));
    let turn2 = single_agent_loop(
        AgentRole::Worker,
        "tmoe Worker — explore-then-patch refactor.",
        history.clone(),
        &llm,
        &reg,
    )
    .await
    .unwrap();
    assert_eq!(turn2.proposal.tool_calls.len(), 3);
    for r in &turn2.tool_outputs {
        let ok = r.as_ref().unwrap();
        assert!(ok.stdout.contains("replacement"), "stdout was: {}", ok.stdout);
    }

    // ファイル状態の物理確認 (target 配下は触れていない)。
    for rel in ["src/lib.rs", "src/util.rs", "tests/it.rs"] {
        let body = std::fs::read_to_string(root.join(rel)).unwrap();
        assert!(body.contains("new_name"), "{rel} missing new_name:\n{body}");
        assert!(!body.contains("old_name"), "{rel} still has old_name:\n{body}");
    }
    let untouched = std::fs::read_to_string(root.join("target/.junk/garbage.rs")).unwrap();
    assert!(
        untouched.contains("old_name"),
        "target/ file must remain untouched"
    );

    history.push(ChatMessage::assistant(&turn2.proposal.raw_text));
    history.push(ChatMessage::user(format_tool_outputs(&turn2)));

    // ── Turn 3: grep_text で残骸ゼロを確認、new_name 出現を確認 ────────
    llm.push(ScriptedTurn::new(
        "```json\n\
         {\"tool\":\"grep_text\",\"args\":{\"pattern\":\"old_name\"}}\n\
         ```\n\
         ```json\n\
         {\"tool\":\"grep_text\",\"args\":{\"pattern\":\"new_name\"}}\n\
         ```\n\
         DONE\n",
    ));
    let turn3 = single_agent_loop(
        AgentRole::Worker,
        "tmoe Worker — explore-then-patch refactor.",
        history.clone(),
        &llm,
        &reg,
    )
    .await
    .unwrap();
    assert!(turn3.proposal.done);
    assert_eq!(turn3.proposal.tool_calls.len(), 2);

    let leftover = &turn3.tool_outputs[0].as_ref().unwrap().stdout;
    let new_hits = &turn3.tool_outputs[1].as_ref().unwrap().stdout;
    assert!(
        leftover.trim().is_empty(),
        "expected zero remaining old_name; got:\n{leftover}"
    );
    let new_count = new_hits.lines().count();
    assert!(
        new_count >= 3,
        "expected at least 3 new_name hits across files, got {new_count}:\n{new_hits}"
    );
}

#[tokio::test]
async fn list_files_then_per_file_patch_drives_targeted_edits() {
    // list_files でファイル列挙 → 各ファイルに **唯一マッチ** で patch を当てる。
    // patch_file の uniqueness 制約 (replace_all=false 既定) を実用的に検証する。
    let d = tempdir().unwrap();
    let root = d.path().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/a.rs"),
        "pub const VERSION: &str = \"0.1.0\";\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/b.rs"),
        "pub const VERSION: &str = \"0.1.0\";\n",
    )
    .unwrap();

    let reg = registry(root.clone());
    let llm = MockLlmClient::new("worker");

    // Turn 1: list_files で .rs を列挙
    llm.push(ScriptedTurn::new(
        "```json\n\
         {\"tool\":\"list_files\",\"args\":{\"pattern\":\"src/**/*.rs\"}}\n\
         ```\n",
    ));
    let mut history = vec![ChatMessage::user("Bump VERSION to 0.2.0 in every src/*.rs.")];
    let t1 = single_agent_loop(
        AgentRole::Worker,
        "tmoe Worker.",
        history.clone(),
        &llm,
        &reg,
    )
    .await
    .unwrap();
    let listed = &t1.tool_outputs[0].as_ref().unwrap().stdout;
    assert!(listed.contains("src/a.rs"));
    assert!(listed.contains("src/b.rs"));

    history.push(ChatMessage::assistant(&t1.proposal.raw_text));
    history.push(ChatMessage::user(format_tool_outputs(&t1)));

    // Turn 2: 各ファイルに patch_file を発行 (デフォルトの uniqueness 制約を満たす唯一マッチ)
    llm.push(ScriptedTurn::new(
        "```json\n\
         {\"tool\":\"patch_file\",\"args\":{\"path\":\"src/a.rs\",\"search\":\"\\\"0.1.0\\\"\",\"replace\":\"\\\"0.2.0\\\"\"}}\n\
         ```\n\
         ```json\n\
         {\"tool\":\"patch_file\",\"args\":{\"path\":\"src/b.rs\",\"search\":\"\\\"0.1.0\\\"\",\"replace\":\"\\\"0.2.0\\\"\"}}\n\
         ```\n\
         DONE\n",
    ));
    let t2 = single_agent_loop(
        AgentRole::Worker,
        "tmoe Worker.",
        history.clone(),
        &llm,
        &reg,
    )
    .await
    .unwrap();
    assert!(t2.proposal.done);
    assert_eq!(t2.tool_outputs.len(), 2);
    for r in &t2.tool_outputs {
        assert!(r.as_ref().unwrap().stdout.contains("1 replacement"));
    }
    for rel in ["src/a.rs", "src/b.rs"] {
        let body = std::fs::read_to_string(root.join(rel)).unwrap();
        assert!(body.contains("\"0.2.0\""));
        assert!(!body.contains("\"0.1.0\""));
    }
}

#[tokio::test]
async fn ambiguous_patch_falls_back_to_replace_all_after_failure() {
    // 唯一マッチ前提で patch を出すと曖昧 reject、その err を見て Worker が
    // replace_all=true で再試行する流れ。Worker が tool_outputs のエラーを
    // 次ターンに反映できることを示す。
    let d = tempdir().unwrap();
    let root = d.path().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "todo\ntodo\nthird\n").unwrap();

    let reg = registry(root.clone());
    let llm = MockLlmClient::new("worker");

    // Turn 1: 普通に patch (曖昧マッチで失敗するはず)
    llm.push(ScriptedTurn::new(
        "```json\n\
         {\"tool\":\"patch_file\",\"args\":{\"path\":\"src/a.rs\",\"search\":\"todo\",\"replace\":\"done\"}}\n\
         ```\n",
    ));
    let mut history = vec![ChatMessage::user("Replace every todo with done in src/a.rs.")];
    let t1 = single_agent_loop(
        AgentRole::Worker,
        "tmoe Worker.",
        history.clone(),
        &llm,
        &reg,
    )
    .await
    .unwrap();
    assert!(t1.tool_outputs[0].is_err());
    let err_msg = match &t1.tool_outputs[0] {
        Err(e) => format!("{e}"),
        _ => unreachable!(),
    };
    assert!(err_msg.contains("matched 2 times"));
    let untouched = std::fs::read_to_string(root.join("src/a.rs")).unwrap();
    assert_eq!(untouched, "todo\ntodo\nthird\n");

    history.push(ChatMessage::assistant(&t1.proposal.raw_text));
    history.push(ChatMessage::user(format_tool_outputs(&t1)));

    // Turn 2: 失敗 note を読んで replace_all=true で再試行
    llm.push(ScriptedTurn::new(
        "```json\n\
         {\"tool\":\"patch_file\",\"args\":{\"path\":\"src/a.rs\",\"search\":\"todo\",\"replace\":\"done\",\"replace_all\":true}}\n\
         ```\n\
         DONE\n",
    ));
    let t2 = single_agent_loop(
        AgentRole::Worker,
        "tmoe Worker.",
        history,
        &llm,
        &reg,
    )
    .await
    .unwrap();
    assert!(t2.proposal.done);
    assert!(t2.tool_outputs[0].as_ref().unwrap().stdout.contains("2 replacements"));
    let body = std::fs::read_to_string(root.join("src/a.rs")).unwrap();
    assert_eq!(body, "done\ndone\nthird\n");
}
