#!/usr/bin/env bash
# Run the Linux legs of .github/workflows/ci.yml on this machine.
#
#   docker/linux-ci.sh                          # linux/amd64 + linux/arm64, all steps
#   docker/linux-ci.sh --platform arm64         # one architecture
#   docker/linux-ci.sh --steps clippy           # only some steps
#   docker/linux-ci.sh --rust 1.97.0 --platform arm64
#
# An emulated architecture is slow enough that the parallel test harness overshoots the 150 ms
# window a11 asserts, so tests run single-threaded there by default; pass `--test-args ''` for the
# fully parallel run CI does.
#
# The toolchain is pinned to the same rustc/clippy the GitHub runners resolve (stable 1.98.1,
# the version in the CI log; see docker/linux.Dockerfile). A different toolchain here is the
# usual reason a lint is green locally and red in CI.
#
# Docker Desktop on Apple Silicon runs linux/amd64 through Rosetta when
# Settings → General → "Use Rosetta for x86_64/amd64 emulation" is enabled; without it
# it falls back to QEMU, which is several times slower for the release build.
#
# Nothing is written to the host tree: the checkout is mounted read-only and copied into
# a per-architecture volume (`goba-ci-<arch>-work`, plus `goba-ci-<arch>-cargo` for the
# registry), so builds are cached and never mix architectures.
set -euo pipefail

rust=${RUST:-1.98.1}
platforms=all
steps=clippy,test,release,size,smoke
test_args=
test_args_set=
reset=

usage() {
  sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
  case $1 in
    --platform) platforms=${2:?}; shift 2 ;;
    --platform=*) platforms=${1#*=}; shift ;;
    --steps) steps=${2:?}; shift 2 ;;
    --steps=*) steps=${1#*=}; shift ;;
    --rust) rust=${2:?}; shift 2 ;;
    --rust=*) rust=${1#*=}; shift ;;
    --test-args) test_args=${2:?}; test_args_set=1; shift 2 ;;
    --test-args=*) test_args=${1#*=}; test_args_set=1; shift ;;
    --reset) reset=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

repo=$(cd "$(dirname "$0")/.." && pwd)
host_arch=$(uname -m)

docker info >/dev/null 2>&1 || {
  echo "docker daemon is not reachable — start Docker Desktop first" >&2
  exit 1
}

# --steps a,b,c -> shell words
step_list=()
while IFS= read -r s; do
  [ -n "$s" ] && step_list+=("$s")
done < <(printf '%s\n' "${steps//,/$'\n'}")

arch_list=()
for p in ${platforms//,/ }; do
  case $p in
    all) arch_list+=(amd64 arm64) ;;
    amd64|linux/amd64|x86_64) arch_list+=(amd64) ;;
    arm64|linux/arm64|aarch64) arch_list+=(arm64) ;;
    *) echo "unknown platform: $p (want amd64|arm64|all)" >&2; exit 2 ;;
  esac
done

status=0
for arch in "${arch_list[@]}"; do
  work="goba-ci-$arch-work"
  cargo="goba-ci-$arch-cargo"

  if [ -n "$reset" ]; then
    docker volume rm -f "$work" "$cargo" >/dev/null
  fi

  [ "$arch" = "$host_arch" ] || echo "-- note: linux/$arch runs under emulation here"

  run_test_args=$test_args
  if [ -z "$test_args_set" ] && [ "$arch" != "$host_arch" ]; then
    # a11 asserts a 150 ms window between a spawn and a follow; emulated scheduling overshoots it
    # when 29 tests run in parallel. Serializing is a property of this host, not of the code.
    run_test_args='-- --test-threads=1'
    echo "-- note: emulated linux/$arch, so tests run single-threaded (override with --test-args)"
  fi

  image="goba-ci:$rust-$arch"
  echo "-- linux/$arch  [rust $rust]  steps: ${step_list[*]}"
  docker build --platform "linux/$arch" \
    --build-arg "RUST=$rust" \
    -t "$image" \
    -f "$repo/docker/linux.Dockerfile" "$repo/docker" >/dev/null || {
      echo "-- FAILED: cannot build $image" >&2
      status=1
      continue
    }

  docker run --rm \
    --platform "linux/$arch" \
    -e CARGO_TERM_COLOR=always \
    -e "TEST_ARGS=$run_test_args" \
    -v "$repo":/src:ro \
    -v "$work":/w \
    -v "$cargo":/usr/local/cargo/registry \
    -w /w \
    "$image" bash /src/docker/ci-steps.sh "${step_list[@]}" || {
      status=$?
      echo "-- FAILED: linux/$arch (exit $status)"
    }
done

if [ "$status" -ne 0 ]; then
  echo "-- one or more architectures failed"
  exit "$status"
fi
echo "-- all requested architectures passed"
