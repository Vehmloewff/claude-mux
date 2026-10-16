#!/bin/sh
# Install claude-mux from the latest GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/Vehmloewff/claude-mux/main/install.sh | sh
#
# Environment:
#   CLAUDE_MUX_INSTALL_DIR  where to put the binary (default ~/.local/bin)
#   CLAUDE_MUX_VERSION      release tag to install, e.g. v0.2.0 (default: latest)
set -eu

repo="Vehmloewff/claude-mux"
dir="${CLAUDE_MUX_INSTALL_DIR:-$HOME/.local/bin}"
version="${CLAUDE_MUX_VERSION:-latest}"

fail() {
	echo "install.sh: $*" >&2
	exit 1
}

case "$(uname -s)" in
Darwin) os=apple-darwin ;;
Linux) os=unknown-linux-musl ;;
*) fail "unsupported OS: $(uname -s)" ;;
esac

case "$(uname -m)" in
x86_64 | amd64) arch=x86_64 ;;
arm64 | aarch64) arch=aarch64 ;;
*) fail "unsupported architecture: $(uname -m)" ;;
esac

# Rosetta reports x86_64; prefer the native binary on Apple silicon.
if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = 1 ]; then
	arch=aarch64
fi

asset="claude-mux-$arch-$os.tar.gz"
if [ "$version" = latest ]; then
	url="https://github.com/$repo/releases/latest/download/$asset"
else
	url="https://github.com/$repo/releases/download/$version/$asset"
fi

command -v curl >/dev/null || fail "curl is required"
command -v tar >/dev/null || fail "tar is required"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $url"
curl -fSL --progress-bar "$url" -o "$tmp/$asset" || fail "download failed"
tar -xzf "$tmp/$asset" -C "$tmp"

mkdir -p "$dir"
[ -f "$tmp/claude-mux" ] || fail "$asset does not contain claude-mux"
# Copy next to the target, then rename over it, so a running copy keeps its old inode.
cp "$tmp/claude-mux" "$dir/.claude-mux.new"
chmod +x "$dir/.claude-mux.new"
mv -f "$dir/.claude-mux.new" "$dir/claude-mux"

echo "Installed $("$dir/claude-mux" --version) to $dir"

case ":$PATH:" in
*":$dir:"*) ;;
*) echo "Note: $dir is not on your PATH. Add it to your shell profile, e.g.: export PATH=\"$dir:\$PATH\"" ;;
esac
