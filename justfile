default: check

check:
  cargo fmt --check
  cargo test --workspace

build: check
  #!/usr/bin/env bash
  set -euo pipefail
  cargo build --release --target wasm32-unknown-unknown -p attricat-csv-component
  mkdir -p dist
  wasm-tools component new target/wasm32-unknown-unknown/release/attricat_csv_component.wasm -o dist/server.wasm
  wasm-tools component wit dist/server.wasm | grep -q 'catalog:host/operations@1.4.0'

pack: build
  #!/usr/bin/env bash
  set -euo pipefail
  mkdir -p dist/package/dist dist/package/assets
  cp manifest.json README.md dist/package/
  cp assets/icon-48.svg dist/package/assets/
  cp dist/server.wasm dist/package/dist/
  (cd dist/package && tar -cf - manifest.json README.md assets dist) | zstd -q -o dist/attricat-connector-csv-0.1.0.tar.zst -f
