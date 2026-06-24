#!/usr/bin/env bash
# Publishes desktop release artifacts to the DEDICATED S3 bucket + invalidates
# CloudFront. Never touches the mobile (ivykids-app-builds) bucket. Uploads
# immutable artifacts first, then latest.json last, so the manifest never points
# at a missing file.
set -euo pipefail

: "${DESKTOP_S3_BUCKET:?set DESKTOP_S3_BUCKET}"
: "${DESKTOP_CLOUDFRONT_DISTRIBUTION_ID:?set DESKTOP_CLOUDFRONT_DISTRIBUTION_ID}"
VERSION="${1:?usage: publish-s3.sh <version>}"

BUNDLE="src-tauri/target/universal-apple-darwin/release/bundle"
DMG=$(ls "$BUNDLE"/dmg/*.dmg)
TARGZ=$(ls "$BUNDLE"/macos/*.app.tar.gz)
SIG="${TARGZ}.sig"

DMG_NAME="Envoy_${VERSION}_universal.dmg"
TARGZ_NAME="Envoy_${VERSION}.app.tar.gz"

echo "Uploading artifacts to s3://${DESKTOP_S3_BUCKET}/ ..."
aws s3 cp "$DMG"    "s3://${DESKTOP_S3_BUCKET}/${DMG_NAME}"        --cache-control "public, max-age=31536000, immutable"
aws s3 cp "$TARGZ"  "s3://${DESKTOP_S3_BUCKET}/${TARGZ_NAME}"     --cache-control "public, max-age=31536000, immutable"
aws s3 cp "$SIG"    "s3://${DESKTOP_S3_BUCKET}/${TARGZ_NAME}.sig" --cache-control "public, max-age=31536000, immutable"

# Build latest.json pointing at the uploaded tar.gz, then upload it LAST.
ARTIFACT_URL="https://downloads.zivon.ai/${TARGZ_NAME}"
LATEST_JSON_OUT=latest.json node scripts/make-latest-json.mjs "$VERSION" "$SIG" "$ARTIFACT_URL" "${RELEASE_NOTES:-Release ${VERSION}}"
aws s3 cp latest.json "s3://${DESKTOP_S3_BUCKET}/latest.json" --cache-control "no-cache"

echo "Invalidating CloudFront /latest.json ..."
aws cloudfront create-invalidation \
  --distribution-id "$DESKTOP_CLOUDFRONT_DISTRIBUTION_ID" \
  --paths "/latest.json"

echo "Published ${VERSION}: https://downloads.zivon.ai/${DMG_NAME}"
