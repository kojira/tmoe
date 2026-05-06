# tmoe

ローカル LLM で動く、3 エージェント協調型のコーディングエージェント。

> 詳細設計は [`docs/DESIGN.md`](docs/DESIGN.md) を参照。本書は概要と起動手順に絞る。

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
| `read_file` / `edit_file` | ファイル読み書き | Read / Write |
| `run_cmd` | プロセス実行 (危険コマンドはブロックリスト経由で拒否) | Run |
| `web_search` / `web_fetch` | **Web 検索・取得** (Obscura ヘッドレスブラウザ) | Read |

`web_search` / `web_fetch` は [h4ckf0r0day/obscura](https://github.com/h4ckf0r0day/obscura)
をバックエンドに使う。LLM フレンドリーな markdown 出力 (`obscura fetch <URL> --dump markdown`)
を直接 Worker に渡す。

```bash
# Obscura のインストール (Linux x86_64 バイナリ)
curl -LO https://github.com/h4ckf0r0day/obscura/releases/latest/download/obscura-x86_64-linux.tar.gz
tar xzf obscura-x86_64-linux.tar.gz
# あるいは cargo build --release (Apple Silicon を含むクロスビルド向け)

# tmoe からの参照: PATH に置くか、明示的にバイナリパスを指定する
export TMOE_OBSCURA_BIN=/path/to/obscura
```

`TMOE_OBSCURA_BIN` 未設定なら `obscura` (= PATH) にフォールバック。
Worker からの呼び出し例:

```json
{"tool":"web_search","args":{"query":"speculative decoding rust","engine":"duckduckgo"}}
{"tool":"web_fetch","args":{"url":"https://docs.vllm.ai/en/latest/features/spec_decode.html"}}
```

## インストール

```bash
git clone https://github.com/kojira/tmoe.git
cd tmoe
cargo build --release
```

## 設定

```bash
cp config/tmoe.toml.example config/tmoe.toml
$EDITOR config/tmoe.toml
```

最小限の編集ポイント:

```toml
[llm]
backend = "llama_cpp"
base_url = "http://127.0.0.1:8080/v1"
main_model = "qwen2.5-coder-32b-instruct"
draft_model = "qwen2.5-coder-0.5b-instruct"
```

LLM サーバー (llama-server / vLLM / LM Studio / Rapid-MLX 等) の起動は
tmoe には同梱しない。各自で別途起動しておくこと。

例 (llama.cpp):

```bash
llama-server \
  -m qwen2.5-coder-32b-instruct.gguf \
  -md qwen2.5-coder-0.5b-instruct.gguf \
  --port 8080
```

## 起動

```bash
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

## ロードマップ

| Phase | 内容 |
|-------|------|
| 0 | workspace 足場 + DESIGN.md + README ✅ |
| 1 | tmoe-llm: LlmClient trait + 能力検出 |
| 2 | tmoe-history: raw + 3 並走 index + 逐次コンパクション |
| 3 | tmoe-core::Agent 単体 + ツール呼び出し |
| 4 | Trio オーケストレータ + 合意ループ + park 状態 |
| 5 | tmoe-tree + tmoe-rag |
| 6 | TUI と Concierge 常駐 |
| 7 | Worktree 自動化と自己レビュー |

詳細は [`docs/DESIGN.md` §11](docs/DESIGN.md#11-ロードマップ-フェーズ分割) 参照。

## ライセンス

Apache-2.0
