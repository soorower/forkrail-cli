#!/usr/bin/env bash
# The builder, forkrail-builder (crates/builder, deploy/install/builder.yaml): the Lambda function
# remote builds run in (`forkrail deploy --remote`). Its image is AWS's Lambda base image for
# Python 3.13 (Amazon Linux 2023, arm64), with Node 24, npm and corepack taken from AWS's Lambda
# Node 24 image and forkrail's builder and runtime added: the same Linux, Python and Node that
# deployments run on. Beside them, the other versions Lambda runs (Python 3.12 and 3.14 from AWS's
# Python images, Node 22 from its Node image in /opt/node22), Go in /opt/go, and Rust with zig and
# cargo-zigbuild in /opt/rust and /opt/zig (zig links and compiles C: the image has no C compiler),
# each pinned below by version and SHA-256. No Docker: crane (Homebrew's) assembles it from layers
# and pushes it to the install's ECR repository (stack forkrail-registry).
#
#   deploy/aws/builder.sh repo [--yes]      create or update the image repository (stack registry)
#   deploy/aws/builder.sh image             build the binaries and the image and push it; prints
#                                           its URI by digest. The base images are today's.
#   deploy/aws/builder.sh release [--yes] [KEY=VALUE...]
#                                           the image, then apply the stack with it (stack.sh shows
#                                           the changes and asks first)
#   deploy/aws/builder.sh plan [KEY=VALUE...]  the image, then what applying would change
#
# Needs cargo-zigbuild and crane (both Homebrew's), and Python 3.10 or newer for layers.py
# (python3.13 when there is one). FORKRAIL_PREBUILT=1 takes the builder and the runtime target/ has,
# without compiling (release.sh compiles everything at once first). The layers taken from AWS's
# images and the toolchains are kept in target/builder-layers by what they're made from (image
# digest, archive SHA-256, layers.py), so an image whose bases haven't changed only rebuilds
# forkrail's own layer.
set -euo pipefail

export AWS_PROFILE="${AWS_PROFILE:-forkrail}"
export AWS_REGION="${AWS_REGION:-us-east-1}"
export AWS_PAGER=""

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET=aarch64-unknown-linux-gnu
OUT="$ROOT/target/builder-image"
PYTHON_IMAGE=public.ecr.aws/lambda/python:3.13
NODE_IMAGE=public.ecr.aws/lambda/nodejs:24
# The other versions Lambda runs, which projects may ask for (crates/cli/src/runtimes.rs).
PYTHON_OTHERS="3.12 3.14"
NODE_OTHERS="22"
REPO=forkrail-builder
# Toolchains, by version and SHA-256 (go.dev/dl, static.rust-lang.org, ziglang.org, GitHub).
GO_URL=https://go.dev/dl/go1.27.4.linux-arm64.tar.gz
GO_SHA=3392582988a273a87ab5043ee1b2c9c17a2c3c00580bf9326c47a2e0aaaeac92
RUSTC_URL=https://static.rust-lang.org/dist/2026-11-12/rustc-1.100.0-aarch64-unknown-linux-gnu.tar.xz
RUSTC_SHA=fcbfa5b8a0cb28f0c4edfb6dda6910ee910993db31f05258613f5cd5b851263a
CARGO_URL=https://static.rust-lang.org/dist/2026-11-12/cargo-1.100.0-aarch64-unknown-linux-gnu.tar.xz
CARGO_SHA=d07ee80eb6ba3c5845c554ac88086e1ff5d63f83d99c3c09cb32504cbedd6844
RUST_STD_URL=https://static.rust-lang.org/dist/2026-11-12/rust-std-1.100.0-aarch64-unknown-linux-gnu.tar.xz
RUST_STD_SHA=c73f62fbe5d837f9807ed9e0076575dd8d3475c79af2b30e3570d328e6713f8f
ZIG_URL=https://ziglang.org/download/0.17.1/zig-aarch64-linux-0.17.1.tar.xz
ZIG_SHA=a1546d5646eb184437918d6635ca6b762a0e889eac09cef720ee7a1ed3fe7795
ZIGBUILD_URL=https://github.com/rust-cross/cargo-zigbuild/releases/download/v0.24.1/cargo-zigbuild-aarch64-unknown-linux-gnu.tar.xz

log() { echo "==> $*" >&2; }
die() { echo "$*" >&2; exit 1; }
usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }

PYTHON3="${PYTHON3:-$(command -v python3.13 || command -v python3)}"
LAYERS_PY="$ROOT/deploy/aws/builder/layers.py"
CACHE="$ROOT/target/builder-layers"
KEPT=() # the cached layers this run used; the others are deleted at the end

# layer NAME KEY COMMAND...: the cached layer for KEY (what it's made from), else COMMAND, which
# writes it to the path in $LAYER_OUT; adds the layer's path to LAYERS. Not in a subshell: KEPT.
layer() {
  local name="$1" key path
  key="$(printf '%s %s' "$2" "$(shasum -a 256 "$LAYERS_PY" | cut -c1-16)" | shasum -a 256 | cut -c1-16)"
  shift 2
  path="$CACHE/$name-$key.tar"
  if [ -f "$path" ]; then
    log "$name: kept layer ($key)"
  else
    mkdir -p "$CACHE"
    LAYER_OUT="$path.partial" "$@" >&2
    mv "$path.partial" "$path"
  fi
  KEPT+=("$path")
  LAYERS+=("$path")
}

# fetch FILE URL [SHA256]: download once into $OUT/../downloads, checked against its SHA-256 (or
# the .sha256 file beside it).
fetch() {
  local dir="$ROOT/target/builder-downloads" want
  mkdir -p "$dir"
  want="${3:-$(curl -sSfL "$2.sha256" | cut -d' ' -f1)}"
  if [ ! -f "$dir/$1" ] || [ "$(shasum -a 256 "$dir/$1" | cut -d' ' -f1)" != "$want" ]; then
    curl -sSfL -o "$dir/$1" "$2"
    [ "$(shasum -a 256 "$dir/$1" | cut -d' ' -f1)" = "$want" ] || die "$1 from $2 isn't the one pinned"
  fi
  echo "$dir/$1"
}

# Build the image and push it; prints its URI by digest.
image() {
  command -v crane >/dev/null || die "needs crane: brew install crane"
  "$PYTHON3" -c 'import sys; sys.exit(sys.version_info < (3, 10))' || die "layers.py needs Python 3.10 or newer: set PYTHON3"
  [ "${FORKRAIL_PREBUILT:-0}" = 1 ] ||
    (cd "$ROOT" && cargo zigbuild --release --target "$TARGET.2.34" -p forkrail-builder -p forkrail-runtime >&2)
  local bin
  for bin in forkrail-builder forkrail-runtime; do
    [ -x "$ROOT/target/$TARGET/release/$bin" ] || die "no $ROOT/target/$TARGET/release/$bin"
  done
  local account registry python_digest node_digest tag digest
  account="$(aws sts get-caller-identity --query Account --output text)"
  registry="$account.dkr.ecr.$AWS_REGION.amazonaws.com"
  aws ecr describe-repositories --repository-names "$REPO" >/dev/null 2>&1 || die "no ECR repository $REPO: deploy/aws/builder.sh repo"
  # The arm64 images by digest, so the layers below come from exactly what's recorded.
  python_digest="$(crane digest --platform linux/arm64 "$PYTHON_IMAGE")"
  node_digest="$(crane digest --platform linux/arm64 "$NODE_IMAGE")"
  log "base $PYTHON_IMAGE@$python_digest, Node from $NODE_IMAGE@$node_digest"
  rm -rf "$OUT" && mkdir -p "$OUT"
  LAYERS=()
  local labels=() version digest_other
  # Exported images go through bash -o pipefail: a download cut short fails the layer.
  layer node "$node_digest" bash -o pipefail -c 'crane export "$1" - | "$2" -I "$3" node "$LAYER_OUT"' - \
    "${NODE_IMAGE%:*}@$node_digest" "$PYTHON3" "$LAYERS_PY"
  "$PYTHON3" -I "$LAYERS_PY" forkrail "$OUT/forkrail.tar" \
    "$ROOT/target/$TARGET/release/forkrail-builder" "$ROOT/target/$TARGET/release/forkrail-runtime"
  LAYERS+=("$OUT/forkrail.tar")
  for version in $PYTHON_OTHERS; do
    digest_other="$(crane digest --platform linux/arm64 "${PYTHON_IMAGE%:*}:$version")"
    layer "python$version" "$digest_other" bash -o pipefail -c 'crane export "$1" - | "$2" -I "$3" python "$4" "$LAYER_OUT"' - \
      "${PYTHON_IMAGE%:*}@$digest_other" "$PYTHON3" "$LAYERS_PY" "$version"
    labels+=(--label "dev.forkrail.python$version=${PYTHON_IMAGE%:*}@$digest_other")
  done
  for version in $NODE_OTHERS; do
    digest_other="$(crane digest --platform linux/arm64 "${NODE_IMAGE%:*}:$version")"
    layer "node$version" "$digest_other" bash -o pipefail -c 'crane export "$1" - | "$2" -I "$3" node "$4" "$LAYER_OUT"' - \
      "${NODE_IMAGE%:*}@$digest_other" "$PYTHON3" "$LAYERS_PY" "$version"
    labels+=(--label "dev.forkrail.node$version=${NODE_IMAGE%:*}@$digest_other")
  done
  local go rustc cargo rust_std zig zigbuild
  go="$(fetch go.tar.gz "$GO_URL" "$GO_SHA")"
  layer go "$GO_SHA" bash -c '"$1" -I "$2" go "$LAYER_OUT" "$3"' - "$PYTHON3" "$LAYERS_PY" "$go"
  rustc="$(fetch rustc.tar.xz "$RUSTC_URL" "$RUSTC_SHA")"
  cargo="$(fetch cargo.tar.xz "$CARGO_URL" "$CARGO_SHA")"
  rust_std="$(fetch rust-std.tar.xz "$RUST_STD_URL" "$RUST_STD_SHA")"
  zig="$(fetch zig.tar.xz "$ZIG_URL" "$ZIG_SHA")"
  zigbuild="$(fetch cargo-zigbuild.tar.xz "$ZIGBUILD_URL")"
  layer rust "$RUSTC_SHA $CARGO_SHA $RUST_STD_SHA $ZIG_SHA $(shasum -a 256 "$zigbuild" | cut -d' ' -f1)" \
    bash -c '"$1" -I "$2" rust "$LAYER_OUT" "$3" "$4" "$5" "$6" "$7"' - "$PYTHON3" "$LAYERS_PY" "$rustc" "$cargo" "$rust_std" "$zig" "$zigbuild"
  # Cached layers no image uses any more.
  local file kept path
  for file in "$CACHE"/*.tar; do
    [ -e "$file" ] || continue
    kept=0
    for path in "${KEPT[@]}"; do [ "$file" = "$path" ] && kept=1; done
    [ "$kept" = 1 ] || rm -f "$file"
  done
  labels+=(--label "dev.forkrail.builder-sha256=$(shasum -a 256 "$ROOT/target/$TARGET/release/forkrail-builder" | cut -d' ' -f1)")
  labels+=(--label "dev.forkrail.runtime-sha256=$(shasum -a 256 "$ROOT/target/$TARGET/release/forkrail-runtime" | cut -d' ' -f1)")

  # ECR's password lives in a folder of this run's own, gone when it ends.
  export DOCKER_CONFIG
  DOCKER_CONFIG="$(mktemp -d)"
  trap 'rm -rf "$DOCKER_CONFIG"' EXIT
  aws ecr get-login-password | crane auth login "$registry" -u AWS --password-stdin >&2
  tag="$(cat "${LAYERS[@]}" <(echo "$python_digest") | shasum -a 256 | cut -c1-16)"
  local appends=() file
  for file in "${LAYERS[@]}"; do appends+=(-f "$file"); done
  crane append --base "${PYTHON_IMAGE%:*}@$python_digest" "${appends[@]}" -t "$registry/$REPO:layers-$tag" >&2
  crane mutate "$registry/$REPO:layers-$tag" -t "$registry/$REPO:$tag" \
    --entrypoint /opt/forkrail/forkrail-builder \
    --env "PATH=/var/lang/bin:/usr/local/bin:/usr/bin/:/bin:/opt/bin:/opt/forkrail/bin:/opt/go/bin:/opt/rust/bin:/opt/zig" \
    --env FORKRAIL_NODE_22=/opt/node22/bin \
    --env FORKRAIL_RUST_CC=/opt/rust/cc-bin \
    "${labels[@]}" \
    --env FORKRAIL_BOOTSTRAP=/opt/forkrail/forkrail-runtime \
    --env FORKRAIL_PYTHON=/var/lang/bin/python3.13 \
    --env FORKRAIL_NATIVE_PYTHON=1 \
    --label "dev.forkrail.base=$PYTHON_IMAGE@$python_digest" \
    --label "dev.forkrail.node=$NODE_IMAGE@$node_digest" \
    --label "dev.forkrail.commit=$(git -C "$ROOT" rev-parse --short HEAD)$(git -C "$ROOT" diff --quiet HEAD -- crates || echo +changes)" >&2
  digest="$(crane digest "$registry/$REPO:$tag")"
  # The image before its settings: its layers stay, as the image's own.
  crane delete "$registry/$REPO:layers-$tag" >&2
  log "pushed $registry/$REPO:$tag ($digest)"
  echo "$registry/$REPO@$digest"
}

cmd="${1:-}"
[ -n "$cmd" ] && shift
case "$cmd" in
  repo) "$ROOT/deploy/aws/stack.sh" apply registry "$@" ;;
  image) image ;;
  release)
    uri="$(image)"
    "$ROOT/deploy/aws/stack.sh" apply builder "ImageUri=$uri" "$@"
    ;;
  plan)
    uri="$(image)"
    "$ROOT/deploy/aws/stack.sh" plan builder "ImageUri=$uri" "$@"
    ;;
  *) usage ;;
esac
