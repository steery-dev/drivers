#!/usr/bin/env bash
# Builds every driver for every target Steery runs on, packs them into a release
# (PROTOCOL.md §7.1 in steery-dev/sdk) and, with --publish, publishes it on GitHub.
#
#   ./release.sh <tag> [--publish] [--no-pack]
#
# Needs cargo-zigbuild and zig, the four targets installed with rustup, and `steery`
# on PATH (or STEERY=/path/to/steery) for the pack step.
set -euo pipefail

usage() { echo "usage: $0 <tag> [--publish] [--no-pack]" >&2; exit 2; }
[[ $# -ge 1 ]] || usage
tag=$1; shift
publish=0 pack=1
for arg in "$@"; do
  case $arg in
    --publish) publish=1 ;;
    --no-pack) pack=0 ;;
    *) usage ;;
  esac
done

root=$(cd "$(dirname "$0")" && pwd)
cd "$root"

# Linux builds link glibc 2.30 at most, so they run in every current Linux image.
targets=(aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu.2.30 aarch64-unknown-linux-gnu.2.30)
# Each crate and the executables it builds.
crates=(pi-family:steery-driver-omp,steery-driver-pi claude-code:steery-driver-claude-code codex:steery-driver-codex)

rm -rf dist
for zig_target in "${targets[@]}"; do
  target=${zig_target%.2.30}
  # zig 0.16 refuses the `-oso_prefix` rustc passes for Apple targets; a Mac links its own
  # targets with Apple's linker.
  build=(cargo zigbuild)
  [[ $target == *-apple-darwin && $(uname -s) == Darwin ]] && build=(cargo build)
  mkdir -p "dist/$target"
  for entry in "${crates[@]}"; do
    crate=${entry%%:*}
    (cd "$crate" && "${build[@]}" --release --locked --target "$zig_target")
    IFS=, read -ra bins <<<"${entry#*:}"
    for bin in "${bins[@]}"; do
      cp "$crate/target/$target/release/$bin" "dist/$target/"
    done
  done
done

[[ $pack -eq 1 ]] || exit 0
folders=()
for zig_target in "${targets[@]}"; do
  target=${zig_target%.2.30}
  folders+=("$target=dist/$target")
done
"${STEERY:-steery}" programs pack --release "$tag" --out dist/release "${folders[@]}"

[[ $publish -eq 1 ]] || exit 0
gh release create "$tag" --title "$tag" dist/release/*
