# Attricat CSV connector (Rust)

Packaged `catalog:host@1.4.0` operations for file or allowlisted HTTPS CSV import, Catalog CSV export, and optional HTTPS output delivery. This repository includes a local immutable profile registry/request generator. **The published host WIT is copied from `attricat/main/crates/extension-runtime/wit-connectors/catalog-extension.wit`; keep it byte-for-byte in sync on host upgrades.** There is no ambient network, database, S3, or filesystem access in the component.

## Build and verify

```sh
just check
just pack      # requires wasm32-unknown-unknown, wasm-tools, zstd
just host-e2e  # requires migrated Attricat test PostgreSQL; real packaged component/host worker
```

`just host-e2e` packages, installs, grants and enables the actual component via the host integration test; exercises a multi-lease 17-row import and paged export using the real runtime. This test uses a fake object store, not an S3 deployment or the external transfer path. An additional **real deployment E2E for this connector's HTTP, schedule, grants and cancellation paths is still needed**; host transfer tests exercise another component. Do not mistake `just check` for end-to-end validation.

## Grants and installation

Side-load `dist/attricat-connector-csv-0.1.0.tar.zst` via the Attricat extension admin UI or `acli extension sideload --file ...`. Grant `catalog.read`, `catalog.write`, `artifacts.read`, `artifacts.write`, then enable. File-only operations do not need `network.request`. For HTTPS transfers, **before packing**, replace the example `csv.example.org` patterns in `manifest.json` with exact production source and destination HTTPS path prefixes. Review each method and `max_transfer_bytes`. Grant the optional `network.request` capability and the required individual `csv-source`/`csv-target` host permissions. `idempotent_delivery: true` permits POST only when the target actually honors the `Idempotency-Key` header; otherwise remove POST and use PUT. Secrets are host-managed *names* used only in `secret_headers`, never secret values in profiles or operation input. URLs must be query-free HTTPS with no userinfo. The host enforces DNS/IP/TLS, grants, quotas, and no redirects at every transfer call. An upgrade resets grants.

## Profiles and on-demand runs

The CLI stores local, immutable, version-addressed profile snapshots. It refuses to overwrite a version. It does not persist credentials or make requests. Store this directory under operator access control; version IDs are scoped to the operator's directory and not a host-wide shared profile database. Every generated request includes the full validated profile snapshot, so an in-flight run or scheduled occurrence never resolves a mutable local profile again. The host independently validates Catalog schema, release, context and grants on execution.

```sh
cargo run -q -p attricat-csv-cli -- save ./profiles import customers import-profile.json
cargo run -q -p attricat-csv-cli -- request ./profiles import customers 1 import-2026-01 <ready-file-uuid> > run.json
# Or generate a schedule with a 3600 second interval:
cargo run -q -p attricat-csv-cli -- request ./profiles import customers 1 ignored <ready-file-uuid> 3600 > schedule.json
# Use the appropriate authenticated Catalog API client to POST run.json to
# /extensions/attricat-connector-csv/operations or schedule.json to
# /extensions/attricat-connector-csv/operation-schedules.
```

Import profile:

```json
{"version":1,"blueprint_id":"<uuid>","blueprint_version":1,"context_id":"<uuid>","business_key":"sku","columns":[{"header":"SKU","attribute":"sku","kind":"string"},{"header":"Qty","attribute":"qty","kind":"integer","default":0}],"unknown_headers":"reject","empty":"null"}
```

Export profile has the same version, blueprint, context and ordered columns but no business key. `request` accepts `[file-id|-] [interval-seconds|-] [endpoint.json|-]`; for HTTPS input pass `- - source.json` and for export delivery pass `- - target.json`. For dry-run add `"dry_run":true` to the generated import request's `input` (not to the profile). Source endpoint JSON is `{"host_permission_id":"csv-source","url":"https://csv.example.org/import/data.csv","secret_headers":[{"secret":"source-token","header":"Authorization","prefix":"Bearer "}]}`. Target endpoint JSON additionally needs `"method":"PUT"` or `"POST"` and a matching `csv-target` permission. Do not combine a file ID with an HTTP source.

The host returns run/schedule IDs promptly. Inspect `/extension-operation-runs`, `/extension-operation-runs/{id}`, `/extension-operation-runs/{id}/artifacts`, `/extension-operation-runs/{id}/deliveries` and the authorized `/extension-operation-runs/{id}/artifacts/{artifact_id}/download` endpoint. Update/disable schedules with `PATCH /extension-operation-schedules/{id}`. Host intervals are 60–2592000 seconds; missed and overlapping occurrences are skipped. A scheduled file repeats the same upsert values; it is **not** a source-version skip. HTTP schedules fetch each occurrence anew. Use a source identity check outside this connector if repeated versions must be skipped. Host retains completed outputs for 30 days. Cancellation, revocation, disable and quarantine block subsequent host calls.

## CSV/transfer policies

UTF-8 with optional BOM; RFC 4180 comma/double-quote escaping, LF or CRLF, embedded newlines. Max 128 distinct header-to-attribute mappings, 64 KiB logical input record, 16 upserts per batch and 64 KiB host JSON. Headers are case-sensitive; duplicate, empty, unknown (by default), or missing headers reject the run. Invalid cells, null/empty keys and duplicate string business keys within the same file receive row-numbered rejection reports (`rejections.ndjson`), not raw row copies. Business key must be a writable **string** attribute. Empty fields are null or empty strings (for string columns only) unless a typed default exists; integers are i64, numbers finite f64, booleans exactly `true`/`false`. Structural CSV errors abort the run. Catalog upserts use host validated/audited/idempotent batch keys; a replay cannot duplicate mutations.

Ready files are opened as `source`; remote inputs are fetched in up to 16 MiB host-managed ranges with a stable per-offset transfer key. Ranges after the first require a strong ETag; absence/change aborts a large transfer. The parser is re-opened from the beginning for every durable batch and skips committed rows, avoiding unsafe seek offsets inside quoted CSV at the cost of quadratic reads for large files. `dry_run` performs mapping and rejection counts without mutations; it does not show valid row previews. A source without an ETag can be parsed only if smaller than one range; the host reuses the already fetched run-bound artifact on retries. Independent runs may observe different versions, so do not schedule mutable endpoints without an external source-version policy.

Export pages are cursor-bound to a host database-clock high-water mark; values use as-of history, but concurrent creates, publication changes and migrations can affect membership. Freeze source mutations if an externally delivered export must be immutable. Output is UTF-8 LF CSV, in host page order with the profile's headers and CSV quoting; null becomes empty. Cells starting after whitespace with `=`, `+`, `-`, `@`, tab or CR get a protective apostrophe (a deliberate value change). Adaptive page sizing reduces output batches to 64 KiB; one oversized row fails explicitly. The host checksums/finalizes `export.csv` before optional delivery. Delivery has a stable key and independent host history: `uncertain` (including timeout/crash) is **not retried**, even on a finish replay. Inspect `/deliveries`; do not assume a completed run proves successful HTTP delivery.

## Remaining hardening

A production signoff still needs a side-loaded S3-backed host E2E for this actual connector including authenticated allowlisted HTTP source and destination, a scheduled occurrence, restart during a multi-range import, invalid/duplicate rows and dry-run report, cancellation, grant loss, oversized input/output, SSRF/redirect denial and uncertain delivery. Profile registry is currently local/operator-managed rather than a host-authorized workspace API. Imports of large files rescan from the start each batch and should move to a durable bounded CSV parser checkpoint when the host provides one. HTTP source versions are not automatically deduplicated between separate scheduled runs.
