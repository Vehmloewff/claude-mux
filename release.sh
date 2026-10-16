#!/bin/sh
# Cut a release: bump the version, commit, build every target, push, and
# publish a GitHub release with the binaries attached.
#
#   ./release.sh 0.2.0        # or: patch | minor | major
#
# Needs: rustup, cargo-zigbuild + zig (for the Linux builds), gh (logged in).
set -eu

cd "$(dirname "$0")"

targets="aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl"

fail() {
	echo "release.sh: $*" >&2
	exit 1
}

[ $# -eq 1 ] || fail "usage: ./release.sh <version|patch|minor|major>"
[ -z "$(git status --porcelain)" ] || fail "working tree is not clean"
[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || fail "not on main"
command -v gh >/dev/null || fail "gh is required"
command -v zig >/dev/null || fail "zig is required for the Linux builds"
cargo zigbuild --help >/dev/null 2>&1 || fail "cargo-zigbuild is required (cargo install cargo-zigbuild)"
gh auth status >/dev/null 2>&1 || fail "gh is not logged in (gh auth login)"

current="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"
IFS=. read -r major minor patch <<EOF
$current
EOF
case "$1" in
major) version="$((major + 1)).0.0" ;;
minor) version="$major.$((minor + 1)).0" ;;
patch) version="$major.$minor.$((patch + 1))" ;;
*) version="${1#v}" ;;
esac
echo "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || fail "invalid version: $version"
tag="v$version"
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && fail "tag $tag already exists"

echo "Releasing $current → $version"

# Build first, so a broken build doesn't leave a version-bump commit behind.
sed -i.bak "s/^version = \".*\"/version = \"$version\"/" Cargo.toml
rm -f Cargo.toml.bak
cargo update --workspace --offline --quiet
restore() {
	git checkout -- Cargo.toml Cargo.lock
}
trap restore EXIT

dist="target/dist"
rm -rf "$dist"
mkdir -p "$dist"
for target in $targets; do
	echo "Building $target"
	rustup target add "$target" >/dev/null
	case "$target" in
	*-linux-*) cargo zigbuild --release --locked --target "$target" ;;
	*) cargo build --release --locked --target "$target" ;;
	esac
	tar -czf "$dist/claude-mux-$target.tar.gz" -C "target/$target/release" claude-mux
done
(cd "$dist" && shasum -a 256 ./*.tar.gz | sed 's| \./| |' >SHA256SUMS)

trap - EXIT
git add Cargo.toml Cargo.lock
git commit -m "Release $tag"
git tag -a "$tag" -m "$tag"
git push origin main "$tag"

gh release create "$tag" "$dist"/*.tar.gz "$dist/SHA256SUMS" --title "$tag" --generate-notes
echo "Released $tag"
