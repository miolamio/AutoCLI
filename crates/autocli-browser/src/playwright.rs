use autocli_core::{CliError, IPage};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tracing::{debug, info};

use crate::cdp::CdpPage;

const STARTUP_TIMEOUT_SECS: u64 = 15;

/// Manages a Node.js child process running playwright-bridge.mjs.
/// The bridge launches headless Chromium via Playwright and exposes a raw CDP endpoint.
pub struct PlaywrightBridge {
    child: tokio::process::Child,
}

impl PlaywrightBridge {
    /// Launch the Playwright bridge with the given auth state file.
    ///
    /// 1. Finds `scripts/playwright-bridge.mjs` relative to the executable, cwd, or project root.
    /// 2. Spawns: `node <script> --storage=<auth_state>`
    /// 3. Reads the first stdout line, expecting `CDP:ws://...`
    /// 4. Connects a CdpPage to that endpoint.
    /// 5. Returns `(page, bridge)` where bridge must outlive the pipeline.
    pub async fn launch(auth_state: &Path) -> Result<(Arc<dyn IPage>, Self), CliError> {
        let script = find_bridge_script()?;
        info!(script = %script.display(), "Launching Playwright bridge");

        let mut child = tokio::process::Command::new("node")
            .arg(&script)
            .arg(format!("--storage={}", auth_state.display()))
            .stdin(std::process::Stdio::piped()) // keep stdin open so bridge stays alive
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| CliError::BrowserConnect {
                message: format!("Failed to spawn Playwright bridge: {e}"),
                suggestions: vec![
                    "Make sure Node.js is installed and in PATH".into(),
                    "Make sure Playwright is installed: npm install playwright".into(),
                ],
                source: None,
            })?;

        // Read first line from stdout (CDP endpoint)
        let stdout = child.stdout.take().ok_or_else(|| {
            CliError::browser_connect("Failed to capture Playwright bridge stdout")
        })?;

        let endpoint = match tokio::time::timeout(
            std::time::Duration::from_secs(STARTUP_TIMEOUT_SECS),
            read_cdp_endpoint(stdout),
        )
        .await
        {
            Ok(Ok(ep)) => ep,
            Ok(Err(e)) => {
                let stderr_msg = read_child_stderr(&mut child).await;
                let detail = if stderr_msg.is_empty() {
                    e.to_string()
                } else {
                    format!("{e}\nBridge stderr: {stderr_msg}")
                };
                return Err(CliError::browser_connect(detail));
            }
            Err(_) => {
                let stderr_msg = read_child_stderr(&mut child).await;
                let detail = if stderr_msg.is_empty() {
                    format!("Playwright bridge did not produce CDP endpoint within {STARTUP_TIMEOUT_SECS}s")
                } else {
                    format!("Playwright bridge timed out ({STARTUP_TIMEOUT_SECS}s). Bridge stderr: {stderr_msg}")
                };
                let _ = child.start_kill();
                return Err(CliError::Timeout {
                    message: detail,
                    suggestions: vec![
                        "Make sure Playwright browsers are installed: npx playwright install chromium".into(),
                    ],
                });
            }
        };

        info!(endpoint = %endpoint, "Playwright bridge ready, connecting CdpPage");
        let page = CdpPage::connect(&endpoint).await?;

        Ok((Arc::new(page), PlaywrightBridge { child }))
    }
}

impl Drop for PlaywrightBridge {
    fn drop(&mut self) {
        // Best-effort kill of the child process
        debug!("Dropping PlaywrightBridge, killing child process");
        let _ = self.child.start_kill();
    }
}

/// Read stderr from the child process (best-effort, for error reporting).
async fn read_child_stderr(child: &mut tokio::process::Child) -> String {
    if let Some(stderr) = child.stderr.take() {
        let mut buf = String::new();
        let mut reader = tokio::io::BufReader::new(stderr);
        // Read up to 4KB of stderr
        match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            async {
                use tokio::io::AsyncReadExt;
                let mut tmp = vec![0u8; 4096];
                if let Ok(n) = reader.read(&mut tmp).await {
                    buf = String::from_utf8_lossy(&tmp[..n]).to_string();
                }
            },
        )
        .await
        {
            _ => {}
        }
        buf.trim().to_string()
    } else {
        String::new()
    }
}

/// Read lines from the bridge's stdout until we find one starting with `CDP:`.
async fn read_cdp_endpoint(
    stdout: tokio::process::ChildStdout,
) -> Result<String, CliError> {
    let reader = tokio::io::BufReader::new(stdout);
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await.map_err(|e| {
        CliError::browser_connect(format!("Failed to read from Playwright bridge: {e}"))
    })? {
        debug!(line = %line, "Playwright bridge stdout");
        if let Some(endpoint) = line.strip_prefix("CDP:") {
            return Ok(endpoint.to_string());
        }
    }

    Err(CliError::browser_connect(
        "Playwright bridge exited without producing a CDP endpoint",
    ))
}

/// Locate the `playwright-bridge.mjs` script by searching several paths.
fn find_bridge_script() -> Result<PathBuf, CliError> {
    // 1. Relative to the current executable
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let candidate = exe_dir.join("scripts").join("playwright-bridge.mjs");
            if candidate.exists() {
                return Ok(candidate);
            }
            // Also check one level up (common for target/debug or target/release)
            if let Some(parent) = exe_dir.parent() {
                let candidate = parent.join("scripts").join("playwright-bridge.mjs");
                if candidate.exists() {
                    return Ok(candidate);
                }
                // Two levels up (target/debug -> target -> project root)
                if let Some(grandparent) = parent.parent() {
                    let candidate = grandparent.join("scripts").join("playwright-bridge.mjs");
                    if candidate.exists() {
                        return Ok(candidate);
                    }
                }
            }
        }
    }

    // 2. Relative to working directory
    let cwd_candidate = PathBuf::from("scripts/playwright-bridge.mjs");
    if cwd_candidate.exists() {
        return Ok(cwd_candidate.canonicalize().unwrap_or(cwd_candidate));
    }

    // 3. Check CARGO_MANIFEST_DIR for development
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let candidate = PathBuf::from(manifest_dir)
            .join("../../scripts/playwright-bridge.mjs");
        if candidate.exists() {
            return Ok(candidate.canonicalize().unwrap_or(candidate));
        }
    }

    Err(CliError::BrowserConnect {
        message: "Cannot find scripts/playwright-bridge.mjs".into(),
        suggestions: vec![
            "Make sure the script exists in the scripts/ directory of the project".into(),
        ],
        source: None,
    })
}

/// Compute the auth state file path for a given site.
/// Returns `~/.autocli/auth/{site}.json`.
pub fn auth_state_path(site: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".autocli").join("auth").join(format!("{site}.json")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_auth_state_path() {
        // auth_state_path should produce a path ending in .autocli/auth/{site}.json
        let path = auth_state_path("youtube");
        assert!(path.is_some());
        let p = path.unwrap();
        assert!(p.to_str().unwrap().contains(".autocli/auth/youtube.json"));
    }

    #[test]
    fn test_auth_state_path_different_sites() {
        let yt = auth_state_path("youtube").unwrap();
        let tw = auth_state_path("twitter").unwrap();
        assert_ne!(yt, tw);
        assert!(yt.to_str().unwrap().ends_with("youtube.json"));
        assert!(tw.to_str().unwrap().ends_with("twitter.json"));
    }

    #[tokio::test]
    async fn test_parse_cdp_endpoint_from_stdout() {
        // Create a mock script that prints a fake CDP endpoint
        let mut script = NamedTempFile::new().unwrap();
        writeln!(
            script,
            r#"#!/usr/bin/env node
console.log("CDP:ws://127.0.0.1:9333/devtools/page/ABC123");
// Keep alive briefly
setTimeout(() => {{}}, 2000);
"#
        )
        .unwrap();

        let script_path = script.path().to_path_buf();

        let mut child = tokio::process::Command::new("node")
            .arg(&script_path)
            .stdout(std::process::Stdio::piped())
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("node must be available for tests");

        let stdout = child.stdout.take().unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_cdp_endpoint(stdout),
        )
        .await;

        let _ = child.start_kill();

        let endpoint = result
            .expect("should not time out")
            .expect("should parse endpoint");
        assert_eq!(endpoint, "ws://127.0.0.1:9333/devtools/page/ABC123");
    }

    #[tokio::test]
    async fn test_parse_cdp_endpoint_with_prefix_noise() {
        // Script that prints some noise before the CDP line
        let mut script = NamedTempFile::new().unwrap();
        writeln!(
            script,
            r#"#!/usr/bin/env node
console.log("Launching browser...");
console.log("Debug info here");
console.log("CDP:ws://127.0.0.1:9444/devtools/page/XYZ789");
setTimeout(() => {{}}, 2000);
"#
        )
        .unwrap();

        let script_path = script.path().to_path_buf();

        let mut child = tokio::process::Command::new("node")
            .arg(&script_path)
            .stdout(std::process::Stdio::piped())
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("node must be available");

        let stdout = child.stdout.take().unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_cdp_endpoint(stdout),
        )
        .await;

        let _ = child.start_kill();

        let endpoint = result
            .expect("should not time out")
            .expect("should parse endpoint");
        assert_eq!(endpoint, "ws://127.0.0.1:9444/devtools/page/XYZ789");
    }

    #[tokio::test]
    async fn test_bridge_fails_without_cdp_line() {
        // Script that exits without printing CDP:
        let mut script = NamedTempFile::new().unwrap();
        writeln!(
            script,
            r#"#!/usr/bin/env node
console.log("no cdp line here");
process.exit(0);
"#
        )
        .unwrap();

        let script_path = script.path().to_path_buf();

        let mut child = tokio::process::Command::new("node")
            .arg(&script_path)
            .stdout(std::process::Stdio::piped())
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("node must be available");

        let stdout = child.stdout.take().unwrap();
        let result = read_cdp_endpoint(stdout).await;

        let _ = child.start_kill();

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("without producing a CDP endpoint"),
            "Unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_timeout_when_no_cdp_line_produced() {
        // Script that hangs forever without producing a CDP line
        let mut script = NamedTempFile::new().unwrap();
        writeln!(
            script,
            r#"#!/usr/bin/env node
console.log("Starting up...");
// Hang forever
setInterval(() => {{}}, 60000);
"#
        )
        .unwrap();

        let script_path = script.path().to_path_buf();

        let mut child = tokio::process::Command::new("node")
            .arg(&script_path)
            .stdout(std::process::Stdio::piped())
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("node must be available");

        let stdout = child.stdout.take().unwrap();
        // Use a short timeout (2s) for the test
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            read_cdp_endpoint(stdout),
        )
        .await;

        let _ = child.start_kill();

        // Should have timed out
        assert!(result.is_err(), "Expected timeout, got: {:?}", result);
    }

    #[test]
    fn test_find_bridge_script_from_cwd() {
        // This test only passes when run from the project root (where scripts/ exists)
        let result = find_bridge_script();
        // We just verify it doesn't panic; it may or may not find the file depending on cwd
        if let Ok(path) = result {
            assert!(path.to_str().unwrap().contains("playwright-bridge.mjs"));
        }
    }
}
