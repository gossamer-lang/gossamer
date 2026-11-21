//! `cargo xtask sbom OUT`: the release SBOM, built from the crate graph of
//! what the release archives ship (`gos` and `libgossamer_runtime.a`), not
//! from every manifest in the repository.

use std::path::Path;

use anyhow::Result;
use sbom_attest::{CargoOptions, GenerateOptions};

/// Workspace packages whose artifacts go into the release archives.
pub(crate) const SHIPPED: &[&str] = &["gossamer-cli", "gossamer-runtime"];

/// CycloneDX 1.6 SBOM for the shipped packages of the workspace at `root`.
pub(crate) fn generate(root: &Path) -> Result<serde_json::Value> {
    let mut cargo = CargoOptions::new(root.join("Cargo.toml"));
    cargo.packages = SHIPPED.iter().map(|p| (*p).to_string()).collect();
    cargo.locked = true;
    let options = GenerateOptions {
        cargo: Some(cargo),
        ..Default::default()
    }
    .with_producer("Gossamer contributors");
    Ok(sbom_attest::to_cyclonedx(&sbom_attest::generate(&options)?))
}

/// Write the release SBOM to `out`.
pub(crate) fn export(root: &Path, out: &Path) -> Result<()> {
    sbom_attest::write(out, &sbom_attest::pretty(&generate(root)?))?;
    println!("xtask: wrote {}", out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    #[test]
    fn release_sbom_lists_each_shipped_crate_once() {
        let root = crate::locate_workspace_root().unwrap();
        let sbom = super::generate(&root).unwrap();
        let components = sbom["components"].as_array().unwrap();
        let mut seen = BTreeSet::new();
        for c in components {
            let key = (c["name"].as_str().unwrap(), c["version"].as_str());
            assert!(seen.insert(key), "duplicate component {key:?}");
        }
        let names: BTreeSet<&str> = components
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        // Tooling-only crates are not in the archives.
        for not_shipped in ["xtask", "serde-reflection", "gossamer-playground"] {
            assert!(!names.contains(not_shipped), "{not_shipped} is not shipped");
        }
        assert!(names.contains("clap"));
    }
}
