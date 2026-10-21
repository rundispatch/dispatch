#!/bin/sh
# Install or upgrade Dispatch: download this machine's release archive,
# verify it against the release's SHA256SUMS, and put `dispatch` into
# ${DISPATCH_INSTALL_DIR:-$HOME/.local/bin}.
#
#   curl -fsSL https://rundispatch.sh/install | sh
#
# The site redirects to this script as the latest release publishes it,
# https://github.com/rundispatch/dispatch/releases/latest/download/install.sh,
# which works the same. Builds exist for macOS and Linux, each on arm64 and
# x86_64. The script reads nothing beside itself, so it runs from a pipe.
#
# DISPATCH_VERSION=0.4.17 installs that release instead of the latest one.
# DISPATCH_RELEASE_URL takes the archive and SHA256SUMS from another base URL
# (file:// included) instead of GitHub.
#
# The new binary is copied beside the old one under a temporary name and
# renamed over it, so a running `dispatch` keeps its own file: macOS kills a
# process whose binary is written in place. A watched project's owner moves
# to the new version by itself at the next Dispatch command there. This
# script never edits a dotfile; when the directory is not on PATH it prints
# the line to add.
set -eu

releases=https://github.com/rundispatch/dispatch/releases

fail() {
  echo "dispatch install: $*" >&2
  exit 1
}

system="$(uname -s)"
machine="$(uname -m)"
case "$system-$machine" in
  Darwin-arm64) platform=macos-arm64 name="macOS on Apple silicon" ;;
  Darwin-x86_64)
    # A shell under Rosetta on Apple silicon says x86_64; the arm64 build
    # runs there natively.
    if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = 1 ]; then
      platform=macos-arm64 name="macOS on Apple silicon"
    else
      platform=macos-x86_64 name="macOS on Intel"
    fi
    ;;
  Linux-x86_64) platform=linux-x86_64 name="Linux on x86_64" ;;
  Linux-aarch64 | Linux-arm64) platform=linux-aarch64 name="Linux on aarch64" ;;
  *) fail "there is no Dispatch release for $system $machine; releases are built for macOS on Apple silicon (arm64) and Intel (x86_64), and Linux on x86_64 and aarch64" ;;
esac
asset="dispatch-$platform.tar.gz"

if [ -n "${DISPATCH_RELEASE_URL:-}" ]; then
  base="${DISPATCH_RELEASE_URL%/}"
elif [ -n "${DISPATCH_VERSION:-}" ]; then
  base="$releases/download/v${DISPATCH_VERSION#v}"
else
  base="$releases/latest/download"
fi

command -v curl >/dev/null 2>&1 || fail "curl is needed to download Dispatch"
if command -v shasum >/dev/null 2>&1; then
  digest() { shasum -a 256 "$1"; }
elif command -v sha256sum >/dev/null 2>&1; then
  digest() { sha256sum "$1"; }
else
  fail "shasum or sha256sum is needed to verify the download"
fi

dir="${DISPATCH_INSTALL_DIR:-$HOME/.local/bin}"
tmp="$(mktemp -d)"
staged=
trap 'rm -rf "$tmp"; [ -z "$staged" ] || rm -f "$staged"' EXIT
trap 'exit 1' HUP INT TERM

# SHA256SUMS first: it says whether this release has a build for this
# machine at all.
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || fail "could not download $base/SHA256SUMS"
expected="$(awk -v name="$asset" '$2 == name || $2 == "*" name { print $1; exit }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] ||
  fail "this release has no build for $name: $base/SHA256SUMS lists no $asset; nothing was installed"
echo "Downloading $asset from $base"
curl -fsSL "$base/$asset" -o "$tmp/$asset" || fail "could not download $base/$asset"
actual="$(digest "$tmp/$asset" | awk '{ print $1 }')"
[ "$actual" = "$expected" ] ||
  fail "$asset does not match SHA256SUMS (expected $expected, got $actual); nothing was installed"

mkdir "$tmp/archive"
tar -xzf "$tmp/$asset" -C "$tmp/archive" || fail "could not extract $asset"
[ -f "$tmp/archive/dispatch" ] || fail "$asset holds no dispatch executable"

mkdir -p "$dir"
dir="$(cd "$dir" && pwd)"
staged="$dir/.dispatch.install.$$"
cp "$tmp/archive/dispatch" "$staged"
chmod 755 "$staged"
mv -f "$staged" "$dir/dispatch"
staged=

version="$("$dir/dispatch" version)" || fail "the installed $dir/dispatch does not run"
echo "Installed $version at $dir/dispatch"

case ":${PATH:-}:" in
  *":$dir:"*) ;;
  *)
    case "$(basename "${SHELL:-sh}")" in
      zsh) profile=".zshrc" line="export PATH=\"$dir:\$PATH\"" ;;
      bash)
        # A terminal on macOS starts bash as a login shell.
        profile=".bashrc"
        [ "$system" != Darwin ] || profile=".bash_profile"
        line="export PATH=\"$dir:\$PATH\""
        ;;
      fish) profile=".config/fish/config.fish" line="fish_add_path \"$dir\"" ;;
      *) profile=".profile" line="export PATH=\"$dir:\$PATH\"" ;;
    esac
    echo
    echo "$dir is not on your PATH. Add this line to ~/$profile, then open a new terminal:"
    echo
    echo "  $line"
    ;;
esac
