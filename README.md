# CSV Import & Export

Bring records into Attricat from CSV files and take them out again. People import and export from the Explorer; administrators can also declare repeatable jobs in a blueprint.

- **Import a file** from the Explorer toolbar. New rows create records, and with a matching attribute chosen, existing records are updated.
- **Export selected records** from the Explorer to a file that opens cleanly in a spreadsheet.
- **Schedule imports and exports** per blueprint, with exports running per publication channel.
- **See what went wrong.** Rejected rows are listed in a downloadable report, by row number and reason, and the rest of the file still imports.

Needs Attricat with host API 1.1. It makes no network requests: files stay in Attricat's own storage.

## Install

Install **CSV Import & Export** from **Manage → Extensions → Marketplace**, grant its permissions, and enable it.

| Permission | Why |
| --- | --- |
| `attricat.read`, `attricat.write` | Read and write records. Runs from the Explorer are also limited to what the person who started them may do. |
| `artifacts.read`, `artifacts.write` | Read the file being imported, and store exports and reports. |
| `storage.extension` | Remember each blueprint's import settings. |
| `client.explorer_action`, `client.explorer_bulk_action`, `client.action_dialog`, `client.files.upload`, `client.operations.start`, `client.notification` | Show the actions and the settings dialog, and start runs. |

## Import a file

1. In the Explorer, open a blueprint and choose **Extension actions → Import CSV…**. This needs write access across the workspace, because an import can create records.
2. Pick a `.csv` file. The import runs in the background, in the context you are viewing.
3. Follow it under **Profile → Extension runs**. If rows were rejected, download `rejections.csv` from the run. If the file couldn't be imported at all, for example because of an unknown column, the report says why.

The file's first row names its columns, and each column is named after an **attribute code** (`sku`, not `SKU code`). Case and surrounding spaces are ignored.

```csv
sku,title,qty,price,active
TS-1,"Shirt, linen",5,19.99,true
TS-2,Mug,,9.50,false
```

| Attribute type | In a cell |
| --- | --- |
| Text | Any text. |
| Integer, number | `12`, `-1.5`. In a semicolon file, a decimal comma also works: `12,5`. |
| Boolean | `true` or `false`, in any case. |
| Date, date and time, time | The format the attribute accepts, such as `2026-11-02`. |
| JSON | A JSON value. |

Relationship and file attributes can't be imported; remove those columns.

### Update existing records

By default every row creates a record. To update records instead, choose **Extension actions → CSV import settings…** and pick the attribute that identifies a record, ideally a text attribute with a [unique key](https://docs.attricat.com/reference/blueprint/#unique-keys). Rows whose value matches a record then update it, and other rows create one. If several rows have the same value, the last one wins.

Save the settings again after publishing a new blueprint revision. Until you do, imports stop with a report that says so, rather than creating duplicates.

### What happens to each row

- Empty cells leave a value as it is, so a file can update only some attributes.
- Rows with only empty cells are passed over.
- A row with an invalid value or the wrong number of cells is rejected, and the other rows are still imported.
- Re-running a file is safe with a matching attribute: rows are updated, not duplicated. Without one, each run creates records again.

### File format

UTF-8, with or without a byte order mark. Fields are separated by commas, semicolons or tabs, detected from the first row. Quotes follow RFC 4180, and quoted fields can hold line breaks. Up to 128 columns and 64 KiB per row.

Attricat's upload limit applies (50 MiB by default).

### Large files

Each row is validated, audited and saved on its own, and a run continues in small batches that re-read the file up to their position. Import time therefore grows faster than file size: expect minutes for thousands of rows and consider splitting files of hundreds of thousands. For scale, 5,000 short rows took about 7 minutes on a development server, and a 20 MiB file of 400 long rows about 2 minutes.

If a file is too large to finish, the run stops with a report and the rows before that point stay imported. With a matching attribute, you can safely import the rest from a smaller file.

## Export records

Select up to 50 records in the Explorer and choose **Actions → Export to CSV**. When the run finishes, download `export.csv` from the notification or from **Profile → Extension runs**.

Columns are the selection's attribute codes, in alphabetical order, and values are those of the context you are viewing. The file starts with a byte order mark so spreadsheets read it as UTF-8. It can be edited and imported back.

To export a whole blueprint, use an export job, below.

**Formula protection.** Text starting with `=`, `+`, `-` or `@` could run as a formula in a spreadsheet, so it is written with a leading apostrophe. Numbers aren't changed, and importing removes the apostrophe.

## Jobs in a blueprint

Administrators can declare imports and exports in a blueprint's YAML, so they are versioned with the data model:

```yaml
connector_jobs:
  - code: products_out
    direction: export
    extension_id: attricat-connector-csv
    operation_id: export
    interval_seconds: 86400        # optional; omit to run by hand
    input:
      profile:
        version: 1
        bom: true                  # optional; for spreadsheets
        columns:
          - {header: SKU, attribute: sku, kind: string}
          - {header: Price, attribute: price, kind: number}

  - code: stock_in
    direction: import
    extension_id: attricat-connector-csv
    operation_id: import
    context: default
    input_file_id: <ready file UUID>
    input:
      dry_run: false               # optional; true checks rows without writing
      profile:
        version: 1
        business_key: sku
        unknown_headers: reject    # or ignore
        empty: skip                # or null, or empty_string
        columns:
          - {header: SKU, attribute: sku, kind: string}
          - {header: Qty, attribute: qty, kind: integer, default: 0}
```

- `input_file_id` is a ready workspace file, such as a value of a file attribute. Attricat's file attributes refuse the `.csv` extension today, so upload the feed as `.txt`; its contents are read as CSV all the same.
- `kind` is `string` (the default), `integer`, `number`, `boolean`, `date`, `datetime`, `time` or `json`, and must match the attribute. Attricat fills in the blueprint and context for each run.
- `business_key` must be a writable text column. A job always updates or creates by it. Headers are matched exactly, and the order of `columns` is the order of an export's columns.
- An export creates one run per enabled publication channel, or only for the channel named as `context`, with the records published there and values resolved through the channel's context.
- An export has no selection limit; it reads every published record in batches. Records changed or published while it runs may or may not be included.
- A job's report can't name the reason a row was refused by Attricat, only its row number. Rows refused by the import itself, such as an invalid number, do name it.
- Re-running an unchanged import file updates the same records again. Scheduled runs aren't skipped when the file hasn't changed.

```sh
acli extension connector-job list <blueprint-id>
acli extension connector-job run <job-id> --idempotency-key manual-2026-11-02
acli extension operation get <run-id>
acli extension operation artifacts <run-id>
acli extension operation download <run-id> <artifact-id> --output products.csv
```

See [Blueprint connector jobs](https://docs.attricat.com/extensions/operations/#blueprint-connector-jobs).

Outputs are kept for 30 days.

## Develop

```sh
pnpm install
just check      # formatting, Rust and UI tests, lints
just pack       # dist/attricat-connector-csv-<version>.tar.zst
just validate   # the checks an upload runs; needs an Attricat checkout
just e2e        # every operation against a running development server; see e2e/verify.mjs
```

Building needs Rust with the `wasm32-unknown-unknown` target, `wasm-tools`, `just`, Node and pnpm. `crates/core` holds the CSV logic, `crates/component` the WebAssembly component, and `client` the settings dialog. To release, raise the version in `manifest.json` and both crates, then push a `vX.Y.Z` tag; CI publishes the archive as a GitHub Release, which the marketplace offers.

## License

MIT. See [LICENSE](LICENSE).
