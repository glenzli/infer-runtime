#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output=${1:-"$root/target/apple-image-worker"}
mkdir -p "$(dirname -- "$output")"
xcrun swiftc -parse-as-library -O -target arm64-apple-macos27.0 \
  "$root/workers/apple_image/main.swift" -o "$output"
printf '%s\n' "$output"
