#!/usr/bin/env bash
# Finds or installs the quod binary for the Quod GitHub Action.
#
# QUOD_BIN       quod-bin input: used as-is when it runs (on PATH or a path)
# QUOD_VERSION   release tag to download, or `latest` (pre-releases included)
# QUOD_REPO      repository that publishes the releases (OWNER/NAME)
# GH_TOKEN       token for `gh` (the workflow's github.token)
#
# Writes `bin=<path>` to $GITHUB_OUTPUT. A download is checked against the
# release's SHA-256 file before use.
set -euo pipefail

out="${GITHUB_OUTPUT:-/dev/stdout}"
if command -v "$QUOD_BIN" > /dev/null 2>&1; then
  echo "bin=$(command -v "$QUOD_BIN")" >> "$out"
  exit 0
fi

case "${RUNNER_OS:-$(uname -s)}-${RUNNER_ARCH:-$(uname -m)}" in
  Linux-X64 | Linux-x86_64) target=x86_64-unknown-linux-gnu ext=tar.gz exe="" ;;
  macOS-ARM64 | Darwin-arm64) target=aarch64-apple-darwin ext=tar.gz exe="" ;;
  macOS-X64 | Darwin-x86_64) target=x86_64-apple-darwin ext=tar.gz exe="" ;;
  Windows-X64) target=x86_64-pc-windows-msvc ext=zip exe=".exe" ;;
  *)
    echo "::error::no prebuilt quod for ${RUNNER_OS:-?}-${RUNNER_ARCH:-?}; build it and set quod-bin"
    exit 1
    ;;
esac

tag="$QUOD_VERSION"
if [ "$tag" = latest ]; then
  tag="$(gh release list -R "$QUOD_REPO" --limit 1 --json tagName -q '.[0].tagName')"
  [ -n "$tag" ] || { echo "::error::$QUOD_REPO has no releases"; exit 1; }
fi

name="quod-$tag-$target"
dir="${RUNNER_TEMP:-/tmp}/quod-install"
mkdir -p "$dir"
gh release download "$tag" -R "$QUOD_REPO" -p "$name.$ext" -p "$name.$ext.sha256" -D "$dir" --clobber
cd "$dir"
if command -v sha256sum > /dev/null; then sha256sum -c "$name.$ext.sha256"; else shasum -a 256 -c "$name.$ext.sha256"; fi
if [ "$ext" = zip ]; then 7z x -y -bd "$name.zip" > /dev/null; else tar xzf "$name.tar.gz"; fi
bin="$dir/$name/quod$exe"
"$bin" --version
echo "bin=$bin" >> "$out"
