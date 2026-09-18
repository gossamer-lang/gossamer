//! The generated symbol table answers for a name the runtime defines and
//! declines one it does not.

#[test]
fn table_resolves_a_known_runtime_symbol() {
    let addr = gossamer_runtime::symbols::address("gos_rt_len");
    assert!(
        addr.is_some(),
        "gos_rt_len missing from the generated table"
    );
}

#[test]
fn table_declines_an_unknown_symbol() {
    assert!(gossamer_runtime::symbols::address("gos_rt_not_a_symbol").is_none());
}

#[test]
fn table_names_are_unique_and_sorted() {
    let names: Vec<&str> = gossamer_runtime::symbols::names().collect();
    assert!(
        names.windows(2).all(|pair| pair[0] < pair[1]),
        "generated table is not strictly sorted, so a name repeats or lookup misses"
    );
}

#[test]
fn every_entry_resolves_to_its_own_address() {
    for (name, addr) in gossamer_runtime::symbols::entries() {
        assert_eq!(
            gossamer_runtime::symbols::address(name),
            Some(addr),
            "{name}"
        );
        assert!(!addr.is_null(), "{name} has a null address");
    }
}

#[test]
fn macro_generated_and_non_c_abi_exports_are_present() {
    for name in [
        "gos_rt_bin_put_u16_be",
        "gos_rt_fs_open_options_read",
        "gos_rt_iter_scan_i64",
        "gos_rt_iter_take_while_ptr",
        "gos_rt_preempt_check",
    ] {
        assert!(
            gossamer_runtime::symbols::address(name).is_some(),
            "{name} missing from the generated table"
        );
    }
}
