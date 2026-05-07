# tmoe — 設計ドキュメント

本書は tmoe の権威ある詳細設計ドキュメントである。README は概要に留め、
変更が議論を要する「なぜそうなっているか」「どう動くか」はすべて本書側に書く。

---

## 0. 設計原則 — なぜ「3 + 1」か

tmoe は **3 つのエージェント** + **1 人のユーザー** という、合計 4 個の頂点で
構成される。だがこの「4」は対称な 4 ではなく、**3 + 1** という非対称な構造で
ある必要がある。これは次の数学的事実に基づく:

1. **平面決定性**:
   同一直線上にない **3 点は平面を一意に決定する**。
   2 点では平面が無数に存在し、4 点は同一平面に乗る保証がなく歪みを生む。
   tmoe の意思決定は 1 枚の平面に落としたい。よって頂点数は 3 が最小かつ最適。

2. **剛性 (rigidity)**:
   三角形は **最小の剛体多角形** である。3 辺が決まれば形が一意に確定する。
   4 辺以上の多角形は対角線を加えるまで潰れる自由度を持つ。
   合意の「形」が時間を超えて崩れないためには三角形である必要がある。

3. **3 次元空間の完成**:
   2D 平面はそれ自身では運動を生まない。**直交する Z 軸の推進ベクトル**が
   加わって初めて 3D 運動空間が成立する。tmoe では Z 軸はユーザーが担う。
   エージェント側を 4 体・5 体に増やしても平面の歪みが増えるだけで、
   Z 軸方向の力は得られない。**Z はユーザーからしか来ない**。

社会的アナロジー (三権分立・トロイカ・三位一体) は意図的に使わない。
これらは表面的に「3」を強調するが、tmoe の設計根拠は完全に幾何学的である。

```
                 Z 軸 (ユーザー Z 軸推進力)
                 ▲
                 │
                 │
   Worker ──────┼───── Supervisor    ← 合意平面 (XY)
       \        │       /
        \       │      /
         \      │     /
          \    Observer
           \   /
            \ /
             ●  feature の現在地

  3 頂点が平面 (2D) を一意に決め、
  ユーザーの Z 軸推進力が加わって初めて前進する。
```

頂点数を「3」に固定することは tmoe の設計原則であり、増減させない。

---

## 1. 三角形の頂点 — 3 つの直交ベクトル

3 エージェントは互いに **線形独立な方向性ベクトル** を背負う。
プロンプトが似てしまうと 3 点が同一直線上に並んで平面が縮退するため、
明確に異なる立場・目的関数で初期化する。

| 頂点 | 方向ベクトル | 目的関数 (内部スコア) | プロンプト基調 |
|------|-------------|---------------------|---------------|
| **Worker** (推進軸) | 「進めよ・形にせよ」 | 課題解決度・完了度。停止コストを高く扱う | 実装志向・速度志向・楽観的 |
| **Supervisor** (批判軸) | 「立ち止まれ・整えよ」 | 整合性・安全性・要件適合度。誤りコストを高く扱う | 慎重・批判的・規範志向 |
| **Observer** (俯瞰軸) | 「外から見よ・全体を測れ」 | ユーザー意図との照合・記憶連続性・ループ検出。逸脱コストを高く扱う | 外在視点・メタ認知・履歴参照 |

3 者の出力が時間とともに似通ってくる現象 (収斂) は平面の縮退を意味する。
Observer は定期的に「3 者の出力ベクトル類似度」をモニタし、閾値超過時に
プロンプト多様性の再注入を要求する。

---

## 2. 第 4 の軸はユーザー — Z 軸推進力

ユーザーは **エージェントの一員ではない**。ユーザーは合意平面に直交する
Z 軸推進力ベクトルとして tmoe アーキテクチャに組み込まれる。

- エージェント側は意思決定の **形** を一意化する (平面合意)
- ユーザー側はそれを動かす **力** を与える (Z 軸推進)
- どちらが欠けても tmoe は次の点へ進めない

`Concierge` は 4 人目のエージェントではなく、**ユーザー Z 軸推進力を平面に
伝達する I/O チャネル**として位置付ける。LLM 推論を含むがエージェント本体
ではなく、TUI の入出力を整形して `z_thrust` シグナル (進めて / 止めて /
方向を変えて) を Trio の MessageBus に流す。Concierge 自身は合意平面の
頂点にカウントしない。

---

## 3. 合意プロトコル — 平面合意 × Z 軸推進

各エージェントは Worker の提案に対し独立に
`(approve: bool, confidence: 0.0–1.0, vector_note)` を返す。
前進量は **平面合意 × Z 軸推進** の積で決まる。どちらかがゼロなら前進量はゼロ。

```rust
// pseudo
loop {
    let proposal = worker.act(&state);
    let (s_vote, s_note) = supervisor.judge(&proposal, &state);
    let (o_vote, o_note) = observer.witness(&proposal, &state);
    let (w_vote, w_note) = worker.self_assess(&proposal);

    let plane_ok        = s_vote.approve && o_vote.approve && w_vote.approve;
    let confidence_sum  = s_vote.confidence + o_vote.confidence + w_vote.confidence;
    let triangle_balance = min(s,o,w) / max(s,o,w);   // 1.0 = 正三角形
    let z_thrust        = user_state.thrust();        // ユーザー由来 Z 軸推進

    if plane_ok && confidence_sum >= 2.4 && triangle_balance >= 0.6 && z_thrust > 0.0 {
        break commit(proposal);   // 平面が均整 × Z が前向き → 3D 上で前進
    } else if !plane_ok || triangle_balance < 0.6 {
        state.absorb(s_note, o_note, w_note);   // 平面の歪み修復
        continue;
    } else if z_thrust <= 0.0 {
        park_and_await_user();    // 平面はできているが Z 推進が無い → 静止
    }
    if iter >= MAX { escalate_to_user(); break; }
}
```

設計上の不変条件:

- **多数決禁止**: 2 人が approve でも 1 人が強く反対 (高 confidence + 低 approve) なら平面が歪むため前進しない
- **三角形バランス**: 1 人だけが過剰主張する状態は不健全 → リバランス
- **Z 軸はユーザー専有**: エージェント側は z_thrust を自家発電できない
- **静止と前進の分離**: park 中も Concierge は常時受付可能。GO で commit、軌道修正で proposal 破棄、停止で feature 中断

平面内では Supervisor が拒否権 (最終却下権) を持ち、
3 次元上の最終 GO はユーザー (Z 軸) が握る。

---

## 4. 三視点の記憶 — raw + 3 並走 index

履歴は **共通 raw ツリー 1 本** + **エージェント別要約 index 3 本** の
四層構造で持つ。中立要約は意図的に作らない (作ると三角形が縮退する)。

### 4.1 SQLite スキーマ

```sql
CREATE TABLE feature (
  id           TEXT PRIMARY KEY,    -- ULID
  title        TEXT NOT NULL,
  status       TEXT NOT NULL,       -- planned / in_progress / done / abandoned
  root_node_id TEXT NOT NULL,
  created_at   INTEGER NOT NULL
);

-- 共通 raw 履歴ツリー
CREATE TABLE raw_node (
  id           TEXT PRIMARY KEY,
  feature_id   TEXT NOT NULL,
  parent_id    TEXT,
  kind         TEXT NOT NULL,       -- plan / turn / decision / code_change / tool_call ...
  content_hash TEXT NOT NULL,       -- BLAKE3 of full content
  created_at   INTEGER NOT NULL,
  FOREIGN KEY(feature_id) REFERENCES feature(id)
);

-- エージェント別要約 index (Worker / Supervisor / Observer の 3 本並走)
CREATE TABLE agent_summary_node (
  id           TEXT PRIMARY KEY,
  feature_id   TEXT NOT NULL,
  agent        TEXT NOT NULL CHECK(agent IN ('worker','supervisor','observer')),
  parent_id    TEXT,                -- 同 agent 内のツリー親
  summary      TEXT NOT NULL,
  ref_raw_ids  TEXT NOT NULL,       -- 参照元 raw_node.id 群 (JSON array)
  ref_hashes   TEXT NOT NULL,       -- 同上の content_hash 群
  level        INTEGER NOT NULL,    -- 0 = 末端 (1ターン要約), 1+ = 高階要約
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL,
  FOREIGN KEY(feature_id) REFERENCES feature(id)
);

CREATE INDEX idx_raw_feature_parent     ON raw_node(feature_id, parent_id);
CREATE INDEX idx_summary_feature_agent  ON agent_summary_node(feature_id, agent, level);
```

- raw のフルテキスト: `~/.tmoe/features/<feature_id>/raw/<node_id>.jsonl`
- 各 agent の要約フルテキスト: `~/.tmoe/features/<feature_id>/<agent>/<node_id>.md`
- `content_hash` は BLAKE3

### 4.2 逐次コンパクション (incremental rolling summary)

「閾値到達で一括圧縮」は禁則。代わりに **ターンが進むたびに各エージェントが
自分の view を 1 ノードぶん延伸し、必要時にだけ既存ノードを 1 階層上に
巻き上げる**。

```rust
// 擬似コード: turn コミット時に並列実行
async fn on_turn_committed(raw_node: RawNode, trio: &Trio) {
    join!(
        trio.worker.update_summary_index(raw_node.id),
        trio.supervisor.update_summary_index(raw_node.id),
        trio.observer.update_summary_index(raw_node.id),
    );
}

// 各エージェントの update_summary_index:
// 1. 自身の最新 level=0 ノードを読む
// 2. 自分のパーソナリティ・プロンプトで raw_node を取捨選択し、要約に追記/差替
//    Worker: 実装決定・コード差分・完了した手筋
//    Supervisor: 規範違反・整合性懸念・差し戻し履歴・最終判断の根拠
//    Observer: ユーザー意図・要件逸脱・記憶連続性・ループ兆候
// 3. level=0 の概算サイズが閾値超過 → LLM に level=1 ノードへ巻き上げを依頼
// 4. level=1 が膨らんだら level=2 へ ... ラダー圧縮を 1 段ずつ進める
```

**パーソナリティに沿った取捨選択**: Worker が捨てる情報を Supervisor が
拾う、その逆もある。3 view 合算は raw を最大限カバーする。
**巻き上げは少しずつ**: 1 回の rollup で複数階層を一度に処理しない。
**要約 ⇄ raw 双方向**: 要約ノードは `ref_raw_ids` / `ref_hashes` を持ち
必要時に raw 本文を逆参照できる。

### 4.3 次セッションの context 復元

次回 tmoe を起動したとき:

- feature 行 (タイトル・status)
- **エージェントごとに自分の view の summary index (level=高)** のみロード
- raw も中立要約も**ロードしない**。LLM が「もっと詳しく見たい」と要求したら
  要約ノードの `ref_hashes` 経由で raw を on-demand 取得

これで:

- 各エージェントは前回の自分のバイアスを引き継ぎ、頂点の個性が時間を超えて保たれる
- context は小さい (要約だけ)
- 必要時に hash で深掘りできる

### 4.4 機能単位ライフサイクル

```
ConciergeAgent  --create_feature-->  Feature(plan node)
              --start_work-->        Trio(worker+supervisor+observer)
                                       ├ each step appends raw_node
                                       ├ 各 agent が自分の summary_index を逐次延伸
                                       └ 巻き上げは段階的・1 階層ずつ
              --close_feature-->     status=done, 各 agent の最上位要約をルートに固定
```

---

## 5. ソース木 — PageIndex 概念のソース版

PageIndex (VectifyAI) は PDF/Markdown 専用ツールだが、tmoe ではライブラリ
そのものは使わず、**「木構造 + 推論探索 + 要約ノード + content_hash」という
思想だけ採用** して Rust で再実装する。

- `tree-sitter` + `tree-sitter-rust` / `-python` / `-typescript` / `-go` で
  言語別パース
- 共通中間表現:
  ```rust
  struct SourceNode {
      id: NodeId,
      kind: SourceKind,            // File | Module | Class | Function
      name: String,
      span: Span,
      children: Vec<NodeId>,
      summary: String,             // LLM 要約
      content_hash: ContentHash,   // BLAKE3
  }
  ```
- ビルド戦略: ファイル走査 → AST → 階層フラット化 → ボトムアップで Worker LLM
  に要約させて木を完成
- 出力先: SQLite (履歴と同じ DB の別テーブル `source_node`) + 永続キャッシュ
- 再ビルドはファイル mtime と content_hash 比較で差分のみ更新

---

## 6. エージェンティック検索 — ベクトルなし木探索

ベクトル類似度・埋め込みは使わない (要件)。LLM がノード要約を読んで
「次に開く子」を選び、木をトラバースする推論ベースの検索:

```rust
async fn search(query: &str, root: NodeId, llm: &dyn LlmClient) -> Vec<NodeId> {
    let mut frontier = vec![root];
    while let Some(node) = frontier.pop() {
        let children = load_children_summaries(node);
        let picks = llm.chat(NAVIGATE_PROMPT, &children).await?;
        if picks.is_terminal() { return picks.leaves; }
        frontier.extend(picks.next);
    }
}
```

検索対象はソース木 (tmoe-tree) と階層履歴 (tmoe-history) の双方。
Worker / Supervisor のどちらも RAG ツールとして呼べる。

---

## 7. 抽象 LLM レイヤ — 投機的推論を吸収する

```rust
#[async_trait]
trait LlmClient: Send + Sync {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse>;
    async fn chat_stream(&self, req: ChatRequest) -> Result<BoxStream<ChatDelta>>;
}

struct OpenAiCompatClient {
    base_url: Url,
    main_model: String,
    draft_model: Option<String>,    // 投機推論用 (バックエンド対応時のみ送る)
    spec_n_max: Option<u32>,
    api_key: Option<String>,
    capabilities: BackendCapabilities,
}
```

- `reqwest` + `eventsource-stream` で SSE
- `draft_model` はバックエンドが受け付けるときだけリクエストに混ぜる。
  未対応バックエンドでは **静かに no-op フォールバック** し `main_model` 単独で動く
- 起動時に `/v1/models` または初回チャットで投機推論が認識されたかを判定し
  `BackendCapabilities` を tmoe-llm 内部にキャッシュ
- バックエンド起動スクリプトは tmoe に同梱しない

### 7.1 対応 LLM ランタイム比較

| バックエンド | プラットフォーム | OpenAI 互換 | 投機推論 | 想定用途 |
|------------|----------------|-----------|--------|----------|
| **llama.cpp llama-server** | クロス (CPU/GPU/Metal) | あり | あり (`--spec-draft-model`) | 第一候補。Linux/macOS 双方で投機推論を活かせる |
| **vLLM** | NVIDIA GPU 中心 | あり | あり (Draft / EAGLE-3 / Suffix Decoding) | 高スループット環境 |
| **LM Studio** | クロス GUI | あり | あり (v0.3.10〜) | 手軽に検証 |
| **Rapid-MLX** | Apple Silicon 専用 | あり | **未実装** (Standard Speculative Decode / EAGLE-3 がロードマップ) | macOS のスピード優先。tmoe 側は no-op フォールバック |
| **その他 OpenAI 互換** | 任意 | あり | バックエンド依存 | 抽象レイヤで吸収 |

すべて tmoe-llm の `OpenAiCompatClient` 1 実装で扱い、差は
`backend_capabilities` で吸収する。

---

## 8. ツール層と権限分離

| ツール | 用途 | Worker | Supervisor | Observer |
|--------|------|:------:|:----------:|:--------:|
| `read_file(path, range)` | ファイル読取 | ✓ | ✓ | ✓ |
| `edit_file(path, patch)` | ファイル編集 | ✓ | — | — |
| `run_cmd(cmdline)` | シェル実行 | ✓ | — | — |
| `git_*` | git 操作 | ✓ | — | — |
| `search_source(query)` | tmoe-rag 木探索 | ✓ | ✓ | ✓ |
| `open_node(hash)` | content_hash → raw 本文 | ✓ | ✓ | ✓ |
| `metrics()` | 進捗メトリクス | — | ✓ | ✓ |

- 危険コマンド (`rm -rf` / `git reset --hard` 等) は明示的なブラックリストで
  Supervisor の拒否権を強制発動する安全弁
- 通常の不可逆操作は人間確認をスキップする (要件: 明示停止のみ)

---

## 9. TUI と Concierge

`ratatui` + `crossterm`。

```
┌──────────────────┬─────────────────────────────┐
│ Concierge 対話   │  Trio ライブログ            │
│  (Z 軸推進入力)  │  (Worker / Supervisor /     │
│                  │   Observer の発話と投票)    │
├──────────────────┼─────────────────────────────┤
│                  │  Observer 警告              │
│                  │  (ループ・記憶ずれ・逸脱)   │
├──────────────────┴─────────────────────────────┤
│  機能ツリー (feature 一覧 + 各 agent の要約 index)│
└────────────────────────────────────────────────┘
```

- 入力中も裏で 3 エージェントが回り続ける (常駐 = 非ブロッキング)
- ホットキー: `Ctrl-P` 一時停止 / `Ctrl-K` 強制中断 / `Ctrl-T` ツリー切替

---

## 10. Cargo workspace 構成

```
tmoe/
├── Cargo.toml                    # workspace
├── crates/
│   ├── tmoe-cli/                 # bin: ratatui TUI + tokio runtime
│   ├── tmoe-core/                # Trio orchestrator, vote, MessageBus
│   ├── tmoe-llm/                 # LlmClient trait + OpenAI 互換実装
│   ├── tmoe-tree/                # tree-sitter → AST 木 (PageIndex 風)
│   ├── tmoe-rag/                 # 木探索エージェンティック検索
│   ├── tmoe-history/             # SQLite + JSONL 階層履歴
│   ├── tmoe-tools/               # read/edit/run_cmd/git ツール
│   └── tmoe-prompts/             # 3 頂点のシステムプロンプト
├── config/
│   └── tmoe.toml.example
└── docs/
    └── DESIGN.md                 # 本書
```

主要外部クレート (確定候補):

| 用途 | クレート |
|------|---------|
| 非同期 | `tokio` |
| HTTP | `reqwest`, `eventsource-stream` |
| TUI | `ratatui`, `crossterm` |
| シリアライズ | `serde`, `serde_json` |
| SQLite | `rusqlite` (with `bundled`) |
| ID/ハッシュ | `ulid`, `blake3` |
| AST | `tree-sitter`, `tree-sitter-rust`, `tree-sitter-python`, `tree-sitter-typescript`, `tree-sitter-go` |
| 設定 | `figment` or `config-rs` (TOML) |
| ロギング | `tracing`, `tracing-subscriber` |
| Git | `git2` |
| エラー | `anyhow`, `thiserror` |

---

## 11. ロードマップ (フェーズ分割)

各フェーズは独立して動作確認可能。

✅ = 実 LLM (Rapid-MLX qwen3-coder-30b) 越しの e2e テストで終了条件を満たすことを確認済み。

| Phase | 内容 | 終了条件 | 状態 |
|-------|------|----------|------|
| **0** | workspace 足場 + DESIGN.md + README.md | `cargo check` 緑 | ✅ |
| **1** | tmoe-llm: LlmClient trait + OpenAiCompatClient + 能力検出 | llama-server に対し chat/stream/draft_model 送出が確認できる | ✅ (`e2e_real_backend`) |
| **2** | tmoe-history: raw + 3 並走 index、逐次コンパクション API | 1 feature を書き込み、再読込で 3 view が異なる粒度で復元される | ✅ (`e2e_real_refactor_compacted`) |
| **3** | tmoe-core::Agent 単体 + ツール呼び出し | "hello.rs を作って" を 1 エージェントで完遂 | ✅ (`e2e_real_program`) |
| **4** | Trio オーケストレータ + 合意ループ + park 状態 | `trio_consensus_loop_terminates` と `park_until_user_thrust` がパス | ✅ (`e2e_real_trio` + `e2e_real_trio_views` で ViewProvider 経由の view 注入も確認) |
| **5** | tmoe-tree + tmoe-rag | 自リポジトリで木構築、`search("ConciergeAgent")` が当該ノードに到達 | ✅ Worker が実 LLM 越しに `search_source` を ToolCall として実呼出し (`e2e_real_trio_search_source`)。LLM ボトムアップ要約 (`enrich_summaries`) は opt-in 実装済み (`SearchSourceTool::with_llm_summaries(true)`、content_hash でキャッシュ) — 既定は構造的フォールバック要約 |
| **6** | TUI と Concierge 常駐 | Trio 動作中に Concierge へメッセージ投入でき、redirect が USER REDIRECT として Worker に再投入される | ✅ TUI 動的タスク投入実装 + `e2e_real_cli` で `tmoe` バイナリが headless 完走 |
| **7** | Worktree 自動化と自己レビュー | `tmoe ask "..."` で worktree 切り出し→コミット→任意で PR ドラフト | ✅ worktree carve → commit → cleanup → `gh pr create --draft` (argv はスタブで検証)。**実 GitHub リモートでの PR 開設は未検証** |

---

## 12. 非対象 (今回の計画から外す)

- LLM サーバーの自動起動・管理スクリプト
- ベクトル DB / 埋め込みベース検索
- pageindex-mcp や PageIndex 本家 Python パッケージへの依存
- リモートエージェント連携 / クラウド LLM 専用機能
- **中立的な単一要約**を作るパス (三角形を潰すため意図的に持たない)
- **閾値到達時の一括コンパクション** (逐次 incremental のみ)
- **4 人目以上のエージェント追加 / 1 人運転モード** (「3」は固定)
