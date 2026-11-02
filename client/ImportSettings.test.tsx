import { renderContribution } from '@attricat/extension/testing';
import { expect, it } from 'vitest';
import { ImportSettings } from './ImportSettings';

const context = { context_version: 1, blueprint_id: 'b1', blueprint_version: 3, record_ids: [] };
const revision = {
  attributes: [
    { id: 'sku-3', code: 'sku', name: 'SKU', value_type: 'string' },
    { id: 'qty-3', code: 'qty', name: 'Quantity', value_type: 'integer' },
    { id: 'gen-3', code: 'generated', name: 'Generated', value_type: 'string', readonly: true },
  ],
};
const broker = (entry: unknown) => ({
  'attricat.read': () => revision,
  'storage.get': () => entry,
  'storage.set': () => ({ revision: 2 }),
  'storage.delete': () => null,
  'notify': () => null,
  'dialog.close': () => null,
});

const lastCall = (view: { calls: { method: string; payload: unknown }[] }, method: string) =>
  view.calls.filter((call) => call.method === method).at(-1)?.payload;

it('offers writable string attributes and keeps IDs of earlier revisions', async () => {
  const view = await renderContribution(ImportSettings, {
    context,
    capabilities: ['attricat.read', 'storage.extension'],
    broker: broker({ value: { attribute_code: 'sku', attribute_ids: { 2: 'sku-2' } }, revision: 1 }),
  });
  await view.settle();
  const select = view.getByLabel('Match existing records by');
  expect(select.props.value).toBe('sku');
  expect((select.props.options as { value: string }[]).map((o) => o.value)).toEqual(['-', 'sku']);
  await view.fire(view.getByText('Save'), 'press');
  await view.settle();
  expect(lastCall(view, 'storage.set')).toMatchObject({
    key: 'import-key:b1',
    value: { attribute_code: 'sku', attribute_ids: { 2: 'sku-2', 3: 'sku-3' } },
  });
  expect(view.warnings()).toEqual([]);
});

it('removes the setting when rows should not match records', async () => {
  const view = await renderContribution(ImportSettings, {
    context,
    capabilities: ['attricat.read', 'storage.extension'],
    broker: broker({ value: { attribute_code: 'sku', attribute_ids: { 3: 'sku-3' } }, revision: 1 }),
  });
  await view.settle();
  await view.fire(view.getByLabel('Match existing records by'), 'change', { value: '-' });
  await view.fire(view.getByText('Save'), 'press');
  await view.settle();
  expect(lastCall(view, 'storage.delete')).toEqual({ key: 'import-key:b1' });
});
