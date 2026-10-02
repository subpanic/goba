#!/usr/bin/env bash
# In-container half of `docker/linux-ci.sh`. Mirrors the steps of the `test` job in
# .github/workflows/ci.yml, in order, on one Linux architecture.
#
# /src is the read-only checkout bind-mounted from the host.
# /w    is a per-arch volume: the sources are copied there (target/ excluded) so the
#       cargo build cache survives between runs, and the host tree is never written to.
#
# usage: ci-steps.sh <step>...   steps: clippy test release size smoke
set -euo pipefail

src=${SRC:-/src}
work=${WORK:-/w}

echo "== container: linux/$(uname -m)  $(rustc --version)  $(cargo --version)"

mkdir -p "$work"
# Refresh the working copy: drop everything but target/, keeping mtimes so cargo stays incremental.
cd "$work"
find . -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} +
tar -C "$src" --exclude=./target --exclude=./.git -cf - . | tar -C "$work" -xf -
cd "$work"

for step in "$@"; do
  echo "== $step"
  case $step in
    # .github/workflows/ci.yml — "Lint"
    clippy)
      cargo clippy --all-targets -- -D warnings
      ;;
    # — "Test"
    test)
      # shellcheck disable=SC2086 # TEST_ARGS is an intentional word-split passthrough
      cargo test ${TEST_ARGS:-}
      ;;
    # — "Release build"
    release)
      cargo build --release
      ;;
    # — "Binary size under 3 MB" (R61)
    size)
      size=$(wc -c < target/release/goba)
      echo "goba: $size bytes"
      test "$size" -le 3145728
      ;;
    # — "Smoke test the release binary"
    smoke)
      export GOBA_DIR="$(mktemp -d)/goba"
      G="$PWD/target/release/goba"

      live=$($G -q sleep 5)
      done_id=$($G -q sh -c 'echo hi')
      test "$($G -l | wc -l)" -eq 2

      until $G -ld | grep -q "$done_id"; do sleep 0.05; done
      $G -v "$done_id" | grep -qx hi

      $G -la | grep -q "$live"
      $G -k "$live"
      $G -ld | grep -q 'killed(SIGTERM)'

      $G -r "$live"; $G -r "$done_id"
      test -z "$($G -l)"
      ;;
    *)
      echo "unknown step: $step (want: clippy test release size smoke)" >&2
      exit 2
      ;;
  esac
done

echo "== ok: $* (linux/$(uname -m))"
