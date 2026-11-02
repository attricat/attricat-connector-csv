import { defineConfig } from 'vite';
import { attricatExtension } from '@attricat/extension/vite';

// Native UI: one classic Worker script at dist/ui.js, within the size budget,
// with the UI kit version it was built against recorded beside it.
export default defineConfig(attricatExtension({ entry: 'client/main.tsx' }));
