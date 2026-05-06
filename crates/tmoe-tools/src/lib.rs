//! tmoe-tools: エージェントが呼び出すツール群。
//!
//! - read_file / edit_file / run_cmd / open_node / search_source など
//! - エージェント役割 (Worker / Supervisor / Observer) ごとに権限プロファイルが異なる
//!   - Worker: read + write + run_cmd
//!   - Supervisor: read + metrics (write 系は拒否権の側に立つので持たない)
//!   - Observer: read + metrics (write も run_cmd も持たない)
//! - 危険コマンド (rm -rf / git reset --hard 等) は Supervisor の拒否権でブロック

pub mod explore;
pub mod git;
pub mod permission;
pub mod registry;
pub mod tool;
pub mod tools;
pub mod web;

pub use explore::{GrepTextTool, ListFilesTool};
pub use git::{
    carve_worktree, cleanup_worktree, commit as git_commit, stage_all, working_diff_text,
    GitError, GitResult, WorktreeHandle,
};
pub use permission::{Permission, PermissionProfile};
pub use registry::ToolRegistry;
pub use tool::{Tool, ToolCall, ToolError, ToolOutput, ToolResult};
pub use tools::{default_blocklist, EditFileTool, PatchFileTool, ReadFileTool, RunCmdTool};
pub use web::{WebFetchTool, WebSearchTool};
