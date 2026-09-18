//! Every compiled-tier runtime helper has a bytecode-VM counterpart, or an
//! entry saying how the VM reaches the same behaviour. A helper with neither
//! is a compiled-only feature.

use gossamer_interp::coverage::{VM_NATIVE_EXEMPT, exemption_for, vm_counterpart};

#[test]
fn every_runtime_helper_is_reachable_from_the_vm() {
    let uncovered: Vec<&str> = gossamer_abi::REGISTRY
        .iter()
        .map(|entry| entry.name)
        .filter(|name| vm_counterpart(name).is_none() && exemption_for(name).is_none())
        .collect();
    assert!(
        uncovered.is_empty(),
        "compiled-only runtime helpers (add a VM builtin, or an entry in \
         VM_NATIVE_EXEMPT saying how the VM reaches the behaviour): {uncovered:?}"
    );
}

#[test]
fn every_exemption_answers_for_a_helper_with_no_builtin() {
    for (pattern, _) in VM_NATIVE_EXEMPT {
        let answers = gossamer_abi::REGISTRY.iter().any(|entry| {
            vm_counterpart(entry.name).is_none()
                && exemption_for(entry.name).is_some_and(|(p, _)| p == pattern)
        });
        assert!(
            answers,
            "exemption {pattern} answers for no uncovered helper; remove it"
        );
    }
}

#[test]
fn every_exemption_gives_a_mechanism() {
    for (pattern, reason) in VM_NATIVE_EXEMPT {
        assert!(reason.len() > 40, "exemption {pattern} needs a real reason");
        let lower = reason.to_lowercase();
        for evasion in ["not implemented", "todo", "later", "unsupported"] {
            assert!(
                !lower.contains(evasion),
                "exemption {pattern} records a gap instead of a mechanism: {reason}"
            );
        }
    }
}

#[test]
fn a_same_named_builtin_is_found() {
    assert_eq!(
        vm_counterpart("gos_rt_fs_read_to_string"),
        Some("fs::read_to_string")
    );
    assert_eq!(vm_counterpart("gos_rt_not_a_helper"), None);
}
