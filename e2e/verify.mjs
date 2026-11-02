// Verifies a packaged release against a running Attricat development server,
// through its API and web app: sideload, grant, every operation, and cleanup.
//
//   acli --server "$WEB/api" --session-file .acli-session auth login default.local --email … --password-stdin
//   ATTRICAT_WEB_URL="$WEB" ATTRICAT_SESSION_FILE=.acli-session just e2e
//
// It removes any installed copy of this extension first, and again at the end.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { chromium } from '@playwright/test';

const EXTENSION = 'attricat-connector-csv';
const WEB = (process.env.ATTRICAT_WEB_URL ?? '').replace(/\/$/, '');
const SESSION_FILE = process.env.ATTRICAT_SESSION_FILE;
const archivePath = process.argv[2];
if (!WEB || !SESSION_FILE || !archivePath) {
  console.error('Set ATTRICAT_WEB_URL and ATTRICAT_SESSION_FILE, and pass the archive.');
  process.exit(2);
}
const session = JSON.parse(readFileSync(SESSION_FILE, 'utf8'));
const cookie = `attricat_session=${session.session}; attricat_csrf=${session.csrf}`;
const suffix = Date.now().toString(36);
let failures = 0;

const request = async (method, path, init = {}) => {
  const response = await fetch(`${WEB}/api${path}`, {
    method,
    ...init,
    headers: { cookie, 'x-attricat-csrf': session.csrf, ...init.headers },
  });
  const body = Buffer.from(await response.arrayBuffer());
  if (!response.ok) throw new Error(`${method} ${path} → ${response.status} ${body.toString().slice(0, 400)}`);
  return body;
};
const api = async (method, path, json) => {
  const init = json === undefined ? {} : { body: JSON.stringify(json), headers: { 'content-type': 'application/json' } };
  const text = (await request(method, path, init)).toString();
  return text ? JSON.parse(text) : null;
};
// E2E_ONLY limits a run to the steps whose labels match, to retry just those.
const only = process.env.E2E_ONLY && new RegExp(process.env.E2E_ONLY);
const step = async (label, run) => {
  if (only && !only.test(label)) return;
  try {
    await run();
    console.log(`PASS ${label}`);
  } catch (error) {
    failures += 1;
    console.log(`FAIL ${label} — ${error.message}`);
  }
};
const poll = async (label, read, accept, timeout = 180_000) => {
  const deadline = Date.now() + timeout;
  let last;
  while (Date.now() < deadline) {
    last = await read();
    if (accept(last)) return last;
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  throw new Error(`${label} timed out; last value ${JSON.stringify(last)?.slice(0, 400)}`);
};

const code = `csv_e2e_${suffix}`;
const definition = (jobs = '', revision = 1) => `format_version: 1
code: ${code}
name: CSV e2e ${suffix} ${revision}
kind: record
views:
  dropdown_option:
    type: dropdown_option
    fields: [sku]
unique_keys:
  - code: sku
    attributes: [sku]
attributes:
  - code: sku
    value_type: string
  - code: title
    value_type: string
  - code: qty
    value_type: integer
  - code: price
    value_type: number
  - code: active
    value_type: boolean
${jobs}`;

await api('DELETE', `/extensions/${EXTENSION}`).catch(() => {});

let release;
const manifest = JSON.parse(readFileSync('manifest.json', 'utf8'));
await step('sideload, grant and enable', async () => {
  await request('POST', '/extensions/sideload', {
    body: readFileSync(archivePath),
    headers: { 'content-type': 'application/zstd' },
  });
  for (const grant_id of manifest.permissions) {
    await api('POST', `/extensions/${EXTENSION}/grants`, { grant_kind: 'capability', grant_id });
  }
  await api('POST', `/extensions/${EXTENSION}/enable`);
  const runtime = await api('GET', '/extensions/runtime');
  release = runtime.find((item) => item.extension_id === EXTENSION).release_id;
});

const blueprint = await api('POST', '/blueprints', { definition: definition() });
let id = blueprint.blueprint.id;
let version = blueprint.blueprint.version;
await api('POST', `/blueprints/${id}/versions/${version}/publish`);

const run = (runId) => api('GET', `/extension-runs/${runId}`);
const finished = (r) => ['completed', 'failed', 'cancelled'].includes(r.status);
const waitRun = (runId, timeout) => poll(`run ${runId}`, () => run(runId), finished, timeout);
const download = async (runId, name) => {
  const detail = await run(runId);
  const artifact = detail.artifacts.find((a) => a.name === name);
  assert(artifact, `${name} not in ${JSON.stringify(detail).slice(0, 400)}`);
  return request('GET', `/extension-runs/${runId}/artifacts/${artifact.id}/download`);
};
let uploads = 0;
const importFile = async (text, name = 'input.csv') => {
  const form = new FormData();
  form.append('file', new Blob([text]), name);
  const file = JSON.parse(
    (await request('POST', `/extensions/${EXTENSION}/import-csv/input-uploads`, { body: form })).toString(),
  );
  uploads += 1;
  const started = await api('POST', `/extensions/${EXTENSION}/import-csv/operations`, {
    release_id: release,
    operation_id: 'import-file',
    input: {},
    idempotency_key: `import-${suffix}-${uploads}`,
    input_file_id: file.file.id,
    selection: { blueprint_id: id, blueprint_version: version, context_id: null, record_ids: [] },
  });
  return started.run_id;
};

let firstRun;
await step('Import CSV… creates records and reports rejected rows', async () => {
  firstRun = await importFile(
    '﻿sku,Title,qty,price,active\n' +
      'SKU-1,"First, ""quoted""",5,-1.5,TRUE\n' +
      'SKU-2,=SUM(A1),nope,2,false\n' +
      'SKU-3,too,many,fields,here,x\n' +
      ',,,,\n' +
      'SKU-4,Fourth,,,\n',
  );
  const done = await waitRun(firstRun);
  assert.equal(done.status, 'completed', JSON.stringify(done));
  assert.deepEqual(done.progress.outcome, { succeeded: 2, failed: 2, skipped: 0 }, JSON.stringify(done.progress));
  const report = (await download(firstRun, 'rejections.csv')).toString();
  assert.match(report, /^row,error\n3,qty: invalid integer\n4,"expected 5 fields, found 6"\n$/);
});

await step('a column no attribute fits stops the import and the report says why', async () => {
  const runId = await importFile('sku,nonsense\nSKU-9,x\n');
  const done = await waitRun(runId);
  assert.equal(done.status, 'completed', JSON.stringify(done));
  assert.deepEqual(done.progress.outcome, { succeeded: 0, failed: 1, skipped: 0 });
  assert.match((await download(runId, 'rejections.csv')).toString(), /nonsense/);
});

const page = await (async () => {
  const browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1400, height: 1000 } });
  const { hostname } = new URL(WEB);
  await context.addCookies(
    ['session', 'csrf'].map((name) => ({ name: `attricat_${name}`, value: session[name], domain: hostname, path: '/' })),
  );
  const page = await context.newPage();
  page.browser_ = browser;
  return page;
})();

const chooseKey = async () => {
  await page.goto(`${WEB}/?blueprint=${code}&version=${version}`);
  await page.getByRole('button', { name: 'Extension actions', exact: true }).click();
  await page.getByRole('menuitem', { name: 'CSV import settings…' }).click();
  const dialog = page.getByRole('dialog');
  const select = dialog.getByLabel('Match existing records by');
  await select.waitFor({ timeout: 60_000 });
  await poll('attributes', () => select.isEnabled(), Boolean);
  await select.click();
  await page.getByRole('option', { name: 'sku', exact: true }).click();
  await dialog.getByRole('button', { name: 'Save' }).click();
  await dialog.waitFor({ state: 'hidden', timeout: 15_000 });
};
await step('the settings dialog chooses the key attribute', chooseKey);

await step('with a key, Import CSV… updates matches, keeps empty cells and creates the rest', async () => {
  const done = await waitRun(await importFile('sku;title;qty;price\nSKU-1;Renamed;;12,5\nSKU-5;Fifth;1;\n'));
  assert.equal(done.status, 'completed', JSON.stringify(done));
  assert.deepEqual(done.progress.outcome, { succeeded: 2, failed: 0, skipped: 0 }, JSON.stringify(done.progress));
});

let recordIds = [];
await step('Export to CSV writes the selected records for a spreadsheet', async () => {
  const found = await api('POST', '/v1/records/search', { blueprint: { code } });
  const items = found.items ?? found.records ?? found.results;
  recordIds = items.map((item) => item.id ?? item.record?.id);
  assert.equal(recordIds.length, 3, JSON.stringify(items).slice(0, 300));
  const started = await api('POST', `/extensions/${EXTENSION}/export-csv/operations`, {
    release_id: release,
    operation_id: 'export-selection',
    input: {},
    idempotency_key: `export-${suffix}`,
    selection: { blueprint_id: id, blueprint_version: version, context_id: null, record_ids: recordIds },
  });
  const done = await waitRun(started.run_id);
  assert.equal(done.status, 'completed', JSON.stringify(done));
  assert.deepEqual(done.progress.outcome, { succeeded: 3, failed: 0, skipped: 0 });
  const bytes = await download(started.run_id, 'export.csv');
  const csv = bytes.toString();
  console.log(csv.replace(/^/gm, '  | '));
  assert.equal(csv.charCodeAt(0), 0xfeff, 'starts with a byte order mark');
  const [header, ...rows] = csv.slice(1).trimEnd().split('\n');
  assert.equal(header, 'active,price,qty,sku,title');
  assert(rows.includes('true,12.5,5,SKU-1,Renamed'), rows.join('|'));
  assert(rows.includes(',,1,SKU-5,Fifth'), rows.join('|'));
});

await step('an exported file imports back without changing anything', async () => {
  const done = await waitRun(await importFile('active,price,qty,sku,title\ntrue,-3,5,SKU-1,"\'=SUM(A1)"\n'));
  assert.equal(done.status, 'completed', JSON.stringify(done));
  assert.deepEqual(done.progress.outcome, { succeeded: 1, failed: 0, skipped: 0 });
  const started = await api('POST', `/extensions/${EXTENSION}/export-csv/operations`, {
    release_id: release,
    operation_id: 'export-selection',
    input: {},
    idempotency_key: `export-back-${suffix}`,
    selection: { blueprint_id: id, blueprint_version: version, context_id: null, record_ids: recordIds },
  });
  await waitRun(started.run_id);
  const csv = (await download(started.run_id, 'export.csv')).toString();
  assert(csv.includes("true,-3.0,5,SKU-1,'=SUM(A1)"), csv);
});

await step('a new blueprint revision asks for the settings again', async () => {
  const next = await api('POST', `/blueprints/${id}/versions`, { definition: definition('', 2) });
  await api('POST', `/blueprints/${id}/versions/${next.blueprint.version}/publish`);
  version = next.blueprint.version;
  const runId = await importFile('sku,title\nSKU-1,x\n');
  const done = await waitRun(runId);
  assert.deepEqual(done.progress.outcome, { succeeded: 0, failed: 1, skipped: 0 });
  assert.match((await download(runId, 'rejections.csv')).toString(), /new revision/);
  await chooseKey();
});

await step('connector jobs: manual import and per-channel export', async () => {
  // A job's input is a ready workspace file, such as a value of a file attribute.
  const holderCode = `csv_e2e_files_${suffix}`;
  const holder = await api('POST', '/blueprints', {
    definition: `format_version: 1
code: ${holderCode}
name: CSV e2e files ${suffix}
kind: record
views:
  dropdown_option:
    type: dropdown_option
    fields: [name]
attributes:
  - code: name
    value_type: string
  - code: feed
    value_type: file
    allowed_extensions: [txt]
`,
  });
  await api('POST', `/blueprints/${holder.blueprint.id}/versions/${holder.blueprint.version}/publish`);
  const context = (await api('GET', '/contexts')).items?.find?.((c) => c.code === 'default');
  const holderRecord = await api('POST', '/v1/records', {
    blueprint: { code: holderCode },
    values: [{ kind: 'scalar', attribute_code: 'name', context_id: context?.id, value: 'feed' }],
  });
  const upload = new FormData();
  upload.append('file', new Blob(['SKU,Qty\nJOB-1,7\nJOB-2,oops\nJOB-4,-2.5x\n,1\nJOB-3,\n'], { type: 'text/plain' }), 'job.txt');
  const stored = JSON.parse(
    (await request('POST', `/records/${holderRecord.id}/file-attributes/feed/uploads`, { body: upload })).toString(),
  );
  const fileId = stored.files[0].id;
  await poll('file ready', () => api('GET', `/files/${fileId}`), (f) => f.status === 'ready');
  const jobs = `connector_jobs:
  - code: csv_import
    direction: import
    extension_id: ${EXTENSION}
    operation_id: import
    context: default
    input_file_id: '${fileId}'
    input:
      profile:
        version: 1
        business_key: sku
        columns:
          - {header: SKU, attribute: sku, kind: string}
          - {header: Qty, attribute: qty, kind: integer}
  - code: csv_export
    direction: export
    extension_id: ${EXTENSION}
    operation_id: export
    input:
      profile:
        version: 1
        bom: true
        columns:
          - {header: SKU, attribute: sku, kind: string}
          - {header: Qty, attribute: qty, kind: integer}
`;
  const next = await api('POST', `/blueprints/${id}/versions`, { definition: definition(jobs, 3) });
  await api('POST', `/blueprints/${id}/versions/${next.blueprint.version}/publish`);
  version = next.blueprint.version;
  const listed = await api('GET', `/blueprints/${id}/connector-jobs`);
  const job = (name) => (listed.items ?? listed).find((item) => item.code === name);
  const importRuns = await api('POST', `/blueprint-connector-jobs/${job('csv_import').id}/run`, { idempotency_key: `i-${suffix}` });
  const importRun = await poll(
    'import job',
    () => api('GET', `/extension-operation-runs/${importRuns.run_ids[0]}`),
    finished,
  );
  assert.equal(importRun.status, 'completed', JSON.stringify(importRun));
  assert.deepEqual(importRun.progress.outcome, { succeeded: 2, failed: 3, skipped: 0 }, JSON.stringify(importRun.progress));
  const channels = await api('GET', '/publication-channels');
  console.log(`  ${(channels.items ?? channels).length} channels`);
  const exportRuns = await api('POST', `/blueprint-connector-jobs/${job('csv_export').id}/run`, { idempotency_key: `e-${suffix}` });
  for (const runId of exportRuns.run_ids) {
    const done = await poll('export job', () => api('GET', `/extension-operation-runs/${runId}`), finished);
    assert.equal(done.status, 'completed', JSON.stringify(done));
    const items = await api('GET', `/extension-operation-runs/${runId}/artifacts`);
    const csv = await request('GET', `/extension-operation-runs/${runId}/artifacts/${items[0].id}/download`);
    assert.equal(csv.toString(), '\uFEFFSKU,Qty\n', 'nothing is published to the channel yet');
  }
});

const notQuarantined = async () => {
  const { installation } = await api('GET', `/extensions/${EXTENSION}`);
  assert.equal(installation.state, 'enabled');
};

await step('a file of 5,000 rows imports within the host limits', async () => {
  await chooseKey();
  const rows = Array.from({ length: 5_000 }, (_, n) => `BULK-${n},Bulk title ${n},${n}\n`).join('');
  const before = Date.now();
  const done = await waitRun(await importFile('sku,title,qty\n' + rows), 900_000);
  console.log(`  ${((Date.now() - before) / 1000).toFixed(0)} s`);
  assert.equal(done.status, 'completed', JSON.stringify(done));
  assert.deepEqual(done.progress.outcome, { succeeded: 5_000, failed: 0, skipped: 0 });
  await notQuarantined();
});

await step('a file of 20 MiB finishes or stops with a reason, and never quarantines', async () => {
  const filler = 'x'.repeat(50_000);
  const rows = Array.from({ length: 400 }, (_, n) => `BIG-${n},${filler}\n`).join('');
  const before = Date.now();
  const runId = await importFile('sku,title\n' + rows);
  const done = await waitRun(runId, 1_500_000);
  console.log(`  ${((Date.now() - before) / 1000).toFixed(0)} s, ${JSON.stringify(done.progress.outcome)}`);
  assert.equal(done.status, 'completed', JSON.stringify(done));
  await notQuarantined();
});

await page.browser_.close();
await api('DELETE', `/extensions/${EXTENSION}`).catch((error) => console.log(`cleanup: ${error.message}`));

if (failures) {
  console.error(`${failures} check(s) failed`);
  process.exit(1);
}
console.log('All checks passed');
