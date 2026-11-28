#!/usr/bin/env bash
# The image optimizer, forkrail-image (deploy/image, deploy/install/image.yaml): Image
# Optimization's resizer, which the routers invoke on a cache miss for `/_vercel/image`. One per
# region a router runs in, beside that region's bucket (its sources); what every one makes is kept
# once, in the home bucket's images/ (Vercel's shared cache).
#
#   deploy/aws/image.sh release [--yes]   install its packages for Lambda (Linux, arm64), zip it,
#                                         upload it to each region's bucket, and apply the stack in
#                                         each region with it (stack.sh shows the changes and asks
#                                         first); then the home bucket's rule that deletes optimized
#                                         images 31 days after they're made (its other lifecycle
#                                         rules are read and kept). Regions: FORKRAIL_IMAGE_REGIONS,
#                                         default "us-east-1 us-west-1"
#   deploy/aws/image.sh plan              the same, but only show what applying would change
#   deploy/aws/image.sh test              its tests, with sharp for this Mac
#   deploy/aws/image.sh serve [PORT]      run it here (default port 3090) over a local router's
#                                         FORKRAIL_STATIC_DIR (FORKRAIL_IMAGE_DIR): the router's
#                                         FORKRAIL_IMAGE_URL=http://127.0.0.1:PORT/
#   deploy/aws/image.sh try KEY W Q [FORMAT]
#                                         invoke the deployed one with your own credentials on a
#                                         static file (static/<sha256>); the image goes to ./image.out
set -euo pipefail

export AWS_PROFILE="${AWS_PROFILE:-forkrail}"
export AWS_REGION="${AWS_REGION:-us-east-1}"
export AWS_PAGER=""

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SRC="$ROOT/deploy/image"
OUT="$ROOT/target/image"
FUNCTION=forkrail-image
HOME_REGION="${FORKRAIL_HOME_REGION:-us-east-1}"
REGIONS="${FORKRAIL_IMAGE_REGIONS:-us-east-1 us-west-1}"
# Optimized images live this long in the home bucket, counted from when they were made (S3's
# lifecycle can't count from the last read); the next request makes one again.
IMAGE_DAYS=31
IMAGE_RULE_ID=forkrail-optimized-images

log() { echo "==> $*" >&2; }
die() { echo "$*" >&2; exit 1; }
usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }

# Packages for Lambda's Linux on arm64 (sharp's prebuilt libvips for glibc), then the zip; prints
# its path. The AWS SDK isn't packed: Lambda's Node runtime has it. The same packages give the same
# zip (fixed times, sorted names).
build() {
  rm -rf "$OUT/pkg" "$OUT/image.zip" && mkdir -p "$OUT/pkg"
  cp "$SRC/index.mjs" "$SRC/package.json" "$OUT/pkg/"
  (cd "$OUT/pkg" && npm install --omit=dev --os=linux --cpu=arm64 --libc=glibc --no-audit --no-fund --silent >&2)
  [ -d "$OUT/pkg/node_modules/@img/sharp-linux-arm64" ] || die "sharp's Linux arm64 build didn't install"
  # sharp's WebAssembly build is its fallback where there's no native one; Lambda has the native
  # one, and the 9 MB would only slow every cold start.
  rm -rf "$OUT/pkg/node_modules/@img/sharp-wasm32" "$OUT/pkg/node_modules/@emnapi"
  find "$OUT/pkg" -exec touch -t 198001010000 {} +
  (cd "$OUT/pkg" && find . -type f | LC_ALL=C sort | zip -X -q "$OUT/image.zip" -@)
  echo "$OUT/image.zip"
}

# The bucket of `region`, as the CLI names them: the install's own in the home region,
# `<bucket>-<region>` elsewhere.
bucket_of() {
  local account
  account="$(aws sts get-caller-identity --query Account --output text)"
  if [ "$1" = "$HOME_REGION" ]; then echo "forkrail-artifacts-$account"; else echo "forkrail-artifacts-$account-$1"; fi
}

# Upload the zip to `region`'s bucket under its hash, unless it's there; prints the key. Lambda
# takes code from a bucket in its own region only.
upload() {
  local zip="$1" region="$2" bucket sha key
  bucket="$(bucket_of "$region")"
  sha="$(shasum -a 256 "$zip" | cut -c1-16)"
  key="image/forkrail-image-$sha.zip"
  if aws s3api head-object --region "$region" --bucket "$bucket" --key "$key" >/dev/null 2>&1; then
    log "s3://$bucket/$key is there already"
  else
    aws s3 cp --region "$region" "$zip" "s3://$bucket/$key" --only-show-errors
    log "uploaded s3://$bucket/$key ($(wc -c <"$zip" | tr -d ' ') bytes)"
  fi
  echo "$key"
}

# The home bucket's lifecycle rule for optimized images: deleted IMAGE_DAYS after they're made.
# Setting a bucket's lifecycle replaces all its rules, so the others (analytics.sh's Postgres
# backups, say) are read first and kept. With "plan", only says what it would do.
ensure_images_expiry() {
  local mode="$1" bucket out rules wanted
  bucket="$(bucket_of "$HOME_REGION")"
  if out="$(aws s3api get-bucket-lifecycle-configuration --region "$HOME_REGION" --bucket "$bucket" --query Rules --output json 2>&1)"; then
    rules="$out"
  elif grep -q NoSuchLifecycleConfiguration <<<"$out"; then
    rules='[]'
  else
    echo "$out" >&2
    return 1
  fi
  wanted="$(jq -n --arg id "$IMAGE_RULE_ID" --argjson days "$IMAGE_DAYS" \
    '{ID: $id, Status: "Enabled", Filter: {Prefix: "images/"}, Expiration: {Days: $days}}')"
  if jq -e --argjson wanted "$wanted" 'any(.[]; .ID == $wanted.ID and .Status == "Enabled" and .Filter.Prefix == $wanted.Filter.Prefix and .Expiration.Days == $wanted.Expiration.Days)' <<<"$rules" >/dev/null; then
    log "s3://$bucket: the rule deleting images/ after $IMAGE_DAYS days is there already"
    return
  fi
  log "s3://$bucket: rule to delete images/ $IMAGE_DAYS days after they're made, keeping its $(jq '[.[] | select(.ID != "'"$IMAGE_RULE_ID"'")] | length' <<<"$rules") other rule(s): $(jq -c '[.[].ID]' <<<"$rules")"
  [ "$mode" = plan ] && return
  aws s3api put-bucket-lifecycle-configuration --region "$HOME_REGION" --bucket "$bucket" --lifecycle-configuration "$(jq \
    --argjson wanted "$wanted" '{Rules: ([.[] | select(.ID != $wanted.ID)] + [$wanted])}' <<<"$rules")"
}

# A local copy with every package, for tests and local runs, outside the source folder.
dev() {
  mkdir -p "$OUT/dev"
  cp "$SRC/index.mjs" "$SRC/index.test.mjs" "$SRC/package.json" "$OUT/dev/"
  if [ ! -d "$OUT/dev/node_modules/sharp" ] || [ "$SRC/package.json" -nt "$OUT/dev/node_modules" ]; then
    (cd "$OUT/dev" && npm install --no-audit --no-fund --silent >&2)
  fi
}

cmd="${1:-}"
[ -n "$cmd" ] && shift
case "$cmd" in
  # Build the zip only and print its path (deploy/aws/bundle.sh, for `forkrail install`).
  zip) build ;;
  release | plan)
    zip="$(build)"
    store="$(bucket_of "$HOME_REGION")"
    for region in $REGIONS; do
      key="$(upload "$zip" "$region")"
      action=apply
      [ "$cmd" = plan ] && action=plan
      log "$region: $action image with $key, keeping images in s3://$store"
      AWS_REGION="$region" "$ROOT/deploy/aws/stack.sh" "$action" image "CodeKey=$key" "Bucket=$(bucket_of "$region")" \
        "StoreBucket=$store" "StoreRegion=$HOME_REGION" "$@"
    done
    ensure_images_expiry "$cmd"
    ;;
  test)
    dev
    (cd "$OUT/dev" && node --test)
    ;;
  serve)
    dev
    : "${FORKRAIL_IMAGE_DIR:?set FORKRAIL_IMAGE_DIR to the local router FORKRAIL_STATIC_DIR}"
    FORKRAIL_IMAGE_DIR="$FORKRAIL_IMAGE_DIR" exec node "$OUT/dev/index.mjs" serve "${1:-3090}"
    ;;
  try)
    [ $# -ge 3 ] || die "usage: image.sh try KEY W Q [FORMAT]"
    out="$(mktemp)"
    trap 'rm -f "$out"' EXIT
    payload="$(jq -nc --arg key "$1" --argjson w "$2" --argjson q "$3" --arg f "${4:-}" \
      '{source: {key: $key}, width: $w, quality: $q, format: (if $f == "" then null else $f end), minimumCacheTTL: 60, immutable: true,
        store: "images/_try/0000000000000000000000000000000000000000"}')"
    aws lambda invoke --function-name "$FUNCTION" --cli-binary-format raw-in-base64-out --payload "$payload" "$out" >/dev/null
    jq -r 'if .ok then .body // empty else error(.message) end' "$out" | base64 -d >image.out
    jq -c 'del(.body)' "$out"
    ;;
  *) usage ;;
esac
