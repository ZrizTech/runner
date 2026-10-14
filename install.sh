#!/bin/sh
# Install the zriz runner binary from a GitHub release. It asks nothing and reads no stdin.
#   ZRIZ_RUNNER_VERSION   tag to install, for example v0.3.0 or 0.3.0 (default: the latest release)
#   ZRIZ_RUNNER_BIN_DIR   target folder (default: /usr/local/bin)
#   ZRIZ_RUNNER_BASE_URL  release download base (default: the GitHub releases of ZrizTech/runner).
#                         Only https is allowed. A file:// base is a test seam: it allows file and nothing else.
set -eu

fail() {
  printf 'install.sh: %s\n' "$1" >&2
  exit 1
}

bin_dir=${ZRIZ_RUNNER_BIN_DIR:-/usr/local/bin}
base=${ZRIZ_RUNNER_BASE_URL:-https://github.com/ZrizTech/runner/releases}

case $base in
  file://*) proto='=file' ;;
  *) proto='=https' ;;
esac
# Plain http is refused, also after a redirect.
fetch() { curl --proto "$proto" --proto-redir "$proto" --tlsv1.2 --retry 3 -fsSL "$@"; }

command -v curl > /dev/null 2>&1 || fail "curl is needed"
case $(uname -s) in
  Linux) ;;
  *) fail "this binary is for Linux only" ;;
esac
case $(uname -m) in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) fail "unsupported CPU: $(uname -m)" ;;
esac

if command -v sha256sum > /dev/null 2>&1; then
  sum_of() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum > /dev/null 2>&1; then
  sum_of() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
  fail "sha256sum or shasum is needed"
fi

tag=${ZRIZ_RUNNER_VERSION:-}
if [ -z "$tag" ]; then
  # The "latest" page redirects to the tag page.
  tag=$(fetch -o /dev/null -w '%{url_effective}' "$base/latest") || fail "cannot find the latest release"
  tag=${tag##*/}
fi
case $tag in
  v*) ;;
  *) tag=v$tag ;;
esac
case $tag in
  *[!A-Za-z0-9._-]* | "") fail "bad version: $tag" ;;
esac
asset=zriz-runner-${tag#v}-$arch-linux

work=$(mktemp -d) || fail "cannot make a temporary folder"
mkdir -p "$bin_dir" || fail "cannot make $bin_dir"
# The binary is written next to its target (same file system), so the last step is an atomic rename.
tmp=
trap 'rm -rf "$work" ${tmp:+"$tmp"}' EXIT HUP INT TERM
tmp=$(mktemp "$bin_dir/.zriz-runner.XXXXXX") || fail "cannot write in $bin_dir (use root, or set ZRIZ_RUNNER_BIN_DIR)"

fetch -o "$tmp" "$base/download/$tag/$asset" || fail "cannot download $asset (does the version $tag exist? check the release page)"
fetch -o "$work/SHA256SUMS" "$base/download/$tag/SHA256SUMS" || fail "cannot download SHA256SUMS"

want=$(awk -v f="$asset" '$2 == f { print $1 }' "$work/SHA256SUMS")
[ -n "$want" ] || fail "no checksum for $asset in SHA256SUMS"
got=$(sum_of "$tmp")
[ "$got" = "$want" ] || fail "checksum mismatch for $asset"

chmod 755 "$tmp" || fail "cannot chmod $tmp"
mv -f "$tmp" "$bin_dir/zriz-runner" || fail "cannot write $bin_dir/zriz-runner"
printf 'installed zriz-runner %s in %s\n' "$tag" "$bin_dir"
