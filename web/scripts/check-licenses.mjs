// Fails when a dependency bundled into the viewer has a license outside the allow list of `deny.toml`. The list
// of bundled dependencies is the `licenses.json` that `npm run build` writes next to the viewer.

import { readFileSync } from 'node:fs';
import { allowList, isAllowed } from './licenses.mjs';

const repository = new URL('../../', import.meta.url);
const allowed = allowList(readFileSync(new URL('deny.toml', repository), 'utf8'));
/** @type {{ name: string, version: string, identifier?: string | null }[]} */
const bundled = JSON.parse(
  readFileSync(new URL('crates/yalper/viewer/licenses.json', repository), 'utf8'),
);

let refused = 0;
for (const { name, version, identifier } of bundled) {
  if (isAllowed(identifier, allowed)) {
    console.log(`${name}@${version}: ${identifier}`);
  } else {
    refused++;
    console.error(
      `${name}@${version}: license ${identifier ?? '(none)'} is not allowed by the [licenses] list of deny.toml`,
    );
  }
}
if (refused > 0) {
  process.exit(1);
}
console.log(`${bundled.length} bundled dependencies, all with an allowed license.`);
