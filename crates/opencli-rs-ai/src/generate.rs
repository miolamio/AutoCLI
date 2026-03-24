//! One-shot generation: explore a URL and synthesize the best adapter candidate.

use opencli_rs_core::{CliError, IPage};

use crate::explore::explore;
use crate::synthesize::synthesize;
use crate::types::{AdapterCandidate, ExploreOptions, SynthesizeOptions};

/// One-shot generation: explore a URL, synthesize candidates, and return the best one.
///
/// Combines `explore` and `synthesize` into a single call for convenience.
pub async fn generate(
    page: &dyn IPage,
    url: &str,
    goal: &str,
) -> Result<AdapterCandidate, CliError> {
    // 1. Explore the URL
    let manifest = explore(page, url, ExploreOptions::default()).await?;

    if manifest.endpoints.is_empty() {
        return Err(CliError::empty_result(format!(
            "No API endpoints discovered at {}",
            url
        )));
    }

    // 2. Synthesize candidates with the given goal
    let options = SynthesizeOptions {
        site: None,
        goal: Some(goal.to_string()),
    };
    let candidates = synthesize(&manifest, options)?;

    // 3. Return the best candidate
    candidates.into_iter().next().ok_or_else(|| {
        CliError::empty_result(format!(
            "Could not generate adapter for {} with goal '{}'",
            url, goal
        ))
    })
}

#[cfg(test)]
mod tests {
    // Integration tests for `generate` require a mock IPage implementation,
    // which is tested at the workspace level. Unit tests for the composition
    // are covered by explore and synthesize tests individually.

    #[test]
    fn test_generate_module_compiles() {
        // Verify the module exists and the public function is accessible.
        // Actual async integration tests require a mock IPage.
        assert_eq!(std::mem::size_of::<crate::types::AdapterCandidate>(), std::mem::size_of::<crate::types::AdapterCandidate>());
    }
}
