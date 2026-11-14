#!/usr/bin/env bash
# Builds every driver for every target Steery runs on, packs them into a release
# (PROTOCOL.md §7.1 in steery-dev/sdk) and, with --publish, publishes it on GitHub and
# records it on Steery's drivers-releases train.
#
#   ./release.sh <tag> [--publish] [--no-pack]
#
# Needs cargo-zigbuild and zig, the four targets installed with rustup, and `steery`
# on PATH (or STEERY=/path/to/steery) for the pack step; --publish also needs `steery`
# signed in to the steery-dev workspace.
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

# A published release is recorded on Steery's drivers-releases train: `build` as this
# script starts, then `shipped`, or `failed` if it stops first. The train's
# release-of-commit then finds the first release that carries a commit.
record() {
  "${STEERY:-steery}" --workspace steery-dev rows act --address resource/drivers-releases \
    --action "$1" --params "$2" "${@:3}" >/dev/null
}
step=building
if [[ $publish -eq 1 ]]; then
  [[ $pack -eq 1 ]] || { echo "release.sh: --publish needs the pack step" >&2; exit 2; }
  # Releases run side by side: tag the commit this build came from, never the default
  # branch's tip, which another release may have moved.
  commit=$(git rev-parse HEAD)
  git fetch --quiet origin
  [[ -n $(git branch -r --contains "$commit") ]] || { echo "release.sh: push $commit first" >&2; exit 1; }
  release="{\"version\":\"$tag\",\"commit\":\"https://github.com/steery-dev/drivers/commit/$commit\"}"
  record build "$release"
  published=0
  trap 'code=$?; [[ $code -eq 0 || $published -eq 1 ]] || record failed "{\"version\":\"$tag\",\"reason\":\"release.sh exited $code while $step\"}" || true' EXIT
fi

# Each driver says this release in `initialize` (`program.release`, .cargo/config.toml).
export STEERY_PROGRAM_RELEASE=$tag

# Linux builds link glibc 2.30 at most, so they run in every current Linux image.
targets=(aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu.2.30 aarch64-unknown-linux-gnu.2.30)
# Each crate and the executables it builds.
crates=(pi-family:steery-driver-omp,steery-driver-pi claude-code:steery-driver-claude-code codex:steery-driver-codex acp-family:steery-driver-grok,steery-driver-hermes,steery-driver-kilo,steery-driver-cursor,steery-driver-copilot,steery-driver-opencode,steery-driver-cline,steery-driver-goose,steery-driver-gemini,steery-driver-qwen,steery-driver-antigravity,steery-driver-openclaw,steery-driver-droid,steery-driver-kiro,steery-driver-openhands amp:steery-driver-amp crush:steery-driver-crush)

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
    step="building $crate for $target"
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
step=packing
"${STEERY:-steery}" programs pack --release "$tag" --out dist/release "${folders[@]}"

[[ $publish -eq 1 ]] || exit 0
step="publishing on GitHub"
gh release create "$tag" --title "$tag" --target "$commit" dist/release/*
published=1
record shipped "$release" --id "shipped-$tag" \
  --words "Published [its GitHub release](https://github.com/steery-dev/drivers/releases/tag/$tag)" || {
  echo "release.sh: $tag is published, but drivers-releases did not record it. Record it with:" >&2
  echo "  steery --workspace steery-dev rows act --address resource/drivers-releases --action shipped --id shipped-$tag --params '$release'" >&2
  exit 1
}
