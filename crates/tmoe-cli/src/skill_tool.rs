//! `skill` ツール: ユーザがプロジェクトや global に置いた SKILL.md を Worker に
//! オンデマンドで投入できる仕組み。opencode の skill ツールと **同じ frontmatter 形式**。
//!
//! 探索場所 (新しい順):
//!   1. `<workdir>/.tmoe/skills/<name>/SKILL.md`
//!   2. `<workdir>/.claude/skills/<name>/SKILL.md`  (opencode 互換: Claude Code の規約)
//!   3. `<workdir>/.agents/skills/<name>/SKILL.md`  (opencode 互換: Agents エコシステム)
//!   4. `~/.tmoe/skills/<name>/SKILL.md`            (global)
//!
//! SKILL.md は YAML frontmatter で `name` と `description` を保持する:
//!
//!   ---
//!   name: rust-refactor
//!   description: How to refactor rust code while keeping tests green.
//!   ---
//!
//!   # ボディ markdown ...
//!
//! Worker は `{"tool":"skill","args":{"name":"rust-refactor"}}` で内容を取り込む。
//! 同階層のファイル一覧も返すので、相対パスで他リソースを参照できる。

use async_trait::async_trait;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tmoe_tools::{Permission, Tool, ToolError, ToolOutput, ToolResult};

#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub location: PathBuf,
    pub content: String,
    /// SKILL.md と同じディレクトリ内のファイル相対パス (skill 自身は除く)。
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: BTreeMap<String, SkillInfo>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 全候補ディレクトリをスキャンして登録する。重複名は **後勝ち**
    /// (workdir > .claude > .agents > global の順なので、global の同名は workdir に上書きされる)。
    pub fn scan(workdir: &Path, home: Option<&Path>) -> Self {
        let mut reg = Self::default();
        // global を先に詰めて、workdir 系で上書きされるようにする。
        if let Some(home) = home {
            reg.scan_dir(&home.join(".tmoe").join("skills"));
        }
        reg.scan_dir(&workdir.join(".agents").join("skills"));
        reg.scan_dir(&workdir.join(".claude").join("skills"));
        reg.scan_dir(&workdir.join(".tmoe").join("skills"));
        reg
    }

    fn scan_dir(&mut self, root: &Path) {
        let Ok(entries) = std::fs::read_dir(root) else { return };
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            let skill_md = p.join("SKILL.md");
            let Ok(body) = std::fs::read_to_string(&skill_md) else { continue };
            if let Some(info) = parse_skill_md(&skill_md, &body) {
                self.skills.insert(info.name.clone(), info);
            }
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.skills.keys().cloned().collect()
    }

    pub fn get(&self, name: &str) -> Option<&SkillInfo> {
        self.skills.get(name)
    }

    pub fn list(&self) -> impl Iterator<Item = &SkillInfo> {
        self.skills.values()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

/// 1 行 1 行で簡易にパース。`---` で挟まれた frontmatter から `name:` / `description:` を抽出。
/// frontmatter 無しの場合は最初の H1 を name にし、最初の paragraph を description にする。
fn parse_skill_md(path: &Path, body: &str) -> Option<SkillInfo> {
    let (name, description, content_body) = if body.starts_with("---") {
        let rest = &body[3..];
        let close = rest.find("\n---")?;
        let frontmatter = &rest[..close];
        let after = rest[close + 4..].trim_start_matches('\n');
        let mut name: Option<String> = None;
        let mut description: Option<String> = None;
        for line in frontmatter.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("name:") {
                name = Some(rest.trim().trim_matches('"').trim_matches('\'').to_string());
            } else if let Some(rest) = line.strip_prefix("description:") {
                description = Some(rest.trim().trim_matches('"').trim_matches('\'').to_string());
            }
        }
        (name?, description.unwrap_or_default(), after.to_string())
    } else {
        // フォールバック: 1 個目の H1 を name に。最初の段落を description に。
        let mut name = None;
        let mut description = None;
        for line in body.lines() {
            let trimmed = line.trim();
            if name.is_none() && trimmed.starts_with("# ") {
                name = Some(trimmed[2..].trim().to_string());
                continue;
            }
            if name.is_some() && description.is_none() && !trimmed.is_empty() && !trimmed.starts_with('#') {
                description = Some(trimmed.to_string());
                break;
            }
        }
        (name?, description.unwrap_or_default(), body.to_string())
    };

    // 同階層のファイル一覧 (SKILL.md は除外、上限 30)。
    let dir = path.parent().unwrap_or(Path::new(""));
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file()
                && p.file_name()
                    .and_then(|s| s.to_str())
                    .map(|n| n != "SKILL.md")
                    .unwrap_or(false)
            {
                if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                    files.push(name.to_string());
                }
            }
        }
    }
    files.sort();
    files.truncate(30);

    Some(SkillInfo {
        name,
        description,
        location: path.to_path_buf(),
        content: content_body,
        files,
    })
}

pub struct SkillTool {
    pub registry: Arc<SkillRegistry>,
}

#[derive(Deserialize)]
struct Args {
    name: String,
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }
    fn requires(&self) -> Permission {
        Permission::Read
    }
    async fn call(&self, args: &serde_json::Value) -> ToolResult {
        let a: Args = serde_json::from_value(args.clone())
            .map_err(|e| ToolError::Args(format!("skill args: {e}")))?;
        let info = self.registry.get(&a.name).ok_or_else(|| {
            let avail: Vec<String> = self.registry.names();
            ToolError::Args(format!(
                "skill '{}' not found. available: {}",
                a.name,
                if avail.is_empty() {
                    "(none)".into()
                } else {
                    avail.join(", ")
                }
            ))
        })?;
        let dir = info
            .location
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let body = format!(
            "<skill name=\"{name}\">\n# {name}\n\n{desc}\n\n{content}\n\nskill_dir: {dir}\nfiles:\n  - {files}\n</skill>",
            name = info.name,
            desc = info.description,
            content = info.content.trim(),
            dir = dir,
            files = if info.files.is_empty() {
                "(none)".into()
            } else {
                info.files.join("\n  - ")
            },
        );
        Ok(ToolOutput::text(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn scan_picks_up_skill_with_frontmatter_in_workdir_tmoe_dir() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();
        write(
            &workdir.join(".tmoe/skills/rust-refactor/SKILL.md"),
            "---\nname: rust-refactor\ndescription: How to refactor rust safely.\n---\n\n# Rust refactor\n\nbody\n",
        );
        let reg = SkillRegistry::scan(workdir, None);
        assert_eq!(reg.names(), vec!["rust-refactor"]);
        let info = reg.get("rust-refactor").unwrap();
        assert_eq!(info.description, "How to refactor rust safely.");
        assert!(info.content.contains("body"));
    }

    #[test]
    fn workdir_skill_overrides_global_with_same_name() {
        let dir = tempdir().unwrap();
        let workdir = dir.path().join("work");
        let home = dir.path().join("home");
        write(
            &home.join(".tmoe/skills/x/SKILL.md"),
            "---\nname: x\ndescription: GLOBAL\n---\n\nglobal body\n",
        );
        write(
            &workdir.join(".tmoe/skills/x/SKILL.md"),
            "---\nname: x\ndescription: WORKDIR\n---\n\nworkdir body\n",
        );
        let reg = SkillRegistry::scan(&workdir, Some(&home));
        let info = reg.get("x").unwrap();
        assert_eq!(info.description, "WORKDIR");
        assert!(info.content.contains("workdir body"));
    }

    #[test]
    fn scan_picks_up_claude_skills_dir_for_compat() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();
        write(
            &workdir.join(".claude/skills/foo/SKILL.md"),
            "---\nname: foo\ndescription: claude-compat skill\n---\n\nbody\n",
        );
        let reg = SkillRegistry::scan(workdir, None);
        assert!(reg.get("foo").is_some());
    }

    #[tokio::test]
    async fn skill_tool_returns_content_and_file_list() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();
        write(
            &workdir.join(".tmoe/skills/k/SKILL.md"),
            "---\nname: k\ndescription: D\n---\n\nbody-text\n",
        );
        write(&workdir.join(".tmoe/skills/k/helper.sh"), "echo hi\n");
        let reg = Arc::new(SkillRegistry::scan(workdir, None));
        let tool = SkillTool { registry: reg };
        let out = tool.call(&serde_json::json!({"name":"k"})).await.unwrap();
        assert!(out.stdout.contains("# k"), "got: {}", out.stdout);
        assert!(out.stdout.contains("body-text"));
        assert!(out.stdout.contains("helper.sh"));
    }

    #[tokio::test]
    async fn skill_tool_lists_available_when_unknown() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();
        write(
            &workdir.join(".tmoe/skills/known/SKILL.md"),
            "---\nname: known\ndescription: d\n---\n\nbody\n",
        );
        let reg = Arc::new(SkillRegistry::scan(workdir, None));
        let tool = SkillTool { registry: reg };
        let err = tool.call(&serde_json::json!({"name":"missing"})).await.unwrap_err();
        match err {
            ToolError::Args(m) => {
                assert!(m.contains("not found"), "{m}");
                assert!(m.contains("known"), "should list known: {m}");
            }
            other => panic!("expected Args, got {other:?}"),
        }
    }

    #[test]
    fn fallback_parser_uses_first_h1_when_no_frontmatter() {
        let dir = tempdir().unwrap();
        let workdir = dir.path();
        write(
            &workdir.join(".tmoe/skills/no-fm/SKILL.md"),
            "# Some Skill\n\nA description sentence.\n\nMore body.\n",
        );
        let reg = SkillRegistry::scan(workdir, None);
        let info = reg.get("Some Skill").unwrap();
        assert_eq!(info.description, "A description sentence.");
    }
}
