/* Extract every `<script nonce="__CSP_NONCE__">` block from the inlined page so each one can
   be syntax-checked with `node --check`. The page has two such blocks (the boot shim and the
   app); checking them individually is stronger than checking the whole document.

   usage: node extract-page-scripts.mjs <index.html> <outDir> */
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';

const [pagePath, outDir] = process.argv.slice(2);
if(!pagePath || !outDir) throw new Error('usage: node extract-page-scripts.mjs <index.html> <outDir>');
mkdirSync(outDir, { recursive: true });
const html = readFileSync(pagePath, 'utf8');
const marker = '<script nonce="__CSP_NONCE__">';
const written = [];
let cursor = 0;
for(;;){
  const start = html.indexOf(marker, cursor);
  if(start < 0) break;
  const bodyStart = start + marker.length;
  const end = html.indexOf('</script>', bodyStart);
  if(end < 0) throw new Error('unterminated <script> block');
  const file = join(outDir, `page-script-${written.length}.js`);
  writeFileSync(file, html.slice(bodyStart, end));
  written.push(file);
  cursor = end + '</script>'.length;
}
if(!written.length) throw new Error('no nonce script block found in the page');
console.log(written.join('\n'));
console.log(`blocks=${written.length}`);
