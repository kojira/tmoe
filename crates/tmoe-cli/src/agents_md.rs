//! プロジェクト固有のエージェント向け指示 (AGENTS.md) を読み込む。
//!
//! 業界共通的な convention に倣い、`AGENTS.md` をワークツリーの起点から
//! ルートに向かって走査し、見つかった内容を **ルート → リーフの順で連結**して
//! Worker の初期プロンプトに混ぜる。同じ階層に複数のエージェント向けファイル
//! (例: `AGENTS.md` と `CLAUDE.md`) があった場合は AGENTS.md を優先しつつ、
//! tmoe 固有の `TMOE.md` があれば末尾に重ねる (= プロジェクト側で最終的な
//! 上書きを許す)。
//!
//! 探索の上限:
//! - 現在のワークツリーの **git ルート** (見つかれば) まで
//! - git ルートが見つからなければ最大 8 階層上まで
//! - HOME ディレクトリより上には絶対に出ない (誤って `/etc` を読みに行かないため)

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct AgentsContext {
    pub files: Vec<AgentsFile>,
}

#[derive(Debug, Clone)]
pub struct AgentsFile {
    pub path: PathBuf,
    pub body: String,
}

impl AgentsContext {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Worker の user メッセージに prepend する形式で整形する。空なら空文字列を返す。
    pub fn render_for_prompt(&self) -> String {
        if self.files.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "PROJECT-SPECIFIC INSTRUCTIONS (read as authoritative — they describe local \
             conventions, must-do/must-not-do rules, paths to known constraints. Apply them \
             to the task below):\n\n",
        );
        for f in &self.files {
            out.push_str(&format!("--- {} ---\n", f.path.display()));
            out.push_str(f.body.trim());
            out.push_str("\n\n");
        }
        out.push_str("--- end of project instructions ---\n\n");
        out
    }
}

/// `start` から filesystem 上方向へ走査し、各階層で見つかった instruction ファイルを
/// **ルート (浅い階層) → リーフ (深い階層) の順** で詰めて返す。Worker は読み下し順に
/// 重ね合わせる: 浅い方が「プロジェクト全体ルール」、深い方が「該当サブディレクトリ固有」。
pub fn collect(start: &Path) -> AgentsContext {
    let mut chain: Vec<PathBuf> = Vec::new();
    let mut cur = start.to_path_buf();
    let stop_at = stop_boundary(start);
    let max_depth = 8;
    let mut depth = 0;
    loop {
        chain.push(cur.clone());
        if Some(cur.as_path()) == stop_at.as_deref() {
            break;
        }
        if depth >= max_depth {
            break;
        }
        let Some(parent) = cur.parent() else { break };
        if parent == cur {
            break;
        }
        // HOME より上には出ない (フィルタは last-resort セーフティ)。
        if let Some(home) = dirs::home_dir() {
            if parent.starts_with(&home).not() && cur.starts_with(&home) {
                break;
            }
        }
        cur = parent.to_path_buf();
        depth += 1;
    }
    chain.reverse(); // ルート → リーフへ
    let mut files = Vec::new();
    for dir in chain {
        for name in CANDIDATE_FILES {
            let p = dir.join(name);
            if let Ok(body) = std::fs::read_to_string(&p) {
                let body = body.trim().to_string();
                if !body.is_empty() {
                    files.push(AgentsFile { path: p, body });
                }
            }
        }
    }
    AgentsContext { files }
}

/// 走査の停止境界: git ルート優先、無ければ HOME。
fn stop_boundary(start: &Path) -> Option<PathBuf> {
    if let Ok(repo) = git2::Repository::discover(start) {
        if let Some(workdir) = repo.workdir() {
            return Some(workdir.to_path_buf());
        }
    }
    dirs::home_dir()
}

const CANDIDATE_FILES: &[&str] = &["AGENTS.md", "TMOE.md"];

trait NotExt {
    fn not(self) -> bool;
}
impl NotExt for bool {
    fn not(self) -> bool {
        !self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(p: &Path, body: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn empty_when_no_files_present() {
        let dir = tempdir().unwrap();
        let ctx = collect(dir.path());
        assert!(ctx.is_empty());
        assert_eq!(ctx.render_for_prompt(), "");
    }

    #[test]
    fn picks_up_agents_md_in_workdir() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("AGENTS.md"), "Use Rust 2021. Prefer patch_file.");
        let ctx = collect(dir.path());
        assert_eq!(ctx.files.len(), 1);
        let rendered = ctx.render_for_prompt();
        assert!(rendered.contains("PROJECT-SPECIFIC INSTRUCTIONS"));
        assert!(rendered.contains("Use Rust 2021"));
    }

    #[test]
    fn root_then_leaf_order_within_a_git_repo() {
        // git repo の中でサブディレクトリから収集すると、ルートの AGENTS.md が先に来る。
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        git2::Repository::init(&repo_root).unwrap();
        let sub = repo_root.join("src/sub");
        fs::create_dir_all(&sub).unwrap();
        write(&repo_root.join("AGENTS.md"), "ROOT_RULE");
        write(&sub.join("AGENTS.md"), "LEAF_RULE");
        let ctx = collect(&sub);
        assert_eq!(ctx.files.len(), 2);
        let r = ctx.render_for_prompt();
        let i_root = r.find("ROOT_RULE").expect("ROOT_RULE missing");
        let i_leaf = r.find("LEAF_RULE").expect("LEAF_RULE missing");
        assert!(i_root < i_leaf, "root rule should appear before leaf rule");
    }

    #[test]
    fn tmoe_md_picked_up_after_agents_md_at_same_dir() {
        // 同じ階層で AGENTS.md と TMOE.md があれば AGENTS.md → TMOE.md の順 (TMOE.md が後勝ち)。
        let dir = tempdir().unwrap();
        write(&dir.path().join("AGENTS.md"), "GENERIC_RULE");
        write(&dir.path().join("TMOE.md"), "TMOE_RULE");
        let ctx = collect(dir.path());
        assert_eq!(ctx.files.len(), 2);
        let r = ctx.render_for_prompt();
        let i_a = r.find("GENERIC_RULE").unwrap();
        let i_t = r.find("TMOE_RULE").unwrap();
        assert!(i_a < i_t, "AGENTS.md should come before TMOE.md at same dir");
    }

    #[test]
    fn empty_files_are_skipped() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("AGENTS.md"), "   \n\n   ");
        let ctx = collect(dir.path());
        assert!(ctx.is_empty(), "whitespace-only files must not contribute");
    }
}
