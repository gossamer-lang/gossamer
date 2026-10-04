//! The foreign-function rule the builds this process runs follow; see
//! [`gossamer_types::ForeignPolicy`].

use gossamer_types::ForeignPolicy;
use parking_lot::RwLock;

static POLICY: RwLock<ForeignPolicy> = RwLock::new(ForeignPolicy::Ungoverned);

/// Sets the rule for the builds this process runs. The CLI calls this once
/// it knows which project a command works in.
pub fn set_foreign_policy(policy: ForeignPolicy) {
    *POLICY.write() = policy;
}

/// The rule in force.
#[must_use]
pub fn foreign_policy() -> ForeignPolicy {
    POLICY.read().clone()
}
