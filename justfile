# Development and release tasks for attricat-connector-csv.

extension_id := "attricat-connector-csv"
version := `node -p "require('./manifest.json').version"`
archive := "dist" / extension_id + "-" + version + ".tar.zst"
core_dir := "../attricat/main"

default: check

check:
  cargo fmt --check
  cargo test --workspace --exclude attricat-csv-component
  cargo clippy --workspace --exclude attricat-csv-component --all-targets -- -D warnings
  pnpm typecheck
  pnpm test
  just versions

# The manifest, crates and a release tag must name one version.
versions:
  #!/usr/bin/env bash
  set -euo pipefail
  for file in crates/core/Cargo.toml crates/component/Cargo.toml; do
    grep -q '^version = "{{version}}"$' "$file" || { echo "$file is not at version {{version}}" >&2; exit 1; }
  done

# wasm32-wasip2 would import ambient WASI interfaces, which Attricat never links.
build-server:
  #!/usr/bin/env bash
  set -euo pipefail
  cargo build --release --target wasm32-unknown-unknown -p attricat-csv-component
  mkdir -p dist
  wasm-tools component new target/wasm32-unknown-unknown/release/attricat_csv_component.wasm -o dist/server.wasm
  # The component must use only the attricat:host@1.1.0 ABI.
  wasm-tools component wit dist/server.wasm | grep -q 'export attricat:host/deferrable-operations@1.1.0'
  if wasm-tools component wit dist/server.wasm | grep -E '^\s*(import|export) ' | grep -v 'attricat:host/.*@1\.1\.0'; then
    echo "server.wasm uses something other than attricat:host@1.1.0" >&2; exit 1
  fi

# The WIT must stay a verbatim copy of the SDK's released snapshot.
check-wit sdk="../attricat-extension-sdk":
  diff {{sdk}}/wit/attricat-host-1.1.0.wit crates/component/wit/attricat-extension.wit

build: build-server
  pnpm build

# The archive Attricat installs: the manifest, README, icon and declared artifacts.
pack: check build
  pnpm exec attricat-pack
  @echo "Created {{archive}}"

# The checks an upload runs, without a server. Needs an Attricat checkout.
validate: pack
  cd {{core_dir}} && cargo run -q -p acli -- extension validate {{justfile_directory()}}/{{archive}}

# Runs the packaged component in the real host against Attricat's test PostgreSQL.
host-e2e: pack
  cd {{core_dir}} && ATTRICAT_CONNECTOR_CSV_ARCHIVE={{justfile_directory()}}/{{archive}} cargo test -p api --test integration extensions::packaged_csv_connector_exports_through_the_real_host -- --nocapture
  cd {{core_dir}} && ATTRICAT_CONNECTOR_CSV_ARCHIVE={{justfile_directory()}}/{{archive}} cargo test -p api --test integration blueprint_connector_jobs::scoped_export_fans_out_to_channels_via_existing_task_queue -- --nocapture

# Sideloads into a running Attricat development server and drives every operation.
# Requires ATTRICAT_WEB_URL and ATTRICAT_SESSION_FILE; see e2e/verify.mjs.
e2e:
  node e2e/verify.mjs {{archive}}

clean:
  rm -rf dist target node_modules
