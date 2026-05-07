# Homebrew formula for tmoe.
#
# このリポ自体が tap として機能する (== 別 `homebrew-tmoe` リポは作らない設計)。
# `version` と各 `sha256` は `git tag vX.Y.Z && git push origin vX.Y.Z` 後に
# `.github/workflows/release.yml` が自動で書き戻す。手で触らなくてよい。
#
# ユーザ向け install 手順:
#   brew tap kojira/tmoe https://github.com/kojira/tmoe
#   brew install tmoe
#   tmoe --version
#
# サブスク認証で動かす場合:
#   tmoe codex login        # ChatGPT Pro/Plus
# ローカル LLM で動かす場合:
#   rapid-mlx serve qwen3-coder-30b --port 8081 &
#   tmoe doctor             # 接続確認
#   tmoe "<task>"

class Tmoe < Formula
  desc "3-agent collaborative coding agent (Worker / Supervisor / Observer + user as Z-axis)"
  homepage "https://github.com/kojira/tmoe"
  version "0.2.1" # tmoe:version
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/kojira/tmoe/releases/download/v#{version}/tmoe-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "e20b9f5b8d4232eb8801d742273765220cd2cf5e2fafe7d6cb971d9b11e8ef32" # tmoe:sha:aarch64-apple-darwin
    end
    on_intel do
      url "https://github.com/kojira/tmoe/releases/download/v#{version}/tmoe-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "1b699d9a3f0bcbf08fbfe9065104f8a4485d874f2d06bda27afcc137f8bfdcbd" # tmoe:sha:x86_64-apple-darwin
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/kojira/tmoe/releases/download/v#{version}/tmoe-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "a05a69c342c879cfd4ff467b6b23853c8f70f8a2dfeca65b2178ef283d97aeac" # tmoe:sha:x86_64-unknown-linux-gnu
    end
  end

  def install
    bin.install "tmoe"
    # 設定例とライセンスもインストール (formula から brew info で見える)。
    pkgshare.install "tmoe.toml.example" if File.exist?("tmoe.toml.example")
    doc.install "README.md" if File.exist?("README.md")
  end

  def caveats
    <<~EOS
      tmoe needs an OpenAI-compatible LLM backend. Two ways to set it up:

      1) ChatGPT Pro/Plus subscription (Codex backend):
           tmoe codex login
         Then set in ~/.tmoe/config.toml:
           [llm]
           backend = "codex"
           main_model = "gpt-5.4"  # or your preferred Codex-allowed model

      2) Local LLM (default, Apple Silicon recommended):
           brew install rapid-mlx        # if available, or follow rapid-mlx docs
           rapid-mlx serve qwen3-coder-30b --port 8081 &

      Then run:
           tmoe doctor       # diagnose backend connectivity
           tmoe "<task>"     # invoke the agent
    EOS
  end

  test do
    assert_match "tmoe", shell_output("#{bin}/tmoe --version")
    # `tmoe --help` should mention the codex login subcommand we ship.
    assert_match "codex login", shell_output("#{bin}/tmoe --help")
  end
end
