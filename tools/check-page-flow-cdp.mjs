/* Drive the served page in a real browser (headless Chrome over CDP) and **assert** that one
 * upload reaches a rendered region list. This is the check that catches the M5 bug class.
 *
 * Why it exists
 * -------------
 * The M5 defect was not a wire-format problem. `/api/jobs/{id}/result` was byte-perfect and the
 * job succeeded server-side; the page threw the accepted job away before polling it, so it never
 * issued a single `GET /api/jobs/{id}`, the scanning animation never stopped, and the region list
 * stayed empty. No amount of field-level contract testing can see that: it is a control-flow bug
 * inside the page. Only executing the page against the real server can.
 *
 * The four invariants it asserts (each one was violated by the pre-M5 page):
 *   1. after the upload the page issues at least one `GET /api/jobs/{id}` — i.e. it polls the job
 *      it was given;
 *   2. it reaches `GET /api/jobs/{id}/result` with 200;
 *   3. `.stage` no longer carries `scanning` (the animation is owned and cleared);
 *   4. `#regionList` holds at least one region, and that region carries text.
 *
 * Modes
 * -----
 *   node tools/check-page-flow-cdp.mjs --capture <capture.json>
 *       Assert the invariants against an already-recorded capture (used to prove the check
 *       *bites*: the pre-fix capture must FAIL it).
 *   node tools/check-page-flow-cdp.mjs --base <url> --cdp <port> --image <path> [--out <dir>]
 *       Drive a live server end to end and assert the same invariants.
 */
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';

const argv = process.argv.slice(2);
const arg = (name, fallback = null) => {
  const at = argv.indexOf(`--${name}`);
  return at >= 0 && argv[at + 1] && !argv[at + 1].startsWith('--') ? argv[at + 1] : fallback;
};
const base = arg('base');
const cdpPort = Number(arg('cdp', '0'));
const imagePath = arg('image');
const outDir = arg('out', join('target', 'flow-gate', 'cdp-live'));
const capturePath = arg('capture');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Run the page against a live server and return the same shape as a recorded capture. */
async function record() {
  if (!base || !cdpPort || !imagePath) {
    throw new Error('live mode needs --base <url> --cdp <port> --image <path>');
  }
  mkdirSync(outDir, { recursive: true });

  async function targetWs() {
    for (let i = 0; i < 80; i += 1) {
      try {
        const list = await (await fetch(`http://127.0.0.1:${cdpPort}/json/list`)).json();
        const page = list.find((t) => t.type === 'page');
        if (page && page.webSocketDebuggerUrl) return page.webSocketDebuggerUrl;
      } catch {}
      await sleep(250);
    }
    throw new Error('CDP target not found');
  }

  const ws = new WebSocket(await targetWs());
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = () => rej(new Error('ws error')); });
  let id = 0;
  const pending = new Map();
  const requests = [];
  const responses = [];
  const exceptions = [];
  ws.onmessage = (event) => {
    const message = JSON.parse(event.data);
    if (message.id && pending.has(message.id)) { pending.get(message.id)(message); pending.delete(message.id); return; }
    if (message.method === 'Network.requestWillBeSent') {
      requests.push({ id: message.params.requestId, method: message.params.request.method, url: message.params.request.url });
    } else if (message.method === 'Network.responseReceived') {
      responses.push({ id: message.params.requestId, status: message.params.response.status, url: message.params.response.url });
    } else if (message.method === 'Runtime.exceptionThrown') {
      exceptions.push(message.params.exceptionDetails?.exception?.description || message.params.exceptionDetails?.text);
    }
  };
  const send = (method, params) => {
    const mid = ++id;
    ws.send(JSON.stringify({ id: mid, method, params: params || {} }));
    return new Promise((res) => pending.set(mid, res));
  };
  const evaluate = async (expression) => {
    const r = await send('Runtime.evaluate', { expression, returnByValue: true });
    const d = r.result || {};
    if (d.exceptionDetails) throw new Error(d.exceptionDetails.exception?.description || d.exceptionDetails.text);
    return d.result ? d.result.value : null;
  };

  await send('Runtime.enable');
  await send('Page.enable');
  await send('Network.enable');
  await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
  await send('Page.navigate', { url: `${base}/` });
  await sleep(2500);

  const document = await send('DOM.getDocument', { depth: -1 });
  const input = await send('DOM.querySelector', { nodeId: document.result.root.nodeId, selector: '#fileInput' });
  const set = await send('DOM.setFileInputFiles', { files: [imagePath], nodeId: input.result.nodeId });
  if (set.error) throw new Error(`setFileInputFiles: ${JSON.stringify(set.error)}`);
  await sleep(1200);

  /* The reporter's action: press 开始识别. */
  await evaluate(`document.querySelector('#ocrBtn').click(); 'clicked'`);

  const samples = [];
  let after = null;
  for (let i = 0; i < 60; i += 1) {
    await sleep(500);
    const probe = JSON.parse(await evaluate(`JSON.stringify({
      stageClass: (document.querySelector('.stage')||{}).className || null,
      jobHidden: (document.querySelector('#jobLine')||{}).hidden,
      jobLine: (document.querySelector('#jobLine')||{}).textContent || null,
      regionItems: document.querySelectorAll('#regionList .region-item').length,
      toasts: [...document.querySelectorAll('#toasts .toast')].map(t => t.textContent)
    })`));
    samples.push(probe);
    after = probe;
    if (probe.regionItems > 0 && !String(probe.stageClass).includes('scanning')) break;
  }
  const firstRegions = JSON.parse(await evaluate(
    `JSON.stringify([...document.querySelectorAll('#regionList .region-item')].slice(0,3).map(e => e.textContent.trim().slice(0,80)))`,
  ));

  const capture = {
    baseUrl: base,
    apiResponses: responses
      .map((r) => ({ ...r, ...(requests.find((q) => q.id === r.id) || {}) }))
      .filter((r) => r.url.includes('/api/')),
    after: { ...after, firstRegions },
    samples,
    exceptions,
  };
  writeFileSync(join(outDir, 'cdp-flow.json'), JSON.stringify(capture, null, 2));
  ws.close();
  return capture;
}

const capture = capturePath ? JSON.parse(readFileSync(capturePath, 'utf8')) : await record();
const calls = capture.apiResponses || [];
const label = capture.baseUrl || '(recorded capture)';

const problems = [];
const check = (ok, message) => { if (!ok) problems.push(message); };

const polls = calls.filter((c) => /\/api\/jobs\/[^/]+$/.test(c.url) && c.method === 'GET');
const results = calls.filter((c) => /\/api\/jobs\/[^/]+\/result$/.test(c.url));
const submitted = calls.find((c) => c.url.includes('/api/ocr') && c.method === 'POST');

check(!!submitted, 'the page must POST /api/ocr');
check(!!submitted && submitted.status === 202, `POST /api/ocr must be accepted (202), got ${submitted && submitted.status}`);
check(polls.length > 0, 'the page must poll GET /api/jobs/{id} at least once — this is exactly what the pre-M5 page never did');
check(results.length > 0, 'the page must fetch GET /api/jobs/{id}/result');
check(results.every((r) => r.status === 200), `every /result fetch must be 200, got ${results.map((r) => r.status).join(',')}`);
check(!String(capture.after.stageClass).includes('scanning'), `the scanning animation must be cleared, stage class was \`${capture.after.stageClass}\``);
check(capture.after.regionItems > 0, `the region list must render at least one region, got ${capture.after.regionItems}`);
check((capture.exceptions || []).length === 0, `the page must raise no exception, got ${JSON.stringify(capture.exceptions)}`);
const firstText = (capture.after.firstRegions || [])[0] || '';
check(/[\p{L}\p{N}]/u.test(firstText), `the first rendered region must carry text, got ${JSON.stringify(firstText)}`);

console.log(`capture: ${label}`);
console.log(`  POST /api/ocr           -> ${submitted ? submitted.status : 'MISSING'}`);
console.log(`  GET /api/jobs/{id}      -> ${polls.length} poll(s) ${polls.map((p) => p.status).join(',')}`);
console.log(`  GET /api/jobs/{id}/result -> ${results.length} fetch(es) ${results.map((r) => r.status).join(',')}`);
console.log(`  .stage class            -> ${capture.after.stageClass}`);
console.log(`  rendered regions        -> ${capture.after.regionItems}`);
console.log(`  first region            -> ${JSON.stringify(String(firstText).replace(/\s+/g, ' ').trim().slice(0, 60))}`);

if (problems.length) {
  for (const problem of problems) console.error(`FAIL ${problem}`);
  console.error(`\npage flow check FAILED (${problems.length} problem(s))`);
  process.exit(1);
}
console.log('\npage flow check PASSED: one upload polls its job, fetches its result, clears the animation and renders regions');
