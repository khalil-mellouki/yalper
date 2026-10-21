// Checks the licenses of the dependencies bundled into the viewer against the allow list of `deny.toml`, the
// one cargo-deny applies to the Rust dependencies, so the binary carries no license that list does not allow.

/**
 * The `allow` list of the `[licenses]` section of a `deny.toml`.
 * @param {string} denyToml
 * @returns {Set<string>}
 */
export function allowList(denyToml) {
  const list = /^\[licenses\][^[]*?^allow\s*=\s*\[([^\]]*)\]/m.exec(denyToml);
  if (!list) {
    throw new Error('deny.toml has no [licenses] allow list');
  }
  return new Set(Array.from(list[1].matchAll(/"([^"]+)"/g), (match) => match[1]));
}

/**
 * Whether an SPDX license expression is satisfied by the allowed licenses: `A OR B` needs one of them,
 * `A AND B` both, and `A WITH B` must be allowed as a whole. An expression that does not parse is refused.
 * @param {string | null | undefined} expression
 * @param {Set<string>} allowed
 * @returns {boolean}
 */
export function isAllowed(expression, allowed) {
  const tokens = expression?.match(/[()]|[^\s()]+/g) ?? [];
  let position = 0;
  const operators = new Set(['AND', 'OR', 'WITH', '(', ')']);

  /** @returns {boolean} */
  function term() {
    const token = tokens[position++];
    if (token === '(') {
      const inner = anyOf();
      if (tokens[position++] !== ')') {
        throw new Error('missing )');
      }
      return inner;
    }
    if (token === undefined || operators.has(token)) {
      throw new Error('license expected');
    }
    if (tokens[position] !== 'WITH') {
      return allowed.has(token);
    }
    position++;
    const exception = tokens[position++];
    if (exception === undefined || operators.has(exception)) {
      throw new Error('exception expected');
    }
    return allowed.has(`${token} WITH ${exception}`);
  }

  /** @returns {boolean} */
  function allOf() {
    let ok = term();
    while (tokens[position] === 'AND') {
      position++;
      const next = term();
      ok = ok && next;
    }
    return ok;
  }

  /** @returns {boolean} */
  function anyOf() {
    let ok = allOf();
    while (tokens[position] === 'OR') {
      position++;
      const next = allOf();
      ok = ok || next;
    }
    return ok;
  }

  try {
    const ok = anyOf();
    return ok && position === tokens.length;
  } catch {
    return false;
  }
}
