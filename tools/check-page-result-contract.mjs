/* Run the **page's own** `/result` parser over a **real captured** response body, in node.
 *
 * Why this exists
 * ---------------
 * The M5 bug was not a server defect: the server's `/result` was byte-perfect and the job
 * succeeded. The page, however, threw the accepted job away before polling it, so nothing was
 * ever rendered. The only way to keep that class of failure from coming back is to execute the
 * page's real parsing code against a real response body somewhere in CI, and to prove that the
 * check *bites* — i.e. that a renamed or missing field is reported with its path.
 *
 * What it does
 * ------------
 * 1. Extracts the named top-level functions of `src/bin/web/index.html` (the parser pipeline:
 *    pget / checkFinite / isInputRegion / parsePolygon / parseRecognition / parseFormula /
 *    normalizeResult / contractFail) and runs them in a `node:vm` context with only the three
 *    things they need stubbed (`state`, `renderDiag`, `toast`). Nothing is re-implemented here:
 *    the code under test is the shipped page code, character for character.
 * 2. Feeds it the captured real body (`tests/fixtures/serve/result-real-42-regions.json`,
 *    SHA-256 pinned by the Rust test as well) and requires a region list to come out.
 * 3. Runs mutation cases — one required field renamed/removed/truncated at a time — and requires
 *    the parser to reject each one **and to name the offending path**. A check that cannot fail
 *    is not a check.
 *
 * usage: node tools/check-page-result-contract.mjs [--quiet]
 */
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import vm from 'node:vm';
import { createHash } from 'node:crypto';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
const QUIET = process.argv.includes('--quiet');

const PAGE = join(root, 'src/bin/web/index.html');
const BODY = join(root, 'tests/fixtures/serve/result-real-42-regions.json');
/** The SHA-256 of the captured body; the Rust contract test pins the same value. */
const BODY_SHA256 = 'fda2f71337aa8b03f147d5fb4b0065981fe0444703c30dfb74573439f691fd27';

/** Top-level functions the parser pipeline is made of, in dependency order. */
const PARSER_FUNCTIONS = [
  'pget',
  'checkFinite',
  'isInputRegion',
  'parsePolygon',
  'parseRecognition',
  'parseFormula',
  'normalizeResult',
  // The failure path is part of the contract too: a rejected field must become a visible toast
  // and a diagnostics entry naming the path, so `diagPush` is exercised rather than stubbed.
  'diagPush',
  'contractFail',
];

/**
 * Extract the page's `<script nonce=...>` body and, from it, the named top-level function
 * declarations. The page mixes one-line helpers (`function pget(o, k){ … }`) with multi-line
 * ones, so the end of a function is found by scanning to the brace that closes its body, skipping
 * string literals and line comments. Each name must be found and must yield a complete body —
 * otherwise this script fails loudly instead of silently testing nothing.
 */
function extractParser(source) {
  const marker = '<script nonce="__CSP_NONCE__">';
  // The page has **two** nonce blocks: a 41-byte boot shim and the app. Concatenating them keeps
  // this extractor independent of which block a function happens to live in.
  let script = '';
  let cursor = 0;
  for (;;) {
    const start = source.indexOf(marker, cursor);
    if (start < 0) break;
    const bodyStart = start + marker.length;
    const end = source.indexOf('</script>', bodyStart);
    if (end < 0) throw new Error('a nonce script block is unterminated');
    script += `${source.slice(bodyStart, end)}\n`;
    cursor = end + '</script>'.length;
  }
  if (!script) throw new Error('the page carries no nonce script block');

  const parts = [];
  for (const name of PARSER_FUNCTIONS) {
    const head = `function ${name}(`;
    const at = script.indexOf(head);
    if (at < 0) throw new Error(`the page no longer declares function ${name}`);
    const close = matchingBrace(script, at + head.length - 1);
    if (close < 0) throw new Error(`function ${name} has no matching closing brace`);
    const chunk = script.slice(at, close + 1);
    if (!chunk.startsWith(head)) throw new Error(`bad extraction for ${name}`);
    parts.push(chunk);
  }
  return parts.join('\n');
}

/**
 * Index of the `}` that closes the `{` at `openIndex`, ignoring braces inside string literals
 * and line comments. Returns -1 when unbalanced.
 */
function matchingBrace(text, openIndex) {
  let depth = 0;
  let quote = null;
  for (let i = openIndex; i < text.length; i += 1) {
    const c = text[i];
    if (quote) {
      if (c === '\\') { i += 1; continue; }
      if (c === quote) quote = null;
      continue;
    }
    if (c === '/' && text[i + 1] === '/') {
      const nl = text.indexOf('\n', i);
      if (nl < 0) return -1;
      i = nl;
      continue;
    }
    if (c === "'" || c === '"' || c === '`') { quote = c; continue; }
    if (c === '{') depth += 1;
    else if (c === '}') {
      depth -= 1;
      if (depth === 0) return i;
    }
  }
  return -1;
}

/** Evaluate the extracted page code with the minimum stubs it needs. */
function loadParser(code) {
  const captured = { diag: [], toasts: [], results: [] };
  const context = {
    state: { diag: [], result: null },
    renderDiag: () => {},
    toast: (message, type) => captured.toasts.push({ message, type }),
  };
  vm.createContext(context);
  vm.runInContext(`${code}\n;globalThis.__normalizeResult = normalizeResult;`, context, {
    filename: 'page-parser.js',
  });
  return {
    normalizeResult: (value) => {
      context.state.diag = [];
      captured.diag = context.state.diag;
      captured.toasts.length = 0;
      const parsed = context.__normalizeResult(value);
      captured.results.push(parsed);
      return { parsed, diag: [...context.state.diag], toasts: [...captured.toasts] };
    },
  };
}

const failures = [];
const check = (condition, message) => {
  if (!condition) failures.push(message);
};
const section = (title) => {
  if (!QUIET) console.log(`\n=== ${title} ===`);
};

const page = readFileSync(PAGE, 'utf8');
const raw = readFileSync(BODY);
const sha = createHash('sha256').update(raw).digest('hex');
const body = JSON.parse(raw.toString('utf8'));

section('captured body');
check(
  sha === BODY_SHA256,
  `the captured body must be the pinned bytes: sha256 ${sha} != ${BODY_SHA256}`,
);
if (!QUIET) {
  console.log(`sha256 ${sha}`);
  console.log(`top-level keys ${Object.keys(body).sort().join(', ')}`);
  console.log(`regions ${body.regions.length}`);
}

const code = extractParser(page);
if (!QUIET) console.log(`extracted ${PARSER_FUNCTIONS.length} page functions (${code.length} bytes)`);
const parser = loadParser(code);

section('the page parses the real captured body');
const real = parser.normalizeResult(body);
check(real.parsed !== null, `the page must render the real captured body: diag=${JSON.stringify(real.diag)}`);
check(real.diag.length === 0, `no contract failure expected for the real body: ${JSON.stringify(real.diag)}`);
check(
  real.parsed && real.parsed.regions.length === body.regions.length,
  `every region must survive parsing: ${real.parsed ? real.parsed.regions.length : 'null'} vs ${body.regions.length}`,
);
if (real.parsed) {
  const first = real.parsed.regions[0];
  check(typeof first.text === 'string' && first.text.length > 0, 'the first region must carry text');
  check(typeof first.conf === 'number', `the first region must carry a numeric score: ${first.conf}`);
  check(Array.isArray(first.polygon) && first.polygon.length === 4, 'the first region must carry four corners');
  check(first.kind === 'text', `the first region must be a text region: ${first.kind}`);
  check(
    typeof real.parsed.plain === 'string' && real.parsed.plain.includes('\n'),
    'the copy-all text must come from `plain_text`',
  );
  check(real.parsed.ledger !== null, 'the diagnostics ledger must be read from `timing_ledger`');
  if (!QUIET) {
    console.log(`regions rendered: ${real.parsed.regions.length}`);
    console.log(`first three: ${real.parsed.regions.slice(0, 3).map((r) => r.text).join(' | ')}`);
    console.log(`plain text: ${real.parsed.plain.length} chars, ledger: ${real.parsed.ledger ? 'read' : 'missing'}`);
  }
}

section('the check bites: a renamed or missing field is reported with its path');
/** Each case mutates the real captured body and requires the named path to be reported. */
const mutations = [
  {
    label: 'regions renamed to items',
    path: 'regions',
    mutate: (copy) => {
      copy.items = copy.regions;
      delete copy.regions;
    },
  },
  {
    label: 'region kind renamed to type',
    path: 'regions[0].kind',
    mutate: (copy) => {
      copy.regions[0].type = copy.regions[0].kind;
      delete copy.regions[0].kind;
    },
  },
  {
    label: 'polygon.points truncated to three corners',
    path: 'regions[0].polygon.points',
    mutate: (copy) => {
      copy.regions[0].polygon.points = copy.regions[0].polygon.points.slice(0, 3);
    },
  },
  {
    label: 'polygon point flattened to a number',
    path: 'regions[0].polygon.points[1]',
    mutate: (copy) => {
      copy.regions[0].polygon.points[1] = 42;
    },
  },
  {
    label: 'recognition.text removed',
    path: 'regions[0].recognition.text',
    mutate: (copy) => {
      delete copy.regions[0].recognition.text;
    },
  },
  {
    label: 'recognition.score removed',
    path: 'regions[0].recognition.score',
    mutate: (copy) => {
      delete copy.regions[0].recognition.score;
    },
  },
  {
    label: 'recognition renamed to rec',
    path: 'regions[0].recognition',
    mutate: (copy) => {
      copy.regions[0].rec = copy.regions[0].recognition;
      delete copy.regions[0].recognition;
    },
  },
  {
    label: 'a formula region without formula.latex',
    path: 'regions[0].formula.latex',
    mutate: (copy) => {
      copy.regions[0].kind = 'formula';
      delete copy.regions[0].recognition;
      copy.regions[0].formula = { model_id: 'pinned-check' };
    },
  },
  {
    label: 'a polygon missing on a detected region',
    path: 'regions[0].polygon',
    mutate: (copy) => {
      delete copy.regions[0].polygon;
    },
  },
];

for (const testCase of mutations) {
  const copy = JSON.parse(raw.toString('utf8'));
  testCase.mutate(copy);
  const outcome = parser.normalizeResult(copy);
  check(outcome.parsed === null, `${testCase.label}: the parser must refuse to render`);
  const named = outcome.diag.some((line) => line.includes(testCase.path));
  check(
    named,
    `${testCase.label}: the parser must name \`${testCase.path}\`, got ${JSON.stringify(outcome.diag)}`,
  );
  check(
    outcome.toasts.length === 1,
    `${testCase.label}: a contract failure must also be visible as exactly one toast, got ${outcome.toasts.length}`,
  );
  if (!QUIET) console.log(`  ${named && outcome.parsed === null ? 'ok  ' : 'FAIL'} ${testCase.label} -> ${JSON.stringify(outcome.diag)}`);
}

section('result');
if (failures.length) {
  for (const failure of failures) console.error(`FAIL ${failure}`);
  console.error(`\npage/result contract check FAILED (${failures.length} problem(s))`);
  process.exit(1);
}
console.log('page/result contract check PASSED: the page parses the real captured body, and every required field rename is reported with its path');
