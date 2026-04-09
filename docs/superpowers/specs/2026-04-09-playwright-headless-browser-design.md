# Playwright Headless Browser Integration

**Date:** 2026-04-09
**Scope:** Personal fork (miolamio/AutoCLI), YouTube only
**Goal:** Replace visible Chrome window + daemon + extension with headless Playwright for cookie-based adapters

## Problem

YouTube adapters (transcript, video, comments) require browser with authenticated session. Current architecture opens a visible Chrome window via daemon + extension. This is disruptive — a browser window pops up every time.

## Solution

Add a Playwright-based headless browser path to `BrowserBridge`. When a cookie-strategy adapter runs and auth state exists, launch headless Chromium via a Node.js helper script, connect through the existing `CdpPage` CDP implementation.

## Architecture

```
CLI → BrowserBridge::connect()
        │
        ├── auth state exists (~/.autocli/auth/youtube.json)?
        │       → PlaywrightBridge::launch()
        │           → spawn: node scripts/playwright-bridge.mjs
        │               → headless Chromium with storageState
        │               → prints CDP ws:// endpoint to stdout
        │           → CdpPage::connect(endpoint)
        │           → pipeline runs (navigate, wait, evaluate)
        │           → Drop → kills child process
        │
        └── no auth state?
                → error with suggestion: "Run autocli auth youtube first"
```

## Components

### 1. `scripts/playwright-bridge.mjs`

Node.js script that launches headless Chromium with Playwright and exposes CDP endpoint.

**Input:** `--storage <path>` — path to Playwright storageState JSON
**Output:** Single line to stdout: `CDP:ws://127.0.0.1:PORT/devtools/browser/ID`
**Lifecycle:** Runs until stdin closes or SIGTERM received, then closes browser.

**Implementation:**
```javascript
import { chromium } from 'playwright';

const storageArg = process.argv.find(a => a.startsWith('--storage='));
const storagePath = storageArg?.split('=')[1];

const browser = await chromium.launch({ headless: true });
const context = await browser.newContext({
  storageState: storagePath || undefined,
});
const page = await context.newPage();

// Get CDP endpoint from browser
const wsEndpoint = browser.wsEndpoint();  // Playwright exposes this directly
console.log(`CDP:${wsEndpoint}`);

// Wait for parent process to signal shutdown
process.stdin.resume();
process.stdin.on('end', async () => {
  await browser.close();
  process.exit(0);
});
process.on('SIGTERM', async () => {
  await browser.close();
  process.exit(0);
});
```

**Note:** Playwright's `browser.wsEndpoint()` returns a Playwright-protocol WS URL, not raw CDP. To get a raw CDP endpoint for `CdpPage`, we need to use Playwright's `chromium.launch()` with `args: ['--remote-debugging-port=0']` and then discover the CDP endpoint via the browser's debug info endpoint. Alternative: use `chromium.connectOverCDP()` in reverse — launch Chromium directly with debugging port and let Playwright manage it. The exact CDP endpoint extraction method will be determined during implementation.

### 2. `PlaywrightBridge` in `crates/autocli-browser/src/bridge.rs`

New struct managing the Node.js child process lifecycle.

```rust
pub struct PlaywrightBridge {
    child: tokio::process::Child,
}

impl PlaywrightBridge {
    /// Launch headless Chromium via Playwright and return a CdpPage.
    pub async fn launch(auth_state: &Path) -> Result<(Arc<dyn IPage>, Self), CliError> {
        // 1. Locate scripts/playwright-bridge.mjs relative to binary or embedded
        // 2. spawn: node <script> --storage=<auth_state>
        //    - pipe stdout for reading CDP endpoint
        //    - pipe stdin for shutdown signaling
        // 3. Read first line from stdout, parse "CDP:ws://..." 
        //    - Timeout after 15s
        // 4. CdpPage::connect(endpoint)
        // 5. Return (page, PlaywrightBridge { child })
    }
}

impl Drop for PlaywrightBridge {
    fn drop(&mut self) {
        // Kill the Node.js process (and Chromium with it)
        let _ = self.child.start_kill();
    }
}
```

### 3. Changes to `BrowserBridge::connect()`

Add Playwright path before daemon path:

```rust
pub async fn connect(&mut self, site: &str) -> Result<Arc<dyn IPage>, CliError> {
    // New: check for Playwright auth state
    let auth_path = dirs::home_dir()
        .unwrap()
        .join(format!(".autocli/auth/{site}.json"));
    
    if auth_path.exists() {
        let (page, _bridge) = PlaywrightBridge::launch(&auth_path).await?;
        return Ok(page);
    }
    
    // Existing daemon flow (unchanged)
    // ...
}
```

**Signature change:** `connect()` needs to know the site name to find auth state. Currently it takes no args. Add `site: &str` parameter.

**Lifetime concern:** `PlaywrightBridge` must outlive the pipeline execution. The bridge (owning the child process) needs to be kept alive until the command finishes. Options:
- Return `PlaywrightBridge` alongside the page and keep it in the caller
- Store it in a `Box<dyn Any>` alongside the Arc<dyn IPage>
- Use a wrapper that holds both

Decision: return a tuple `(Arc<dyn IPage>, Option<PlaywrightBridge>)` from `connect()`. Caller holds both until done.

### 4. Auth command: `autocli auth youtube`

New subcommand in `crates/autocli-cli/src/main.rs`.

```rust
// In the match for subcommands:
"auth" => {
    if let Some(site) = matches.get_one::<String>("site") {
        let auth_path = home.join(format!(".autocli/auth/{site}.json"));
        // Launch interactive Playwright browser for login
        let status = std::process::Command::new("npx")
            .args(["playwright", "open", 
                   &format!("--save-storage={}", auth_path.display()),
                   &format!("https://{}.com", site)])
            .status()?;
        if status.success() {
            println!("Auth saved to {}", auth_path.display());
        }
    }
}
```

This opens an interactive (visible) Chromium window where the user logs in. When they close the window, Playwright saves cookies + localStorage to the JSON file. This is a one-time operation.

## Data Flow: YouTube Transcript

```
1. autocli youtube transcript "URL"
2. Adapter has strategy: cookie, browser: true
3. main.rs: needs browser → calls BrowserBridge::connect("youtube")
4. BrowserBridge: ~/.autocli/auth/youtube.json exists
5.   → PlaywrightBridge::launch(auth_path)
6.     → node scripts/playwright-bridge.mjs --storage=~/.autocli/auth/youtube.json
7.     → headless Chromium starts with YouTube cookies loaded
8.     → stdout: "CDP:ws://127.0.0.1:54321/devtools/browser/abc123"
9.   → CdpPage::connect("ws://127.0.0.1:54321/devtools/browser/abc123")
10. Pipeline step 1 (evaluate): extract video ID → CdpPage.evaluate() → CDP Runtime.evaluate
11. Pipeline step 2 (navigate): youtube.com/watch?v=ID → CdpPage.goto() → CDP Page.navigate
12. Pipeline step 3 (wait 3s): tokio::sleep
13. Pipeline step 4 (evaluate): InnerTube API call with credentials → CdpPage.evaluate()
    - fetch() with credentials: 'include' uses Chromium's cookie jar
    - YouTube cookies loaded from storageState at launch
    - Returns transcript segments
14. Output: JSON/table with timestamp, speaker, text
15. Command finishes → PlaywrightBridge dropped → child killed → Chromium exits
```

## Error Handling

| Scenario | Behavior |
|----------|----------|
| No auth state file | Error: "Not authenticated. Run `autocli auth youtube` first." |
| Node.js not installed | Error: "Node.js is required for browser automation. Install from nodejs.org" |
| Playwright not installed | Error: "Playwright is required. Run `npm install -g playwright`" |
| CDP connection fails | Error with timeout details |
| Auth state expired | YouTube returns 401 → adapter JS throws → suggest re-running `autocli auth youtube` |
| Playwright script crash | Child process exit detected → error propagated |

## Prerequisites (user-facing)

One-time setup:
```bash
# 1. Install Playwright + Chromium
npm install -g playwright
npx playwright install chromium

# 2. Login to YouTube
autocli auth youtube
# → browser opens → log in → close window
```

After that, all YouTube commands work headlessly:
```bash
autocli youtube transcript "VIDEO_URL" -f json  # no window opens
```

## Files Changed

| File | Change |
|------|--------|
| `scripts/playwright-bridge.mjs` | **New.** Node.js helper script |
| `crates/autocli-browser/src/bridge.rs` | Add `PlaywrightBridge` struct + Playwright path in `connect()` |
| `crates/autocli-cli/src/main.rs` | Add `autocli auth <site>` subcommand |
| `package.json` (root, optional) | Add playwright as devDependency for script |

## Testing

- Unit: `PlaywrightBridge::launch()` with mock script that prints fake CDP endpoint
- Integration: `autocli youtube transcript <known-video> -f json` after auth
- Error paths: no auth file, no node, script crash

## Out of Scope

- Replacing daemon/extension for other sites
- Multi-site auth management
- Playwright browser management CLI (install, update, etc.)
- Cookie refresh/rotation
