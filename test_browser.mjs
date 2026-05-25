// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

// Browser integration test — validates WASM queries via each page's built-in
// validation UI.  Each dataset page has a "Run All Queries & Validate" button
// that runs its example queries against validation.json ground truth.  This
// script clicks that button headlessly and reads the results from the DOM.
//
// Datasets tested:
//   1. Synthetic 6-resource example  (example/index.html)
//   2. Aonach Mór 12k-resource dataset (example/aonach_mor/index.html)
//
// Usage: node test_browser.mjs

import puppeteer from 'puppeteer';
import { createServer } from 'http';
import { readFileSync, existsSync, statSync } from 'fs';
import { resolve, extname } from 'path';

const MIME = {
  '.html': 'text/html', '.js': 'application/javascript', '.wasm': 'application/wasm',
  '.json': 'application/json', '.bin': 'application/octet-stream', '.dat': 'application/octet-stream',
  '.css': 'text/css', '.ttl': 'text/turtle', '.nt': 'text/plain',
};
const ROOT = resolve('example');

function startServer() {
  return new Promise((res) => {
    const srv = createServer((req, resp) => {
      const url = new URL(req.url, 'http://localhost');
      let fp = resolve(ROOT, '.' + url.pathname);
      if (fp.endsWith('/')) fp += 'index.html';
      if (!existsSync(fp) || statSync(fp).isDirectory()) {
        resp.writeHead(404); resp.end(); return;
      }
      const data = readFileSync(fp);
      const ext = extname(fp);
      const mime = MIME[ext] || 'application/octet-stream';
      const range = req.headers.range;
      if (range) {
        const m = range.match(/bytes=(\d+)-(\d*)/);
        if (m) {
          const start = parseInt(m[1]);
          const end = m[2] ? Math.min(parseInt(m[2]), data.length - 1) : data.length - 1;
          resp.writeHead(206, {
            'Content-Range': `bytes ${start}-${end}/${data.length}`,
            'Content-Length': end - start + 1,
            'Content-Type': mime,
            'Access-Control-Allow-Origin': '*',
          });
          resp.end(data.slice(start, end + 1));
          return;
        }
      }
      resp.writeHead(200, { 'Content-Type': mime, 'Access-Control-Allow-Origin': '*' });
      resp.end(data);
    });
    srv.listen(0, () => res({ srv, port: srv.address().port }));
  });
}

async function runTests() {
  const { srv, port } = await startServer();
  const baseUrl = `http://localhost:${port}`;
  console.log(`Server on ${baseUrl}`);

  const browser = await puppeteer.launch({ headless: true, args: ['--no-sandbox'] });
  const page = await browser.newPage();
  page.on('console', msg => {
    if (msg.type() === 'error') console.log(`  [console.error] ${msg.text()}`);
  });

  let totalPassed = 0, totalFailed = 0;

  // Navigate to a dataset page, wait for init, click the validation button,
  // and read the pass/fail results from the DOM.
  //
  // opts.initStore  — call window.initStore() manually (pages without auto-init)
  // opts.indexUrl   — set the base-url input before init (configurable pages)
  async function testDataset(label, pagePath, opts = {}) {
    console.log(`\n=== ${label} ===`);

    await page.goto(`${baseUrl}${pagePath}`, { waitUntil: 'domcontentloaded' });

    if (opts.indexUrl) {
      await page.evaluate((u) => {
        const el = document.getElementById('base-url');
        if (el) el.value = u;
      }, opts.indexUrl);
    }
    if (opts.initStore) {
      await page.evaluate(() => window.initStore());
    }

    // Wait for "Ready" or "Error"
    await page.waitForFunction(() => {
      const s = document.getElementById('status');
      if (!s) return false;
      const t = s.textContent;
      return t.includes('Ready') || s.className === 'ready'
          || t.includes('Error') || s.className === 'error';
    }, { timeout: 30000 });

    const status = await page.evaluate(() => document.getElementById('status').textContent);
    if (status.includes('Error')) {
      console.log(`  FAIL: init failed — ${status}`);
      totalFailed++;
      return;
    }
    console.log(`  Store: ${status}`);

    // Click the validation button (id varies between pages)
    await page.click('button[onclick*="runValidation"]');

    // Wait for validation to complete
    await page.waitForFunction(() => {
      const s = document.getElementById('val-status');
      return s && s.textContent
        && !s.textContent.includes('Running')
        && !s.textContent.includes('Loading');
    }, { timeout: 120000 });

    // Read results from the table
    const result = await page.evaluate(() => {
      const summary = document.getElementById('val-status').textContent;
      const rows = [...document.querySelectorAll('table.val tr')].slice(1);
      return {
        summary,
        rows: rows.map(r => {
          const cells = [...r.children].map(c => c.textContent.trim());
          return { query: cells[0], expected: cells[1], got: cells[2], cls: r.className };
        }),
      };
    });

    console.log(`  ${result.summary}`);
    for (const r of result.rows) {
      const icon = r.cls === 'pass' ? 'PASS' : r.cls === 'fail' ? 'FAIL' : 'WARN';
      console.log(`    ${icon} ${r.query}: expected=${r.expected} got=${r.got}`);
    }

    totalPassed += result.rows.filter(r => r.cls === 'pass').length;
    totalFailed += result.rows.filter(r => r.cls === 'fail').length;
  }

  // --- Synthetic example ---
  await testDataset(
    'Synthetic example (6 resources)',
    '/index.html',
    { initStore: true, indexUrl: './static/ros-madair/' },
  );

  // --- Aonach Mór ---
  if (existsSync(resolve(ROOT, 'aonach_mor/static/ros-madair/summary.bin'))) {
    await testDataset('Aonach Mór (12k resources)', '/aonach_mor/index.html');
  } else {
    console.log('\n=== Aonach Mór: SKIPPED (index not built) ===');
  }

  console.log(`\n=== Summary: ${totalPassed} passed, ${totalFailed} failed ===`);
  await browser.close();
  srv.close();
  process.exit(totalFailed > 0 ? 1 : 0);
}

runTests().catch(e => { console.error(e); process.exit(1); });
