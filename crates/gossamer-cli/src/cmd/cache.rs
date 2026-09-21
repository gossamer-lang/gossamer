//! Cache inspection and retention commands.

use anyhow::Result;
use gossamer_driver::cache_maintenance::{self, CacheClass, CachePolicy, CacheScope};

pub(crate) fn status(paths_only: bool, scope: CacheScope) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let entries = cache_maintenance::status(&cwd, scope);
    if paths_only {
        for entry in entries {
            outln!("{}\t{}", entry.class.name(), entry.path.display());
        }
        return Ok(());
    }
    let mut total = 0;
    for entry in entries {
        total += entry.bytes;
        outln!(
            "{:<10} {:>12}  {:>8} files  {}",
            entry.class.name(),
            human_bytes(entry.bytes),
            entry.files,
            entry.path.display()
        );
    }
    outln!("total      {:>12}", human_bytes(total));
    Ok(())
}

pub(crate) fn prune(scope: CacheScope, dry_run: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let policy = CachePolicy::default();
    let (bytes, files) = cache_maintenance::prune(&cwd, policy, scope, dry_run)?;
    let verb = if dry_run {
        "would reclaim"
    } else {
        "reclaimed"
    };
    outln!(
        "cache prune ({}): {verb} {} from {files} files (cap={}, max-age={} days)",
        scope.name(),
        human_bytes(bytes),
        human_bytes(policy.max_bytes),
        policy.max_age.as_secs() / 86_400
    );
    Ok(())
}

/// Removes every cache class known to the toolchain within `scope`, without
/// touching project build outputs or vendored dependencies.
pub(crate) fn clear(scope: CacheScope, dry_run: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let removed = cache_maintenance::remove(&cwd, CacheClass::all(), scope, dry_run)?;
    let bytes = removed.iter().map(|entry| entry.bytes).sum::<u64>();
    let verb = if dry_run { "would remove" } else { "removed" };
    outln!(
        "cache clear ({}): {verb} {} from {} cache roots",
        scope.name(),
        human_bytes(bytes),
        removed.len()
    );
    Ok(())
}

/// Formats a byte count in compact base-1024 units, matching the convention
/// used by tools such as `df -h`.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "K", "M", "G", "T", "P", "E"];

    if bytes < 1024 {
        return format!("{bytes}B");
    }

    let mut unit = 0;
    let mut divisor = 1_u128;
    while unit + 1 < UNITS.len() && u128::from(bytes) >= divisor * 1024 {
        unit += 1;
        divisor *= 1024;
    }

    let mut tenths = (u128::from(bytes) * 10 + divisor / 2) / divisor;
    if tenths >= 10 * 1024 && unit + 1 < UNITS.len() {
        unit += 1;
        divisor *= 1024;
        tenths = (u128::from(bytes) * 10 + divisor / 2) / divisor;
    }
    if tenths < 100 {
        format!("{}.{}{}", tenths / 10, tenths % 10, UNITS[unit])
    } else {
        let rounded = (u128::from(bytes) + divisor / 2) / divisor;
        format!("{rounded}{}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

    #[test]
    fn human_bytes_formats_base_1024_boundaries() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(1023), "1023B");
        assert_eq!(human_bytes(1024), "1.0K");
        assert_eq!(human_bytes(1536), "1.5K");
        assert_eq!(human_bytes(10 * 1024), "10K");
        assert_eq!(human_bytes(1024 * 1024 - 1), "1.0M");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0M");
        assert_eq!(human_bytes(u64::MAX), "16E");
    }
}
