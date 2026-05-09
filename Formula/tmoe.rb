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
  version "0.3.9" # tmoe:version
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/kojira/tmoe/releases/download/v#{version}/tmoe-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "eb8849ea89d18ff5b1aa3f4669a4495efbf80558c7d9e1b59f7ffde2c6ca3c2b" # tmoe:sha:aarch64-apple-darwin
    end
    on_intel do
      url "https://github.com/kojira/tmoe/releases/download/v#{version}/tmoe-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "29bdfdfb2e943414ee505edba8bf3707298cf15443167367292cadd0b109cbab" # tmoe:sha:x86_64-apple-darwin
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/kojira/tmoe/releases/download/v#{version}/tmoe-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "808c11154eac0e39dd5832da64f0a40b717506db1704e9dc9da187609039fdbb" # tmoe:sha:x86_64-unknown-linux-gnu
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
