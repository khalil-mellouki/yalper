import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { allowList, isAllowed } from './licenses.mjs';

const allowed = allowList(readFileSync(new URL('../../deny.toml', import.meta.url), 'utf8'));

describe('the license check of bundled dependencies', () => {
  it('reads the allow list of deny.toml', () => {
    expect(allowed.has('MIT')).toBe(true);
    expect(allowed.has('Apache-2.0 WITH LLVM-exception')).toBe(true);
    expect(allowed.has('GPL-3.0-only')).toBe(false);
  });

  it('evaluates SPDX expressions against it', () => {
    expect(isAllowed('MIT', allowed)).toBe(true);
    expect(isAllowed('(MIT OR GPL-3.0-only)', allowed)).toBe(true);
    expect(isAllowed('MIT AND ISC', allowed)).toBe(true);
    expect(isAllowed('Apache-2.0 WITH LLVM-exception', allowed)).toBe(true);
    expect(isAllowed('GPL-2.0-only WITH LLVM-exception', allowed)).toBe(false);
    expect(isAllowed('MPL-2.0', allowed)).toBe(false);
    expect(isAllowed('MIT AND MPL-2.0', allowed)).toBe(false);
    expect(isAllowed('MPL-2.0 OR (MIT AND CC-BY-4.0)', allowed)).toBe(false);
    expect(isAllowed('MIT AND (ISC OR MPL-2.0)', allowed)).toBe(true);
  });

  it('refuses a missing or malformed license', () => {
    for (const expression of [undefined, null, '', 'MIT OR', '(MIT', 'MIT)', 'MIT ISC', 'WITH', 'MIT WITH']) {
      expect(isAllowed(expression, allowed)).toBe(false);
    }
  });
});
