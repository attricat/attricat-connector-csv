/* Chooses the attribute that matches rows of an imported file to existing records.
 * The server component reads the choice from extension storage. */
import { useAttricat, useLocale, useOutletContext, type ActionContext } from '@attricat/extension';
import { Button, Select, Stack, Text } from '@attricat/ui-kit';
import { useEffect, useState } from 'preact/hooks';

type Attribute = { id: string; code: string; value_type: string; readonly?: boolean };
type Revision = { attributes: Attribute[] };
type ImportKey = { attribute_code: string; attribute_ids: Record<string, string> };

/* The option for importing without matching. No attribute code contains a dash alone. */
const CREATE_ONLY = '-';

const copy = {
  en: {
    label: 'Match existing records by',
    createOnly: "Don't match: every row creates a record",
    help: 'Rows whose value matches a record update that record; other rows create one. Choose an attribute with unique values.',
    columns: 'Name each column of your file after an attribute code. Empty cells leave a value as it is.',
    save: 'Save',
    cancel: 'Cancel',
    saved: 'CSV import settings saved',
    failed: 'These settings could not be loaded or saved. Try again.',
  },
  pl: {
    label: 'Dopasuj istniejące rekordy według',
    createOnly: 'Bez dopasowania: każdy wiersz tworzy rekord',
    help: 'Wiersze z wartością pasującą do rekordu aktualizują ten rekord, a pozostałe tworzą nowy. Wybierz atrybut o unikalnych wartościach.',
    columns: 'Nazwij każdą kolumnę pliku kodem atrybutu. Puste komórki pozostawiają wartość bez zmian.',
    save: 'Zapisz',
    cancel: 'Anuluj',
    saved: 'Zapisano ustawienia importu CSV',
    failed: 'Nie udało się wczytać ani zapisać tych ustawień. Spróbuj ponownie.',
  },
};

export const ImportSettings = () => {
  const { blueprint_id, blueprint_version } = useOutletContext<ActionContext>();
  const attricat = useAttricat();
  const text = useLocale().startsWith('pl') ? copy.pl : copy.en;
  const storageKey = `import-key:${blueprint_id}`;
  const [attributes, setAttributes] = useState<Attribute[]>();
  const [saved, setSaved] = useState<ImportKey | null>(null);
  const [choice, setChoice] = useState(CREATE_ONLY);
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    Promise.all([
      attricat.request(`/api/blueprints/${blueprint_id}/versions/${blueprint_version}`) as Promise<Revision>,
      attricat.storage.get({ key: storageKey }) as Promise<{ value: ImportKey } | null>,
    ])
      .then(([revision, entry]) => {
        setAttributes(revision.attributes.filter((a) => a.value_type === 'string' && !a.readonly));
        setSaved(entry?.value ?? null);
        setChoice(entry?.value.attribute_code ?? CREATE_ONLY);
      })
      .catch(() => setFailed(true));
  }, [blueprint_id, blueprint_version]);

  const save = async () => {
    setBusy(true);
    setFailed(false);
    try {
      const attribute = attributes?.find((a) => a.code === choice);
      if (attribute) {
        // A record of an older revision keeps matching by the IDs of its revision.
        const attribute_ids = saved?.attribute_code === choice ? saved.attribute_ids : {};
        const value: ImportKey = {
          attribute_code: choice,
          attribute_ids: { ...attribute_ids, [blueprint_version]: attribute.id },
        };
        await attricat.storage.set({ key: storageKey, value });
      } else {
        await attricat.storage.delete({ key: storageKey });
      }
      await attricat.notify({ message: text.saved, severity: 'success' });
      await attricat.dialog.close();
    } catch {
      setFailed(true);
      setBusy(false);
    }
  };

  return (
    <Stack gap="md">
      <Select
        label={text.label}
        helperText={text.help}
        error={failed ? text.failed : undefined}
        disabled={!attributes}
        value={choice}
        options={[
          { value: CREATE_ONLY, label: text.createOnly },
          // A name can be an untranslated string, which only the host resolves.
          ...(attributes ?? []).map((a) => ({ value: a.code, label: a.code })),
        ]}
        onChange={({ value }) => setChoice(value ?? CREATE_ONLY)}
      />
      <Text tone="muted">{text.columns}</Text>
      <Stack direction="horizontal" gap="sm">
        <Button variant="primary" disabled={!attributes || busy} onPress={save}>
          {text.save}
        </Button>
        <Button onPress={() => attricat.dialog.close()}>{text.cancel}</Button>
      </Stack>
    </Stack>
  );
};
