import react from '@vitejs/plugin-react';
import { defineConfig } from 'vitest/config';

// The build goes into the crate, where it is committed and embedded in the binary, so that building Yalper
// never needs Node. CI rebuilds it and fails if the committed files differ.
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: '../crates/yalper/viewer',
    emptyOutDir: true,
    sourcemap: false,
    // Every run of the viewer has a new port, so a new origin and no browser cache: content hashes in file
    // names would only make each rebuild a new file instead of a small change to the same one.
    rolldownOptions: {
      output: {
        entryFileNames: 'assets/[name].js',
        chunkFileNames: 'assets/[name].js',
        assetFileNames: 'assets/[name][extname]',
      },
    },
    // No inline script and no `data:` URL, which the viewer's Content Security Policy refuses.
    modulePreload: { polyfill: false },
    assetsInlineLimit: 0,
    license: { fileName: 'licenses.json' },
  },
  test: {
    include: ['src/**/*.test.ts', 'scripts/**/*.test.ts'],
  },
});
