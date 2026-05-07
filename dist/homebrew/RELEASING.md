# Release & Homebrew tap update

tmoe は **GitHub Release の pre-built バイナリ** + **`kojira/homebrew-tmoe` tap** で配る。
このディレクトリの `tmoe.rb` は tap repo の `Formula/tmoe.rb` の参考コピー。

## 1 回限りの初期セットアップ

別リポを 1 つ作る:

```sh
gh repo create kojira/homebrew-tmoe --public --description "Homebrew tap for tmoe" \
  --add-readme
git clone git@github.com:kojira/homebrew-tmoe.git
cd homebrew-tmoe
mkdir -p Formula
cp /path/to/tmoe/dist/homebrew/tmoe.rb Formula/tmoe.rb
```

## 各リリース時の手順

1. **バージョンを bump** (workspace 全体で `Cargo.toml` の `version = "x.y.z"`)。
   `cargo update --workspace` で `Cargo.lock` も更新。コミットして push。

2. **タグを切って push**:
   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```
   `.github/workflows/release.yml` が macOS arm64 / macOS x86_64 / Linux x86_64 の
   3 バイナリをビルドし、tarball + sha256 を GitHub Release に upload する。

3. **GitHub Actions の release ジョブ summary を開く**。"Build per-target SHA256
   summary (for formula)" ステップに以下のような出力が出る:
   ```ruby
   # tmoe-v0.1.0-aarch64-apple-darwin.tar.gz
   sha256 "abc123..."
   # tmoe-v0.1.0-x86_64-apple-darwin.tar.gz
   sha256 "def456..."
   # tmoe-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
   sha256 "789xyz..."
   ```

4. **tap repo の `Formula/tmoe.rb` を更新**:
   - `version "0.1.0"` を新バージョンに
   - 各 `sha256 "REPLACE_WITH_..."` を上の summary から差し替え
   - コミットして push

5. **動作確認**:
   ```sh
   brew untap kojira/tmoe 2>/dev/null
   brew tap kojira/tmoe
   brew install tmoe
   tmoe --version
   ```

## 失敗時の戻し方

GitHub Release を delete + tag を delete:

```sh
gh release delete v0.1.0 --cleanup-tag --yes
```

その後で再度 tag を切り直す。tap formula は既に更新していた場合は revert commit を
入れて push すれば古いバージョンに戻る。
