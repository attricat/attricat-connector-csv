import { defineConfig } from 'vitest/config';

export default defineConfig({
  esbuild: { jsx: 'automatic', jsxImportSource: 'preact' },
  test: { environment: 'node', include: ['client/**/*.test.tsx'] },
});
