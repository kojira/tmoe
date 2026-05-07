# tmoe

ローカル LLM で動く、3 エージェント協調型のコーディングエージェント。

> 詳細設計は [`docs/DESIGN.md`](docs/DESIGN.md) を参照。本書は概要と起動手順に絞る。

---

## クイックスタート (5 分で動かす)

**前提**: macOS (Apple Silicon / Intel) または Linux x86_64。LLM サーバーは tmoe には同梱しない。

### Homebrew で入れる (推奨)

```bash
brew tap kojira/tmoe
brew install tmoe
tmoe --version
```

### ソースからビルドする

Rust 1.85+ が必要。

```bash
# 1) clone & build
git clone https://github.com/kojira/tmoe.git
cd tmoe
cargo build --release

# 2) ローカル LLM を立てる (Apple Silicon の例)
#    Rapid-MLX は 3rd-party tap で配布されている。初回のみ tap が必要。
brew install raullenchai/rapid-mlx/rapid-mlx
rapid-mlx serve qwen3-coder-30b --port 8081 &
# 初回起動時は Hugging Face からモデル (4bit 量子化で 18GB 程度) を取得するので時間がかかる。

# 3) 環境を診断 (この 1 コマンドで設定/接続/オプショナル bin が表示される)
./target/release/tmoe doctor

# 4) 動かす — 既定値は Rapid-MLX 8081 + qwen3-coder-30b
./target/release/tmoe --headless --no-worktree --workdir /tmp/sandbox \
    "create hello.rs with a fn main printing hello"
```

**Linux / CUDA で llama.cpp を使う場合:**

```bash
llama-server -m qwen2.5-coder-32b-instruct.gguf --port 8081 --host 127.0.0.1
TMOE_LLM_BACKEND=llama_cpp TMOE_LLM_MODEL=qwen2.5-coder-32b-instruct \
    ./target/release/tmoe doctor
```

**設定の永続化** (任意):
```bash
mkdir -p ~/.tmoe
cp config/tmoe.toml.example ~/.tmoe/config.toml
$EDITOR ~/.tmoe/config.toml
```
`~/.tmoe/config.toml` があれば自動で読み込まれる。無ければ環境変数 (`TMOE_LLM_*`) と
内蔵デフォルト (Rapid-MLX 8081 + qwen3-coder-30b) にフォールバックする。

**LLM が立っていないと?** `tmoe` は最初に `GET /v1/models` で preflight して、
失敗時はエラースタックトレースではなく**起動コマンド例つきのヒント**を出して終了する
(History DB や worktree も作らずに帰る)。`tmoe doctor` でも同じ診断が見られる。

---

## なぜ「3 + 1」なのか

tmoe は 3 つのエージェント (Worker / Supervisor / Observer) と 1 人のユーザーで
構成される。この個数は気分や慣習ではなく、**幾何学的な必然性**から決まっている。

1. **平面決定性**: 同一直線上にない 3 点は平面を一意に決める。
   2 点では平面が無数、4 点は同一平面に乗らない歪みを生む。
2. **剛性**: 三角形は最小の剛体多角形。3 辺で形が一意。4 辺以上は対角線が
   ないと潰れる自由度を持つ。
3. **3 + 1 で 3 次元空間が完成する**: 3 エージェントが平面 (XY) を張り、
   ユーザーがそれに直交する **Z 軸の推進力**として加わって初めて 3D 運動空間が
   成立する。エージェントを 4・5 と増やしても平面の歪みが増えるだけで、
   Z 軸方向の力は得られない。**Z はユーザーからしか来ない**。

```
                 Z 軸 (ユーザー Z 軸推進力)
                 ▲
                 │
   Worker ──────┼───── Supervisor    ← 合意平面 (XY)
       \        │       /
        \       │      /
         \    Observer
          \   /
           \ /
            ●  feature の現在地
```

頂点数を「3」に固定することは tmoe の **設計原則** である。
増やしたり減らしたりしない。社会・宗教的アナロジー (三権分立・三位一体など)
は意図的に使わない。根拠は完全に幾何学的である。

---

## 三角形の頂点

| 頂点 | 方向ベクトル | 目的関数 | プロンプト基調 |
|------|-------------|--------|---------------|
| **Worker** (推進軸) | 「進めよ・形にせよ」 | 課題解決度・完了度 | 実装志向・速度志向・楽観的 |
| **Supervisor** (批判軸) | 「立ち止まれ・整えよ」 | 整合性・安全性・要件適合度 | 慎重・批判的・規範志向 |
| **Observer** (俯瞰軸) | 「外から見よ・全体を測れ」 | ユーザー意図照合・記憶連続性・ループ検出 | 外在視点・メタ認知 |

3 者のプロンプトが似てしまうと 3 点が同一直線上に並んで平面が縮退する。
Observer はそれを監視する。

## 第 4 の軸はユーザー

ユーザーは合意平面に直交する Z 軸推進力。エージェントは平面合意で
意思決定の **形** を一意化し、ユーザーが **力** を与える。
tmoe は両者の積でしか前進しない。

`Concierge` は 4 人目のエージェントではない。ユーザーの Z 軸推進を
平面に伝達する I/O チャネルである。

## 合意プロトコル

前進条件:

```
plane_ok           = (3 者すべてが approve)
confidence_sum     >= 2.4
triangle_balance   >= 0.6   // min/max。1.0 が正三角形
z_thrust           > 0.0    // ユーザーからの GO
```

これらすべてが揃ったとき、かつそのときに限り proposal が commit される。
**多数決ではない**。1 人が強く反対していれば前進しない。
平面合意ができても Z 推進が無ければ park 状態に入り、Concierge は
常時受付可能のままユーザー入力を待つ。

平面内では Supervisor が拒否権を持ち、3 次元上の最終 GO はユーザーが握る。

---

## 三視点の記憶

履歴は **共通 raw ツリー 1 本** + **エージェント別要約 index 3 本** の
四層構造で持つ。**中立要約は作らない** (三角形を潰すため)。

- 各エージェントは自分のパーソナリティで raw を取捨選択し、自分専用の
  要約 index を **逐次** 延伸する (rolling summary、一括コンパクション禁則)
- 同じ事実から 3 つの解釈が並走することで、互いの盲点を補完する
- 次セッションでは各エージェントが自分の要約 index だけをロード。raw は
  hash で必要時に on-demand 取得

## PageIndex 思想の流用

ベクトル DB / 埋め込みは使わない。VectifyAI の PageIndex の思想
(木 + 推論探索 + 要約ノード + content_hash) だけを採用し、
ソース AST と会話履歴の双方に Rust で再実装する。

## 投機的推論で速度を稼ぐ

ローカル LLM 前提のコーディング作業で実用速度を出すため、
ドラフトモデル (小さく速いモデル) が予想を量産し、本体モデルが検証する
**投機的推論 (speculative decoding)** を活用する。

tmoe-llm は OpenAI 互換 HTTP の抽象レイヤで、`draft_model` 設定値を
バックエンドが受け付ければ送り、未対応なら no-op フォールバックする。

| バックエンド | プラットフォーム | 投機推論 |
|------------|----------------|--------|
| llama.cpp llama-server | クロス | あり |
| vLLM | NVIDIA GPU 中心 | あり |
| LM Studio | クロス GUI | あり |
| Rapid-MLX | Apple Silicon 専用 | 未実装 (ロードマップ) |

詳細は [`docs/DESIGN.md` §7](docs/DESIGN.md#7-抽象-llm-レイヤ--投機的推論を吸収する) 参照。

---

## ビルトインスキル

Worker は次のツールを既定で持つ (`tmoe-tools`):

| ツール | 用途 | Permission |
|--------|------|-----------|
| `read_file` | ファイル読み取り | Read |
| `edit_file` | ファイル全文書き込み (新規作成・全置換) | Write |
| `patch_file` | **位置指定の部分編集** (Aider 風 search/replace、唯一マッチを既定で要求) | Write |
| `list_files` | glob で**ファイル列挙** (`**/*.rs` 等、target/.git/node_modules を skip) | Read |
| `grep_text` | リテラル/正規表現で**行検索** (case_insensitive、サブパス限定可) | Read |
| `run_cmd` | プロセス実行 (危険コマンドはブロックリスト経由で拒否) | Run |
| `web_search` / `web_fetch` | **Web 検索・取得** (Obscura ヘッドレスブラウザ) | Read |

`web_search` / `web_fetch` は [h4ckf0r0day/obscura](https://github.com/h4ckf0r0day/obscura)
をバックエンドに使う。`obscura fetch <URL> --dump text` で得られる
レンダリング済みテキストを Worker に直接渡す。

```bash
# Obscura のインストール
# Apple Silicon
curl -LO https://github.com/h4ckf0r0day/obscura/releases/latest/download/obscura-aarch64-macos.tar.gz
tar xzf obscura-aarch64-macos.tar.gz
# Linux x86_64
# curl -LO https://github.com/h4ckf0r0day/obscura/releases/latest/download/obscura-x86_64-linux.tar.gz

# tmoe からの参照: PATH に置くか、明示的にバイナリパスを指定する
export TMOE_OBSCURA_BIN=/path/to/obscura
```

実 Obscura に対する gated e2e:

```bash
TMOE_E2E_OBSCURA_BIN=/path/to/obscura \
  cargo test -p tmoe-tools --test e2e_real_obscura -- --ignored
```

`TMOE_OBSCURA_BIN` 未設定なら `obscura` (= PATH) にフォールバック。
Worker からの呼び出し例:

```json
{"tool":"web_search","args":{"query":"speculative decoding rust","engine":"duckduckgo"}}
{"tool":"web_fetch","args":{"url":"https://docs.vllm.ai/en/latest/features/spec_decode.html"}}
```

## プロジェクト固有の指示 (AGENTS.md)

ワークディレクトリに `AGENTS.md` を置くと、tmoe はそれを Worker の初期プロンプトに
prepend する。git ルートからサブディレクトリまで遡って収集し、**ルート (浅い階層) →
リーフ (深い階層) の順** で連結するので、プロジェクト全体ルールはルートに、
モジュール固有の制約はサブディレクトリに置けば階層的に適用される。

例:
```markdown
# AGENTS.md
- All output files must contain TMOE_PROJECT_RULE on the first line as a comment.
- Use snake_case for filenames.
- Never use `unwrap()` in production code; prefer explicit error handling.
```

同階層に `TMOE.md` があれば AGENTS.md の後に重ねて読む (= tmoe 固有の上書きを許す)。
空ファイルや空白だけのファイルは無視される。

## 詳しい設定

設定は以下の優先順位で解決される:

1. `--config <path>` で渡された TOML
2. `~/.tmoe/config.toml`
3. 環境変数 `TMOE_LLM_URL` / `TMOE_LLM_MODEL` / `TMOE_LLM_BACKEND` / `TMOE_LLM_DRAFT` / `TMOE_LLM_API_KEY`
4. 内蔵デフォルト: Rapid-MLX バックエンド、`http://127.0.0.1:8081/v1`、`qwen3-coder-30b`

`config/tmoe.toml.example` の `[llm]` 全部:

```toml
[llm]
# backend: llama_cpp | vllm | lm_studio | rapid_mlx | openai_compat
backend     = "rapid_mlx"
base_url    = "http://127.0.0.1:8081/v1"
main_model  = "qwen3-coder-30b"
draft_model = ""             # rapid_mlx は投機推論未対応 → 空でよい (llama_cpp なら指定可)
spec_n_max  = 16
```

LLM サーバー (llama-server / vLLM / LM Studio / Rapid-MLX) の起動は tmoe には同梱しない。
各自で別途起動しておくこと。`tmoe doctor` で `GET /v1/models` の到達性を 1 ショット確認できる。

`llama.cpp` 例:

```bash
llama-server \
  -m qwen2.5-coder-32b-instruct.gguf \
  -md qwen2.5-coder-0.5b-instruct.gguf \
  --port 8081
```

## 起動

```bash
# CLI (headless モード) - 一発で feature を完走させる
./target/release/tmoe --headless "<task>"

# TUI モード (デフォルト) - 動作中も Concierge から介入できる
./target/release/tmoe "<task>"

# 引数なしで起動して TUI 内で task を打ち込むこともできる
./target/release/tmoe
```

ホットキー (動作中の Trio に Z 軸推進シグナルを直接届ける):

| キー | 動作 | UserThrust |
|------|------|-----------|
| `Ctrl-P` | Trio を一時停止 (park) | `Pause` |
| `Ctrl-G` | park 状態の Trio を再開 | `Go { strength: 1.0 }` |
| `Ctrl-K` | 現在の feature を強制中断 | `Stop` |
| `Ctrl-C` / `Esc` | tmoe そのものを終了 | — |

park 中も Concierge ペインは入力を受け付け続けるため、エージェント停止と
ユーザー操作の両立が保たれる (常駐非ブロッキング設計)。
e2e: `crates/tmoe-cli/tests/e2e_hotkey_pause.rs` がこの不変条件を検証する。

---

## ディレクトリ構成

```
tmoe/
├── crates/
│   ├── tmoe-cli/        # bin: ratatui TUI
│   ├── tmoe-core/       # Trio orchestrator
│   ├── tmoe-llm/        # 抽象 LLM レイヤ
│   ├── tmoe-tree/       # tree-sitter → AST 木
│   ├── tmoe-rag/        # 木探索エージェンティック検索
│   ├── tmoe-history/    # SQLite + JSONL 階層履歴
│   ├── tmoe-tools/      # read/edit/run_cmd/git
│   └── tmoe-prompts/    # 3 頂点のシステムプロンプト
├── config/
│   └── tmoe.toml.example
└── docs/
    └── DESIGN.md        # 詳細設計
```

## テスト

```bash
# 通常テスト (Mock LLM のみ、決定論的・高速)
cargo test --workspace

# 実 LLM に対する gated e2e (Worker が実際に Rust の FizzBuzz を書く)
TMOE_E2E_LLM_URL=http://127.0.0.1:8080/v1 \
TMOE_E2E_LLM_MODEL=qwen2.5-coder-32b-instruct \
TMOE_E2E_LLM_BACKEND=llama_cpp \
  cargo test --workspace -- --ignored
```

`TMOE_E2E_LLM_*` を設定しないと実 LLM テストは skip される。
バックエンドは `llama_cpp | vllm | lm_studio | rapid_mlx | openai_compat` を選択可能。

## ロードマップと現状

各 Phase の状態は **ライブラリ完了** と **end-to-end 実機 LLM 経由で検証済み** を区別する。
✅ = 実 LLM (Rapid-MLX qwen3-coder-30b) を介した e2e テストで挙動が確認済み。

| Phase | 内容 | 状態 | 検証 e2e |
|-------|------|------|----------|
| 0 | workspace 足場 + DESIGN.md + README | ✅ | `cargo check` |
| 1 | tmoe-llm: LlmClient trait + 能力検出 | ✅ | `e2e_real_backend` |
| 2 | tmoe-history: raw + 3 並走 index + 逐次コンパクション | ✅ | `e2e_real_refactor_compacted` (3 view 別パーソナリティ) |
| 3 | tmoe-core::Agent 単体 + ツール呼び出し | ✅ | `e2e_real_program` (3 シナリオ) |
| 4 | Trio オーケストレータ + 合意ループ + park 状態 | ✅ | `e2e_real_trio` / `e2e_real_trio_views` (Worker view → Supervisor / Observer に注入) |
| 5 | tmoe-tree + tmoe-rag | ✅ | `e2e_real_trio_search_source` (Worker が `search_source` を実呼出し) |
| 6 | TUI と Concierge 常駐 (動的タスク投入対応) | ✅ | `e2e_real_cli` (バイナリ headless 完走) + ユニット (TUI dynamic spawn) |
| 7 | Worktree 自動化 + 任意 PR ドラフト | ✅ | `runtime` ユニット (`gh pr create` argv をスタブで検証) + `e2e_real_cli` (worktree carve + commit + cleanup) |

**実 LLM 経由で確認済みの結線層 (= 「ライブラリは動くが繋がっていない」状態の解消):**
`tmoe "<task>"` 1 コマンドで「feature 作成 → worktree 切り出し → Trio (Worker/Supervisor/Observer) +
Z 軸推進 → ツール実行 (read/edit/patch/list/grep/run_cmd/search_source/web_*) → ViewProvider 経由で
3 view brief を投票に渡す → raw + 3 view 並走逐次コンパクション → git commit → 任意で `gh pr create --draft`」
までが 1 セッションで通る。Concierge からの redirect は USER REDIRECT として Worker に再投入され、
park 状態は ThrustChannel 経由で次の Z 軸推進を待つ。`--max-rounds N` でセッション境界を制御。

**残る (今日時点の) 設計上のスコープ:**
- 実 LLM が `web_fetch` を呼ぶ経路は obscura スタブ越しに e2e 確認済み。**実 obscura バイナリ越しは未検証** (`TMOE_E2E_OBSCURA_BIN` 設定時の単体 e2e のみ)。
- `gh pr create --draft` は argv 構築をスタブで検証済み。**実 GitHub リモートでの PR 開設は未検証**。

詳細は [`docs/DESIGN.md` §11](docs/DESIGN.md#11-ロードマップ-フェーズ分割) 参照。

## ライセンス

Apache-2.0
