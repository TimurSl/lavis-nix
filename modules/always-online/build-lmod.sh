#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
rm -rf build
mkdir -p build dist
CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -buildvcs=false -ldflags='-s -w -buildid=' -o build/always-online .
chmod 700 build/always-online
cp module.json build/module.json
chmod 600 build/module.json
TZ=UTC touch -t 198001010000 build/always-online build/module.json
rm -f dist/always-online.lmod
(
  cd build
  zip -q -0 -X ../dist/always-online.lmod module.json always-online
)
printf 'built %s\n' "$(realpath dist/always-online.lmod)"
