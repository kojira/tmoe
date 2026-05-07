//! 履歴の永続化レイヤ。
//!
//! SQLite で feature / raw_node / agent_summary_node を管理し、本文 (raw 会話本文と
//! 各エージェントの要約本文) は JSONL / Markdown としてファイルシステムに置く。
//! content_hash は BLAKE3。

use crate::error::{HistoryError, Result};
use crate::types::{AgentSummaryNode, AgentView, Feature, FeatureStatus, RawKind, RawNode};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 既知のスキーママイグレーション。番号順に適用される。
/// 新しい列やテーブルを足したいときはこのリストの末尾に `(N+1, "...sql...")` を追加する。
/// 既存の番号は **絶対に書き換えない** (歴史的不変)。
const MIGRATIONS: &[(u32, &str)] = &[(
    1,
    r#"
    CREATE TABLE IF NOT EXISTS feature (
        id           TEXT PRIMARY KEY,
        title        TEXT NOT NULL,
        status       TEXT NOT NULL,
        root_node_id TEXT NOT NULL,
        created_at   INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS raw_node (
        id           TEXT PRIMARY KEY,
        feature_id   TEXT NOT NULL,
        parent_id    TEXT,
        kind         TEXT NOT NULL,
        content_hash TEXT NOT NULL,
        created_at   INTEGER NOT NULL,
        FOREIGN KEY(feature_id) REFERENCES feature(id)
    );
    CREATE TABLE IF NOT EXISTS agent_summary_node (
        id           TEXT PRIMARY KEY,
        feature_id   TEXT NOT NULL,
        agent        TEXT NOT NULL CHECK(agent IN ('worker','supervisor','observer')),
        parent_id    TEXT,
        summary      TEXT NOT NULL,
        ref_raw_ids  TEXT NOT NULL,
        ref_hashes   TEXT NOT NULL,
        level        INTEGER NOT NULL,
        created_at   INTEGER NOT NULL,
        updated_at   INTEGER NOT NULL,
        FOREIGN KEY(feature_id) REFERENCES feature(id)
    );
    CREATE INDEX IF NOT EXISTS idx_raw_feature_parent
        ON raw_node(feature_id, parent_id);
    CREATE INDEX IF NOT EXISTS idx_summary_feature_agent
        ON agent_summary_node(feature_id, agent, level);
    "#,
)];

pub struct HistoryStore {
    conn: Mutex<Connection>,
    base_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct AppendRaw {
    pub feature_id: String,
    pub parent_id: Option<String>,
    pub kind: RawKind,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct AppendSummary {
    pub feature_id: String,
    pub agent: AgentView,
    pub parent_id: Option<String>,
    pub summary: String,
    pub ref_raw_ids: Vec<String>,
    pub ref_hashes: Vec<String>,
    pub level: i32,
}

impl HistoryStore {
    /// `base_dir` の下に `db.sqlite` と `features/` を置いて開く。
    pub fn open(base_dir: impl AsRef<Path>) -> Result<Self> {
        let base = base_dir.as_ref().to_path_buf();
        fs::create_dir_all(&base)?;
        fs::create_dir_all(base.join("features"))?;
        let db_path = base.join("db.sqlite");
        let conn = Connection::open(&db_path)?;
        Self::migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn), base_dir: base })
    }

    fn migrate(conn: &Connection) -> Result<()> {
        // schema_version 表自体は手動で必ず作る (= migration runner の足場)。
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER NOT NULL PRIMARY KEY
            );
            "#,
        )?;
        let cur: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let target = MIGRATIONS.last().map(|(v, _)| *v as i64).unwrap_or(0);
        if cur > target {
            return Err(HistoryError::Invalid(format!(
                "history DB is at schema version {cur} but this build only knows up to {target}; \
                 please upgrade tmoe or use a newer DB on a newer install"
            )));
        }
        for (v, sql) in MIGRATIONS.iter() {
            if (*v as i64) <= cur {
                continue;
            }
            conn.execute_batch(sql)?;
            conn.execute(
                "INSERT INTO schema_version(version) VALUES(?1)",
                params![*v as i64],
            )?;
        }
        Ok(())
    }

    /// テスト/ doctor 用: 現在の schema バージョンを返す。
    pub fn schema_version(&self) -> Result<u32> {
        let conn = self.conn.lock().unwrap();
        let v: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        Ok(v as u32)
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn new_id() -> String {
        ulid::Ulid::new().to_string()
    }

    pub fn create_feature(&self, title: impl Into<String>) -> Result<Feature> {
        let conn = self.conn.lock().unwrap();
        let now = Self::now();
        let id = Self::new_id();
        let root_id = Self::new_id();
        let title = title.into();

        conn.execute(
            "INSERT INTO feature (id, title, status, root_node_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, title, FeatureStatus::Planned.as_str(), root_id, now],
        )?;
        conn.execute(
            "INSERT INTO raw_node (id, feature_id, parent_id, kind, content_hash, created_at) VALUES (?1, ?2, NULL, ?3, ?4, ?5)",
            params![root_id, id, RawKind::Plan.as_str(), "", now],
        )?;
        drop(conn);

        // ファイル基盤も準備する。
        self.feature_dir(&id, true)?;
        Ok(Feature {
            id,
            title,
            status: FeatureStatus::Planned,
            root_node_id: root_id,
            created_at: now,
        })
    }

    pub fn set_feature_status(&self, feature_id: &str, status: FeatureStatus) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE feature SET status = ?1 WHERE id = ?2",
            params![status.as_str(), feature_id],
        )?;
        if n == 0 {
            return Err(HistoryError::NotFound(format!("feature {feature_id}")));
        }
        Ok(())
    }

    /// 全 feature 行を created_at の降順 (新しい順) で返す。doctor / `tmoe history` で使う。
    pub fn list_features(&self) -> Result<Vec<Feature>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, status, root_node_id, created_at FROM feature \
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (id, title, status_s, root_node_id, created_at) = r?;
            let status = FeatureStatus::parse(&status_s)
                .ok_or_else(|| HistoryError::Invalid(format!("status {status_s}")))?;
            out.push(Feature { id, title, status, root_node_id, created_at });
        }
        Ok(out)
    }

    pub fn get_feature(&self, feature_id: &str) -> Result<Feature> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT id, title, status, root_node_id, created_at FROM feature WHERE id = ?1",
                params![feature_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        let (id, title, status, root_node_id, created_at) =
            row.ok_or_else(|| HistoryError::NotFound(format!("feature {feature_id}")))?;
        let status = FeatureStatus::parse(&status)
            .ok_or_else(|| HistoryError::Invalid(format!("status {status}")))?;
        Ok(Feature { id, title, status, root_node_id, created_at })
    }

    pub fn append_raw(&self, append: AppendRaw) -> Result<RawNode> {
        let id = Self::new_id();
        let hash = blake3::hash(append.body.as_bytes()).to_hex().to_string();
        let now = Self::now();
        let node = RawNode {
            id: id.clone(),
            feature_id: append.feature_id.clone(),
            parent_id: append.parent_id.clone(),
            kind: append.kind,
            content_hash: hash.clone(),
            created_at: now,
        };
        // SQLite に行を追加。
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO raw_node (id, feature_id, parent_id, kind, content_hash, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, append.feature_id, append.parent_id, append.kind.as_str(), hash, now],
            )?;
        }
        // 本文を JSONL ファイルへ書き出す。
        let dir = self.raw_dir(&append.feature_id, true)?;
        let path = dir.join(format!("{id}.jsonl"));
        let line = serde_json::json!({
            "id": id,
            "feature_id": append.feature_id,
            "parent_id": append.parent_id,
            "kind": append.kind.as_str(),
            "content_hash": hash,
            "created_at": now,
            "body": append.body,
        });
        fs::write(path, format!("{}\n", line))?;
        Ok(node)
    }

    pub fn read_raw_body(&self, feature_id: &str, raw_id: &str) -> Result<String> {
        let dir = self.raw_dir(feature_id, false)?;
        let path = dir.join(format!("{raw_id}.jsonl"));
        let text = fs::read_to_string(&path)?;
        let line = text.lines().next().unwrap_or("");
        let v: serde_json::Value = serde_json::from_str(line)?;
        Ok(v.get("body").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }

    pub fn list_raw(&self, feature_id: &str) -> Result<Vec<RawNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, feature_id, parent_id, kind, content_hash, created_at \
             FROM raw_node WHERE feature_id = ?1 ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map(params![feature_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (id, feature_id, parent_id, kind, content_hash, created_at) = r?;
            let kind = RawKind::parse(&kind)
                .ok_or_else(|| HistoryError::Invalid(format!("kind {kind}")))?;
            out.push(RawNode { id, feature_id, parent_id, kind, content_hash, created_at });
        }
        Ok(out)
    }

    pub fn append_summary(&self, append: AppendSummary) -> Result<AgentSummaryNode> {
        let id = Self::new_id();
        let now = Self::now();
        let ref_raw_ids = serde_json::to_string(&append.ref_raw_ids)?;
        let ref_hashes = serde_json::to_string(&append.ref_hashes)?;
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO agent_summary_node \
                 (id, feature_id, agent, parent_id, summary, ref_raw_ids, ref_hashes, level, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                params![
                    id,
                    append.feature_id,
                    append.agent.as_str(),
                    append.parent_id,
                    append.summary,
                    ref_raw_ids,
                    ref_hashes,
                    append.level,
                    now,
                ],
            )?;
        }
        // 本文 (Markdown) をエージェント別ディレクトリに保存。
        let dir = self.agent_dir(&append.feature_id, append.agent, true)?;
        let path = dir.join(format!("{id}.md"));
        fs::write(path, &append.summary)?;
        Ok(AgentSummaryNode {
            id,
            feature_id: append.feature_id,
            agent: append.agent,
            parent_id: append.parent_id,
            summary: append.summary,
            ref_raw_ids: append.ref_raw_ids,
            ref_hashes: append.ref_hashes,
            level: append.level,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn update_summary(
        &self,
        node_id: &str,
        new_summary: &str,
        merged_ref_raw_ids: &[String],
        merged_ref_hashes: &[String],
    ) -> Result<()> {
        let now = Self::now();
        let agent_str: String;
        let feature_id: String;
        {
            let conn = self.conn.lock().unwrap();
            let row = conn
                .query_row(
                    "SELECT feature_id, agent FROM agent_summary_node WHERE id = ?1",
                    params![node_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            let (fid, ag) = row.ok_or_else(|| HistoryError::NotFound(format!("summary {node_id}")))?;
            feature_id = fid;
            agent_str = ag;
            conn.execute(
                "UPDATE agent_summary_node SET summary = ?1, ref_raw_ids = ?2, ref_hashes = ?3, updated_at = ?4 WHERE id = ?5",
                params![
                    new_summary,
                    serde_json::to_string(merged_ref_raw_ids)?,
                    serde_json::to_string(merged_ref_hashes)?,
                    now,
                    node_id,
                ],
            )?;
        }
        let agent = AgentView::parse(&agent_str)
            .ok_or_else(|| HistoryError::Invalid(format!("agent {agent_str}")))?;
        let dir = self.agent_dir(&feature_id, agent, true)?;
        fs::write(dir.join(format!("{node_id}.md")), new_summary)?;
        Ok(())
    }

    pub fn list_summary(
        &self,
        feature_id: &str,
        agent: AgentView,
    ) -> Result<Vec<AgentSummaryNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, feature_id, agent, parent_id, summary, ref_raw_ids, ref_hashes, level, created_at, updated_at \
             FROM agent_summary_node WHERE feature_id = ?1 AND agent = ?2 ORDER BY level, created_at, id",
        )?;
        let rows = stmt.query_map(params![feature_id, agent.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i32>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (id, feature_id, agent_s, parent_id, summary, ref_raw_ids_s, ref_hashes_s, level, created_at, updated_at) = r?;
            let agent = AgentView::parse(&agent_s)
                .ok_or_else(|| HistoryError::Invalid(format!("agent {agent_s}")))?;
            let ref_raw_ids: Vec<String> = serde_json::from_str(&ref_raw_ids_s)?;
            let ref_hashes: Vec<String> = serde_json::from_str(&ref_hashes_s)?;
            out.push(AgentSummaryNode {
                id,
                feature_id,
                agent,
                parent_id,
                summary,
                ref_raw_ids,
                ref_hashes,
                level,
                created_at,
                updated_at,
            });
        }
        Ok(out)
    }

    /// 指定 agent の最新 (latest_at で見て最新) の level=0 ノードを返す。
    pub fn latest_level0(&self, feature_id: &str, agent: AgentView) -> Result<Option<AgentSummaryNode>> {
        let nodes = self.list_summary(feature_id, agent)?;
        Ok(nodes.into_iter().filter(|n| n.level == 0).next_back())
    }

    fn feature_dir(&self, feature_id: &str, create: bool) -> Result<PathBuf> {
        let dir = self.base_dir.join("features").join(feature_id);
        if create {
            fs::create_dir_all(&dir)?;
        }
        Ok(dir)
    }

    fn raw_dir(&self, feature_id: &str, create: bool) -> Result<PathBuf> {
        let dir = self.feature_dir(feature_id, create)?.join("raw");
        if create {
            fs::create_dir_all(&dir)?;
        }
        Ok(dir)
    }

    fn agent_dir(&self, feature_id: &str, agent: AgentView, create: bool) -> Result<PathBuf> {
        let dir = self.feature_dir(feature_id, create)?.join(agent.as_str());
        if create {
            fs::create_dir_all(&dir)?;
        }
        Ok(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn schema_version_is_set_after_open() {
        let dir = tempdir().unwrap();
        let store = HistoryStore::open(dir.path()).unwrap();
        assert_eq!(store.schema_version().unwrap(), 1);
        // 同じ DB を再度 open しても再適用されないこと (idempotent)。
        drop(store);
        let store2 = HistoryStore::open(dir.path()).unwrap();
        assert_eq!(store2.schema_version().unwrap(), 1);
    }

    #[test]
    fn rejects_db_with_higher_schema_version_than_we_know() {
        // 「未来から来た DB」(schema_version > MIGRATIONS の最大番号) は明示的に拒否する。
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");
        std::fs::create_dir_all(dir.path().join("features")).unwrap();
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY); \
             INSERT INTO schema_version(version) VALUES(99);",
        )
        .unwrap();
        drop(conn);
        let res = HistoryStore::open(dir.path());
        assert!(res.is_err(), "should reject DB from the future");
        let msg = res.err().unwrap().to_string();
        assert!(
            msg.contains("schema version 99") || msg.contains("schema_version") || msg.contains("up to"),
            "unexpected error: {msg}"
        );
    }

    fn fresh_store() -> (tempfile::TempDir, HistoryStore) {
        let dir = tempdir().unwrap();
        let store = HistoryStore::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn create_feature_creates_root_raw_node() {
        let (_d, s) = fresh_store();
        let f = s.create_feature("add gcd").unwrap();
        assert_eq!(f.title, "add gcd");
        let raws = s.list_raw(&f.id).unwrap();
        assert_eq!(raws.len(), 1);
        assert_eq!(raws[0].id, f.root_node_id);
    }

    #[test]
    fn append_raw_persists_body_to_jsonl() {
        let (_d, s) = fresh_store();
        let f = s.create_feature("t").unwrap();
        let raw = s
            .append_raw(AppendRaw {
                feature_id: f.id.clone(),
                parent_id: Some(f.root_node_id.clone()),
                kind: RawKind::Turn,
                body: "user said hello".to_string(),
            })
            .unwrap();
        assert_ne!(raw.content_hash, "");
        let body = s.read_raw_body(&f.id, &raw.id).unwrap();
        assert_eq!(body, "user said hello");
    }

    #[test]
    fn three_agents_have_independent_summary_indexes() {
        let (_d, s) = fresh_store();
        let f = s.create_feature("t").unwrap();
        let raw = s
            .append_raw(AppendRaw {
                feature_id: f.id.clone(),
                parent_id: None,
                kind: RawKind::Turn,
                body: "user wants gcd".into(),
            })
            .unwrap();
        for (agent, summary) in [
            (AgentView::Worker, "implement gcd via euclid"),
            (AgentView::Supervisor, "watch for overflow"),
            (AgentView::Observer, "intent: math util add"),
        ] {
            s.append_summary(AppendSummary {
                feature_id: f.id.clone(),
                agent,
                parent_id: None,
                summary: summary.into(),
                ref_raw_ids: vec![raw.id.clone()],
                ref_hashes: vec![raw.content_hash.clone()],
                level: 0,
            })
            .unwrap();
        }
        let w = s.list_summary(&f.id, AgentView::Worker).unwrap();
        let p = s.list_summary(&f.id, AgentView::Supervisor).unwrap();
        let o = s.list_summary(&f.id, AgentView::Observer).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(p.len(), 1);
        assert_eq!(o.len(), 1);
        assert_ne!(w[0].summary, p[0].summary);
        assert_ne!(p[0].summary, o[0].summary);
    }

    #[test]
    fn update_summary_overwrites_md_and_row() {
        let (_d, s) = fresh_store();
        let f = s.create_feature("t").unwrap();
        let n = s
            .append_summary(AppendSummary {
                feature_id: f.id.clone(),
                agent: AgentView::Worker,
                parent_id: None,
                summary: "v1".into(),
                ref_raw_ids: vec![],
                ref_hashes: vec![],
                level: 0,
            })
            .unwrap();
        s.update_summary(&n.id, "v2", &["raw-1".into()], &["hash-1".into()])
            .unwrap();
        let after = s.list_summary(&f.id, AgentView::Worker).unwrap();
        assert_eq!(after[0].summary, "v2");
        assert_eq!(after[0].ref_raw_ids, vec!["raw-1".to_string()]);
    }

    #[test]
    fn set_status_done() {
        let (_d, s) = fresh_store();
        let f = s.create_feature("t").unwrap();
        s.set_feature_status(&f.id, FeatureStatus::Done).unwrap();
        let f2 = s.get_feature(&f.id).unwrap();
        assert_eq!(f2.status, FeatureStatus::Done);
    }
}
