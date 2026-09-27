# Attricat CSV connector (Rust)

The extension is the **CSV source/destination adapter** for Attricat's blueprint connector jobs. **Attricat core owns blueprint configuration, job publication, run scheduling/fan-out, authorization, scope, checkpoint/lease orchestration, Catalog mutation validation/audit, and artifact storage/download.** The v1.4 host contract still requires the extension to invoke mediated Catalog `schema`, `page` and `upsert-batch` calls, parse/map CSV, and invoke mediated artifact/HTTPS transfer calls. No ambient I/O, SQL, S3 keys, sockets, or connector-specific CLI are used. The WIT in `crates/component/wit` is copied from Attricat's published `wit-connectors/catalog-extension.wit`.

## Build, install, test

```sh
just check
just pack      # requires wasm32-unknown-unknown, wasm-tools, zstd
just host-e2e  # requires migrated Attricat test PostgreSQL
```

`just host-e2e` exercises the packaged component in the host runtime, including file import, paged export, worker restart, and **scoped blueprint connector jobs** (export fan-out per enabled publication channel and a scoped import). These integration tests use a fake object store, not an S3 deployment. The HTTPS, actual interval occurrence, uncertain delivery, grant loss, and cancellation paths of this connector still need deployment-level E2E; see [issue #3](https://github.com/attricat/attricat-connector-csv/issues/3).

Side-load `dist/attricat-connector-csv-0.1.0.tar.zst` with the Attricat extension admin UI or `acli extension sideload --file ...`. Grant `catalog.read`, `catalog.write`, `artifacts.read`, `artifacts.write`, then enable. Before packing for HTTPS use, replace example `csv.example.org` host patterns in `manifest.json` with the approved source/target HTTPS path prefixes and review methods, limits, and `idempotent_delivery`. Grant optional `network.request` and the specific `csv-source` and/or `csv-target` host permission. File-only use does not require network grants. Only allow POST when the target documents `Idempotency-Key` support; otherwise remove POST and use PUT. Named secret header **references**, never secret values, belong in connector input.

## Configure in the entity blueprint

Declare jobs in the *entity blueprint TOML* and publish the revision. `version` identifies the profile snapshot; column order determines export column order. Attricat overwrites the profile's blueprint ID, blueprint version, and context on each run, so they should **not** be hardcoded in the declaration. Publish rejects jobs without a compatible enabled extension, declared operation, valid input, ready file, or valid import context. Changes to job input are published as a new blueprint revision; the host stores run-specific input snapshots and reconciles jobs by stable `code`.

```toml
[[connector_jobs]]
code = "csv_import"
direction = "import"
extension_id = "attricat-connector-csv"
operation_id = "import"
context = "en_GB"                 # workspace context code, required for imports
input_file_id = "<ready-file-uuid>" # omit for an HTTPS source
interval_seconds = 3600          # optional; omit for manual-only
input = { profile = { version = 1, business_key = "sku", columns = [{ header = "SKU", attribute = "sku", kind = "string" }, { header = "Qty", attribute = "qty", kind = "integer", default = 0 }] } }

[[connector_jobs]]
code = "csv_export"
direction = "export"
extension_id = "attricat-connector-csv"
operation_id = "export"
input = { profile = { version = 1, columns = [{ header = "SKU", attribute = "sku", kind = "string" }] } }
```

For a file import, use `input_file_id`; for HTTP, omit it and include an endpoint in input:

```toml
input = { profile = { version = 1, business_key = "sku", columns = [{ header = "SKU", attribute = "sku", kind = "string" }] }, source = { host_permission_id = "csv-source", url = "https://csv.example.org/import/data.csv", secret_headers = [{ secret = "source-token", header = "Authorization", prefix = "Bearer " }] } }
```

For HTTP export, add `destination = { host_permission_id = "csv-target", url = "https://csv.example.org/export/data.csv", method = "PUT" }` to export `input`. `dry_run = true` in import `input` validates rows but does not write. Do not configure both a ready file and an HTTPS source. Use `enabled = false` to pause a job; new blueprint revisions disable removed jobs while retaining their history. Core enqueues interval occurrences (60–2592000 seconds) and skips overlaps/missed intervals. HTTP and file sources are **not** deduplicated across distinct occurrences: idempotent upsert avoids duplicate entities but does not skip work.

Inspect jobs via `GET /blueprints/{blueprint_id}/connector-jobs`. Manually start with `POST /blueprint-connector-jobs/{id}/run` and `{"idempotency_key":"manual-1"}`; this returns run IDs. Exports run once **per enabled publication channel**, including only entities currently published for that channel. Imports run once in the declared workspace context. Inspect `/extension-operation-runs/{id}`, `/extension-operation-runs/{id}/artifacts`, `/extension-operation-runs/{id}/deliveries` and download via `/extension-operation-runs/{id}/artifacts/{artifact_id}/download`. The host owns authorization and retention; outputs expire after 30 days. Host-managed schedules and generic unscoped operations also exist, but **blueprint connector jobs** are the supported operator configuration path for this extension.

## Data and failure policy

Import: UTF-8 RFC 4180 CSV with optional BOM, comma, double-quote escaping, LF/CRLF and quoted newlines; max 128 unique mapped columns, 64 KiB logical input record, up to 16 rows per Catalog batch. Headers are case-sensitive; duplicate, empty, missing or (by default) unknown headers reject the run. Invalid values, empty or repeated business keys produce row-numbered `rejections.ndjson` without raw row data; malformed CSV aborts the run. Business key must be a writable string attribute. Empty fields are null or empty strings (string columns only) unless a typed default exists; integers are i64, numbers finite f64, booleans exactly `true`/`false`. Host validates workspace/blueprint/context on each mediated upsert; retries use the run's stable batch key. Input is reopened and rescanned from the start at each batch, so large inputs are currently quadratic. HTTP inputs use pinned run-bound 16 MiB ranges with strong ETag required after the first range.

Export: host-filtered/paged, context-specific Catalog values are mapped to ordered columns and written as LF-terminated, UTF-8 CSV. Null becomes empty; leading spreadsheet formula sigils (`=`, `+`, `-`, `@`, including after whitespace) get an apostrophe. Output chunks adapt to the 64 KiB host bound; a single oversized row fails. Host finalizes a checksummed, immutable `export.csv`; optional HTTPS delivery has an independent recorded `succeeded`, `failed`, or `uncertain` outcome. Never assume a completed run means delivery succeeded. An uncertain delivery is never blindly resent. Page cursors carry a high-water mark but do not freeze membership across concurrent creates or publication changes; freeze changes when immutable external exports are required.

**Remaining before production signoff:** real S3-backed side-loaded E2E for this connector's HTTPS, scheduled occurrence, restart, invalid rows, cancellation, grant loss, source-version handling, oversized transfers and uncertain delivery; see issue #3. Attricat's host behavior is tracked in attricat/attricat#276.
