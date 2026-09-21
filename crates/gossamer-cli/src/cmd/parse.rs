//! `gos parse FILE` - pretty-prints the AST for the supplied source.

use std::path::Path;

use anyhow::{Result, anyhow};

use crate::paths::read_source;

/// Entry point for `gos parse FILE`.
pub(crate) fn run(file: &Path) -> Result<()> {
    let source = read_source(file)?;
    let mut map = gossamer_lex::SourceMap::new();
    let file_id = map.add_file(file.to_string_lossy().into_owned(), source.clone());
    let (sf, diags) = gossamer_parse::parse_source_file(&source, file_id);
    if !diags.is_empty() {
        let render_opts = gossamer_diagnostics::RenderOptions {
            colour: crate::paths::stderr_supports_colour(),
        };
        for diag in &diags {
            let structured = diag.to_diagnostic();
            eprintln!(
                "{}",
                gossamer_diagnostics::render(&structured, &map, render_opts)
            );
        }
        return Err(anyhow!("{} parse error(s)", diags.len()));
    }
    outln!("{sf}");
    Ok(())
}
