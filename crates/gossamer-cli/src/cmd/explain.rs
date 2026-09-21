//! `gos explain CODE` describes any registered diagnostic or lint code.

use anyhow::{Result, anyhow};

/// Entry point for `gos explain CODE`.
pub(crate) fn run(code: &str) -> Result<()> {
    let upper = code.to_ascii_uppercase();
    let text = gossamer_diagnostics::explain(&upper).ok_or_else(|| {
        anyhow!(
            "no explanation registered for `{upper}`. See docs/diagnostics.md for the code catalogue."
        )
    })?;
    outln!("{upper}\n\n{text}");
    Ok(())
}
