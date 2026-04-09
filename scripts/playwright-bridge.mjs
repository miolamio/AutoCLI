#!/usr/bin/env node
// Playwright Bridge: launches headless Chromium with Playwright and exposes a raw CDP endpoint.
// Usage: node playwright-bridge.mjs --storage=<path-to-storageState.json>

import { chromium } from 'playwright';
import http from 'node:http';
import net from 'node:net';

function parseArgs() {
  const args = process.argv.slice(2);
  let storagePath = null;
  for (const arg of args) {
    if (arg.startsWith('--storage=')) {
      storagePath = arg.slice('--storage='.length);
    }
  }
  return { storagePath };
}

async function getDebuggerUrl(port) {
  return new Promise((resolve, reject) => {
    const req = http.get(`http://127.0.0.1:${port}/json/version`, (res) => {
      let data = '';
      res.on('data', (chunk) => { data += chunk; });
      res.on('end', () => {
        try {
          const json = JSON.parse(data);
          resolve(json.webSocketDebuggerUrl);
        } catch (e) {
          reject(new Error(`Failed to parse /json/version response: ${e.message}`));
        }
      });
    });
    req.on('error', reject);
    req.setTimeout(5000, () => {
      req.destroy();
      reject(new Error('Timeout querying /json/version'));
    });
  });
}

async function getPageDebuggerUrl(port) {
  return new Promise((resolve, reject) => {
    const req = http.get(`http://127.0.0.1:${port}/json/list`, (res) => {
      let data = '';
      res.on('data', (chunk) => { data += chunk; });
      res.on('end', () => {
        try {
          const json = JSON.parse(data);
          // Find a page target
          const page = json.find(t => t.type === 'page');
          if (page && page.webSocketDebuggerUrl) {
            resolve(page.webSocketDebuggerUrl);
          } else {
            resolve(null);
          }
        } catch (e) {
          reject(new Error(`Failed to parse /json/list response: ${e.message}`));
        }
      });
    });
    req.on('error', reject);
    req.setTimeout(5000, () => {
      req.destroy();
      reject(new Error('Timeout querying /json/list'));
    });
  });
}

function getFreePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.listen(0, () => {
      const port = srv.address().port;
      srv.close(() => resolve(port));
    });
    srv.on('error', reject);
  });
}

async function main() {
  const { storagePath } = parseArgs();

  // Find a free port for remote debugging
  const debugPort = await getFreePort();

  const launchOptions = {
    headless: true,
    args: [
      `--remote-debugging-port=${debugPort}`,
      '--no-first-run',
      '--no-default-browser-check',
    ],
  };

  // Launch Chromium via Playwright
  const browser = await chromium.launch(launchOptions);

  // Create a context with storageState if provided
  const contextOptions = {};
  if (storagePath) {
    contextOptions.storageState = storagePath;
  }
  const context = await browser.newContext(contextOptions);
  await context.newPage();

  // Wait briefly for the debug port to be ready, then query for the CDP endpoint
  let cdpUrl = null;
  const maxRetries = 10;
  for (let i = 0; i < maxRetries; i++) {
    try {
      // Try to get a page-level endpoint first (preferred for CdpPage)
      const pageUrl = await getPageDebuggerUrl(debugPort);
      if (pageUrl) {
        cdpUrl = pageUrl;
        break;
      }
      // Fall back to browser-level endpoint
      cdpUrl = await getDebuggerUrl(debugPort);
      if (cdpUrl) break;
    } catch {
      // Retry after a short delay
      await new Promise(r => setTimeout(r, 300));
    }
  }

  if (!cdpUrl) {
    process.stderr.write('ERROR: Could not obtain CDP endpoint\n');
    await browser.close();
    process.exit(1);
  }

  // Print the CDP endpoint to stdout (the only line Rust reads)
  process.stdout.write(`CDP:${cdpUrl}\n`);

  // Keep running until stdin closes or SIGTERM
  const cleanup = async () => {
    try { await browser.close(); } catch {}
    process.exit(0);
  };

  process.on('SIGTERM', cleanup);
  process.on('SIGINT', cleanup);

  // Wait for stdin to close (parent process dies)
  process.stdin.resume();
  process.stdin.on('end', cleanup);
  process.stdin.on('close', cleanup);
}

main().catch((err) => {
  process.stderr.write(`playwright-bridge error: ${err.message}\n`);
  process.exit(1);
});
