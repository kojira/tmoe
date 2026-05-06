//! tmoe-tools: エージェントが呼ぶツール群。
//!
//! read_file / edit_file / run_cmd / git_* / search_source / open_node など。
//! Worker は全権、Supervisor は read 系、Observer は read + メトリクス読み取りに権限を絞る。
//! 危険コマンド (rm -rf / git reset --hard 等) は Supervisor の拒否権で停止する。
