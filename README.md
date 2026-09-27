# Attricat CSV connector (Rust)

**Status: partial implementation for [connector #1](https://github.com/attricat/attricat-connector-csv/issues/1). Not installable against current Attricat.** This package targets a *proposed* `catalog:host@1.4.0` operation WIT in `crates/component/wit/catalog-extension.wit`, pending the published ABI from [host #276](https://github.com/attricat/attricat/issues/276). Do not advertise it as host-compatible, or schedule production runs, until that WIT is replaced by the published contract and a real-host integration test passes. No HTTP transfer or scheduler is implemented in this repository yet. There is no ambient file/network access.

## Build

`just check` runs Rust formatting and tests. `just pack` builds a `wasm32-unknown-unknown` WASM component (requires `wasm-tools` and `zstd`) and packages `dist/attricat-connector-csv-0.1.0.tar.zst`. The archive contains only manifest, icon, README and component. **Packaging does not prove host compatibility.**

## Proposed host contract

`manifest.json` declares `catalog.read`, `catalog.write`, `artifacts.read`, `artifacts.write` as required grants; file-only operation needs no network permission. The WIT imports host-owned file streams, blueprint/context/revision schema lookup, snapshot-paged Catalog reads, replay-idempotent Catalog upsert batches and restart-safe, batch-keyed output staging. The host must authorize every call, pin/recheck workspace/release/grants, enforce body, byte and time quotas, keep staged output across restarts, and checksum the finalized immutable artifact. The operations are async and release-pinned; the host owns enqueueing, lease/checkpoint commits, cancellation, download permissions and cleanup. WIT here is a **proposal**, not a claim that #276 shipped it. Schema response is `{"attributes":[{"id":"sku","kind":"string"}]}`; page response is `{"rows":[{"sku":"A"}],"next_cursor":null}`. Pages must pin a stable snapshot across invocations. Upserts must atomically deduplicate `(run_id,batch_key)` and use the normal validated Catalog mutation/audit/outbox path. `append-output` must atomically deduplicate the same batch key and `finalize-output` must checksum before publishing.

## Input contract (once host ABI is available)

The caller submits `POST /extensions/{extension_id}/operations` with `operation_id` `import` or `export`, `input` holding a **fully snapshotted** profile, and for import `source_reference: {"input_file_id":"<ready-workspace-file-uuid>"}`. The host must copy the profile input into the durable run/schedule occurrence snapshot, not resolve a mutable profile again on retries. `import` also accepts `dry_run: true`. Example import input:

```json
{"profile":{"version":1,"blueprint_id":"<uuid>","blueprint_version":1,"context_id":"<uuid>","business_key":"sku","columns":[{"header":"SKU","attribute":"sku","kind":"string"},{"header":"Qty","attribute":"qty","kind":"integer","default":0}],"unknown_headers":"reject","empty":"null"},"dry_run":true}
```

Export input:

```json
{"profile":{"version":1,"blueprint_id":"<uuid>","blueprint_version":1,"context_id":"<uuid>","columns":[{"header":"SKU","attribute":"sku","kind":"string"}]}}
```

Profiles are versioned in input, but there is **no operator profile registry or CLI setup command yet**. Submit exact profile JSON, keep your own version history, and do not put secrets in it. On the future host, sideload the archive, grant all four permissions, enable, and use the operation run endpoints to inspect run progress and download `export.csv` / `rejections.ndjson`. Grant loss must block the next host call. Import reopens the host-pinned `source` file every batch and skips checkpointed records; this uses bounded memory but time grows with file length. Export requests pages of at most 16 rows and appends to host staging; outputs must not be published on failure/cancellation.

## CSV policy

UTF-8 with optional leading BOM; comma delimiter, double-quoted fields and doubled quote escapes, CRLF or LF, quoted newlines. Header names are case-sensitive, duplicate/empty/missing headers reject the run; unknown headers reject by default or may be ignored. Max 128 mapped columns, 64 KiB per data record and 16 records per batch. Missing fields, malformed CSV, or non-UTF-8 headers fail the run; invalid typed values, non-UTF-8 data cells, empty business keys and repeated business keys in the same file are row-numbered rejections. Empty fields become JSON null by default or empty string, unless a typed default is provided; integers are signed 64-bit, numbers finite float64, booleans exactly `true` or `false`. Only declared columns are mapped; attributes must match host schema type. Import upserts by the mapped business key and requires host idempotent batch commit. Dry-run validates rows and produces rejection counts without mutations. Rejection reports contain row number and error text, not full input rows.

Export follows the host's stable page order and emits LF-terminated UTF-8 RFC 4180 CSV with a header. Null becomes empty; other JSON scalars are stringified. Cells starting (after whitespace) with `=`, `+`, `-`, `@`, tab or CR get an apostrophe prefix to prevent spreadsheet formula execution; this alters exported text intentionally. Consumers must preserve it. CSV output is finalized by the host with checksum validation.

## Remaining before issue #1 can close

- Replace proposed WIT with host #276's actual released WIT, add compatible manifest version and a real-host sideload/operation E2E (including audit, restart and grant loss).
- Operator-owned immutable profile versions and management CLI/API; schema lookup must check defaults, permissions and business-key uniqueness.
- Allowlisted, mediated HTTP(S) source/destination transfer with named secret references, immutable source identity and explicit uncertain-delivery status. Never blindly retry timed-out POSTs.
- Host-backed scheduling, source deduplication/overlap rules, artifact/rejection download checks, cancellation and oversized-transfer coverage.
