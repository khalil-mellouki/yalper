// Fails when the built viewer grows over its budget: everything embedded in the binary under 3 MB, and the
// script the page loads first under 500 kB (Vite's own warning limit), so the viewer opens fast.

import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const TOTAL_BUDGET = 3_000_000;
const ENTRY_BUDGET = 500_000;

const viewer = fileURLToPath(new URL('../../crates/yalper/viewer/', import.meta.url));

let total = 0;
for (const file of readdirSync(viewer, { recursive: true, encoding: 'utf8' })) {
  const stats = statSync(join(viewer, file));
  if (stats.isFile()) {
    total += stats.size;
  }
}

const html = readFileSync(join(viewer, 'index.html'), 'utf8');
const entry = /<script type="module"[^>]*\ssrc="\/([^"]+)"/.exec(html)?.[1];
if (entry === undefined) {
  console.error('index.html loads no module script');
  process.exit(1);
}
const entrySize = statSync(join(viewer, entry)).size;

console.log(`Embedded viewer: ${total} bytes (budget ${TOTAL_BUDGET})`);
console.log(`Entry chunk ${entry}: ${entrySize} bytes (budget ${ENTRY_BUDGET})`);
if (total > TOTAL_BUDGET || entrySize > ENTRY_BUDGET) {
  console.error('The viewer is over its size budget.');
  process.exit(1);
}
