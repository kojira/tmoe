# tmoe リリース手順

リリースは **タグを push するだけ** で完結する。CI が以下を全部やる:

1. macOS arm64 / macOS x86_64 / Linux x86_64 のバイナリをビルド
2. tar.gz + sha256 を GitHub Release に upload
3. `Formula/tmoe.rb` の `version` と各 `sha256` を新しい値に書き換えて main に commit + push

別 `homebrew-tmoe` リポは作らず、本体リポ自体が tap として機能する。

## 手順

```sh
# 1) workspace バージョンを bump
sed -i '' -E 's/^(version = )"[^"]+"/\1"0.2.0"/' Cargo.toml
cargo update --workspace      # Cargo.lock を更新
cargo test --workspace --lib  # サニティ
git commit -am "release: 0.2.0"
git push origin main

# 2) タグを切って push
git tag v0.2.0
git push origin v0.2.0
```

これだけ。CI が回ると 5–10 分後に:
- https://github.com/kojira/tmoe/releases/tag/v0.2.0 に成果物
- `main` ブランチに `Formula: bump tmoe to 0.2.0 [skip ci]` という commit

ユーザは何もしなくていい:

```sh
brew upgrade tmoe
```

## ロールバック

リリースを取り消したい場合:

```sh
gh release delete v0.2.0 --cleanup-tag --yes
git revert <formula-bump-commit-sha>
git push origin main
```

## ローカル動作確認 (タグ push 前)

CI を待たずに formula update スクリプトを試したいなら:

```sh
VERSION=0.2.0 \
SHA_ARM_MAC=$(shasum -a 256 some-tarball.tar.gz | awk '{print $1}') \
SHA_INTEL_MAC=... \
SHA_LINUX=... \
python3 .github/workflows/release.yml-snippet.py    # (インライン版を抽出して実行)
```

実際にはワークフローと同じ python ブロックを手で実行することになる。普段はやらなくて
よい。

## 既知の落とし穴

- **タグ名は `v<semver>` のみ**。`release` トリガは `v*` パターンに依存している。
- **CI が main に push するため、main ブランチの保護ルールで bot を許可** する必要がある。
  required reviewers / status checks は maintain しつつ、`tmoe-release-bot` (= `GITHUB_TOKEN`)
  を bypass list に入れる。プライベートリポなら何もしなくてよい。
- **macos-13 ランナーは将来 deprecated される**。GitHub から廃止アナウンスが出たら
  `aarch64-apple-darwin` から x86_64 へクロスコンパイルする方式に切替が必要。
