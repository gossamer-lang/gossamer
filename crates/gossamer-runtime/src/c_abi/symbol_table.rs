//! Address table: every `gos_rt_*` runtime symbol mapped to its
//! in-process function pointer. Generated from (and kept complete
//! against) the `gossamer-abi` registry by `symbol_table_covers_registry`
//! so the Cranelift JIT can resolve any runtime call without a
//! hand-curated list that silently drifts behind the registry.

/// Returns `(symbol_name, function_address)` for every `gos_rt_*`
/// runtime symbol the compiled tiers may call. The Cranelift JIT
/// registers these against its builder at module-build time.
#[must_use]
pub fn runtime_symbol_addrs() -> Vec<(&'static str, *const u8)> {
    vec![
        (
            "gos_rt_aggr_alloc",
            crate::c_abi::gos_rt_aggr_alloc as *const u8,
        ),
        (
            "gos_rt_aggr_alloc_leak",
            crate::c_abi::gos_rt_aggr_alloc_leak as *const u8,
        ),
        (
            "gos_rt_aggr_free",
            crate::c_abi::gos_rt_aggr_free as *const u8,
        ),
        (
            "gos_rt_arena_restore",
            crate::c_abi::gos_rt_arena_restore as *const u8,
        ),
        (
            "gos_rt_arena_save",
            crate::c_abi::gos_rt_arena_save as *const u8,
        ),
        (
            "gos_rt_arr_format_arr_bool",
            crate::c_abi::gos_rt_arr_format_arr_bool as *const u8,
        ),
        (
            "gos_rt_arr_format_arr_f64",
            crate::c_abi::gos_rt_arr_format_arr_f64 as *const u8,
        ),
        (
            "gos_rt_arr_format_arr_i64",
            crate::c_abi::gos_rt_arr_format_arr_i64 as *const u8,
        ),
        (
            "gos_rt_arr_format_adt",
            crate::c_abi::gos_rt_arr_format_adt as *const u8,
        ),
        (
            "gos_rt_arr_format_bool",
            crate::c_abi::gos_rt_arr_format_bool as *const u8,
        ),
        (
            "gos_rt_arr_format_char",
            crate::c_abi::gos_rt_arr_format_char as *const u8,
        ),
        (
            "gos_rt_arr_format_f64",
            crate::c_abi::gos_rt_arr_format_f64 as *const u8,
        ),
        (
            "gos_rt_arr_format_i64",
            crate::c_abi::gos_rt_arr_format_i64 as *const u8,
        ),
        (
            "gos_rt_arr_format_string",
            crate::c_abi::gos_rt_arr_format_string as *const u8,
        ),
        (
            "gos_rt_arr_format_u8",
            crate::c_abi::gos_rt_arr_format_u8 as *const u8,
        ),
        (
            "gos_rt_arr_iter",
            crate::c_abi::gos_rt_arr_iter as *const u8,
        ),
        (
            "gos_rt_arr_iter_free",
            crate::c_abi::gos_rt_arr_iter_free as *const u8,
        ),
        (
            "gos_rt_arr_iter_next",
            crate::c_abi::gos_rt_arr_iter_next as *const u8,
        ),
        ("gos_rt_arr_len", crate::c_abi::gos_rt_arr_len as *const u8),
        (
            "gos_rt_arr_reverse",
            crate::c_abi::gos_rt_arr_reverse as *const u8,
        ),
        (
            "gos_rt_arr_sort_by_aggr",
            crate::c_abi::gos_rt_arr_sort_by_aggr as *const u8,
        ),
        (
            "gos_rt_arr_sort_by_f64",
            crate::c_abi::gos_rt_arr_sort_by_f64 as *const u8,
        ),
        (
            "gos_rt_arr_sort_by_i64",
            crate::c_abi::gos_rt_arr_sort_by_i64 as *const u8,
        ),
        (
            "gos_rt_arr_sort_i64",
            crate::c_abi::gos_rt_arr_sort_i64 as *const u8,
        ),
        (
            "gos_rt_arr_sort_str",
            crate::c_abi::gos_rt_arr_sort_str as *const u8,
        ),
        (
            "gos_rt_arr_sort_tuple",
            crate::c_abi::gos_rt_arr_sort_tuple as *const u8,
        ),
        (
            "gos_rt_atomic_bool_cas",
            crate::c_abi::gos_rt_atomic_bool_cas as *const u8,
        ),
        (
            "gos_rt_atomic_bool_load",
            crate::c_abi::gos_rt_atomic_bool_load as *const u8,
        ),
        (
            "gos_rt_atomic_bool_new",
            crate::c_abi::gos_rt_atomic_bool_new as *const u8,
        ),
        (
            "gos_rt_atomic_bool_store",
            crate::c_abi::gos_rt_atomic_bool_store as *const u8,
        ),
        (
            "gos_rt_atomic_i64_cas",
            crate::c_abi::gos_rt_atomic_i64_cas as *const u8,
        ),
        (
            "gos_rt_atomic_i64_cas_acq_rel",
            crate::c_abi::gos_rt_atomic_i64_cas_acq_rel as *const u8,
        ),
        (
            "gos_rt_atomic_i64_fetch_sub",
            crate::c_abi::gos_rt_atomic_i64_fetch_sub as *const u8,
        ),
        (
            "gos_rt_atomic_i64_fetch_add",
            crate::c_abi::gos_rt_atomic_i64_fetch_add as *const u8,
        ),
        (
            "gos_rt_atomic_i64_fetch_add_acqrel",
            crate::c_abi::gos_rt_atomic_i64_fetch_add_acqrel as *const u8,
        ),
        (
            "gos_rt_atomic_i64_load",
            crate::c_abi::gos_rt_atomic_i64_load as *const u8,
        ),
        (
            "gos_rt_atomic_i64_load_acquire",
            crate::c_abi::gos_rt_atomic_i64_load_acquire as *const u8,
        ),
        (
            "gos_rt_atomic_i64_load_relaxed",
            crate::c_abi::gos_rt_atomic_i64_load_relaxed as *const u8,
        ),
        (
            "gos_rt_atomic_i64_new",
            crate::c_abi::gos_rt_atomic_i64_new as *const u8,
        ),
        (
            "gos_rt_atomic_i64_store",
            crate::c_abi::gos_rt_atomic_i64_store as *const u8,
        ),
        (
            "gos_rt_atomic_i64_store_relaxed",
            crate::c_abi::gos_rt_atomic_i64_store_relaxed as *const u8,
        ),
        (
            "gos_rt_atomic_i64_store_release",
            crate::c_abi::gos_rt_atomic_i64_store_release as *const u8,
        ),
        (
            "gos_rt_atomic_i64_swap",
            crate::c_abi::gos_rt_atomic_i64_swap as *const u8,
        ),
        (
            "gos_rt_barrier_new",
            crate::c_abi::gos_rt_barrier_new as *const u8,
        ),
        (
            "gos_rt_barrier_wait",
            crate::c_abi::gos_rt_barrier_wait as *const u8,
        ),
        (
            "gos_rt_bheap_len",
            crate::c_abi::gos_rt_bheap_len as *const u8,
        ),
        (
            "gos_rt_bheap_is_empty",
            crate::c_abi::gos_rt_bheap_is_empty as *const u8,
        ),
        (
            "gos_rt_bheap_clear",
            crate::c_abi::gos_rt_bheap_clear as *const u8,
        ),
        (
            "gos_rt_bheap_max_format",
            crate::c_abi::gos_rt_bheap_max_format as *const u8,
        ),
        (
            "gos_rt_bheap_min_format",
            crate::c_abi::gos_rt_bheap_min_format as *const u8,
        ),
        (
            "gos_rt_bheap_max_new_i64",
            crate::c_abi::gos_rt_bheap_max_new_i64 as *const u8,
        ),
        (
            "gos_rt_bheap_max_from_vec_i64",
            crate::c_abi::gos_rt_bheap_max_from_vec_i64 as *const u8,
        ),
        (
            "gos_rt_bheap_max_from_vec_f64",
            crate::c_abi::gos_rt_bheap_max_from_vec_f64 as *const u8,
        ),
        (
            "gos_rt_bheap_max_push_i64",
            crate::c_abi::gos_rt_bheap_max_push_i64 as *const u8,
        ),
        (
            "gos_rt_bheap_max_push_f64",
            crate::c_abi::gos_rt_bheap_max_push_f64 as *const u8,
        ),
        (
            "gos_rt_bheap_max_pop_i64",
            crate::c_abi::gos_rt_bheap_max_pop_i64 as *const u8,
        ),
        (
            "gos_rt_bheap_max_pop_f64",
            crate::c_abi::gos_rt_bheap_max_pop_f64 as *const u8,
        ),
        (
            "gos_rt_bheap_max_peek_i64",
            crate::c_abi::gos_rt_bheap_max_peek_i64 as *const u8,
        ),
        (
            "gos_rt_bheap_min_new_i64",
            crate::c_abi::gos_rt_bheap_min_new_i64 as *const u8,
        ),
        (
            "gos_rt_bheap_min_from_vec_i64",
            crate::c_abi::gos_rt_bheap_min_from_vec_i64 as *const u8,
        ),
        (
            "gos_rt_bheap_min_from_vec_f64",
            crate::c_abi::gos_rt_bheap_min_from_vec_f64 as *const u8,
        ),
        (
            "gos_rt_bheap_min_push_i64",
            crate::c_abi::gos_rt_bheap_min_push_i64 as *const u8,
        ),
        (
            "gos_rt_bheap_min_push_f64",
            crate::c_abi::gos_rt_bheap_min_push_f64 as *const u8,
        ),
        (
            "gos_rt_bheap_min_pop_i64",
            crate::c_abi::gos_rt_bheap_min_pop_i64 as *const u8,
        ),
        (
            "gos_rt_bheap_min_pop_f64",
            crate::c_abi::gos_rt_bheap_min_pop_f64 as *const u8,
        ),
        (
            "gos_rt_bheap_min_peek_i64",
            crate::c_abi::gos_rt_bheap_min_peek_i64 as *const u8,
        ),
        (
            "gos_rt_binding_bytes_from_vec",
            crate::c_abi::gos_rt_binding_bytes_from_vec as *const u8,
        ),
        (
            "gos_rt_binding_bytes_to_vec",
            crate::c_abi::gos_rt_binding_bytes_to_vec as *const u8,
        ),
        (
            "gos_rt_binding_map_free",
            crate::c_abi::gos_rt_binding_map_free as *const u8,
        ),
        (
            "gos_rt_binding_map_from_map",
            crate::c_abi::gos_rt_binding_map_from_map as *const u8,
        ),
        (
            "gos_rt_binding_map_to_map",
            crate::c_abi::gos_rt_binding_map_to_map as *const u8,
        ),
        (
            "gos_rt_binding_struct_from_slots",
            crate::c_abi::gos_rt_binding_struct_from_slots as *const u8,
        ),
        (
            "gos_rt_binding_struct_to_slots",
            crate::c_abi::gos_rt_binding_struct_to_slots as *const u8,
        ),
        (
            "gos_rt_binding_tuple_from_slots",
            crate::c_abi::gos_rt_binding_tuple_from_slots as *const u8,
        ),
        (
            "gos_rt_binding_tuple_to_slots",
            crate::c_abi::gos_rt_binding_tuple_to_slots as *const u8,
        ),
        (
            "gos_rt_binding_variant_to_result",
            crate::c_abi::gos_rt_binding_variant_to_result as *const u8,
        ),
        (
            "gos_rt_bin_get_u8",
            crate::c_abi::gos_rt_bin_get_u8 as *const u8,
        ),
        (
            "gos_rt_bin_get_u16_be",
            crate::c_abi::gos_rt_bin_get_u16_be as *const u8,
        ),
        (
            "gos_rt_bin_get_u16_le",
            crate::c_abi::gos_rt_bin_get_u16_le as *const u8,
        ),
        (
            "gos_rt_bin_get_u32_be",
            crate::c_abi::gos_rt_bin_get_u32_be as *const u8,
        ),
        (
            "gos_rt_bin_get_u32_le",
            crate::c_abi::gos_rt_bin_get_u32_le as *const u8,
        ),
        (
            "gos_rt_bin_get_u64_be",
            crate::c_abi::gos_rt_bin_get_u64_be as *const u8,
        ),
        (
            "gos_rt_bin_get_u64_le",
            crate::c_abi::gos_rt_bin_get_u64_le as *const u8,
        ),
        (
            "gos_rt_bin_put_u8",
            crate::c_abi::gos_rt_bin_put_u8 as *const u8,
        ),
        (
            "gos_rt_bin_put_u16_be",
            crate::c_abi::gos_rt_bin_put_u16_be as *const u8,
        ),
        (
            "gos_rt_bin_put_u16_le",
            crate::c_abi::gos_rt_bin_put_u16_le as *const u8,
        ),
        (
            "gos_rt_bin_put_u32_be",
            crate::c_abi::gos_rt_bin_put_u32_be as *const u8,
        ),
        (
            "gos_rt_bin_put_u32_le",
            crate::c_abi::gos_rt_bin_put_u32_le as *const u8,
        ),
        (
            "gos_rt_bin_put_u64_be",
            crate::c_abi::gos_rt_bin_put_u64_be as *const u8,
        ),
        (
            "gos_rt_bin_put_u64_le",
            crate::c_abi::gos_rt_bin_put_u64_le as *const u8,
        ),
        (
            "gos_rt_bin_uvarint",
            crate::c_abi::gos_rt_bin_uvarint as *const u8,
        ),
        (
            "gos_rt_bin_varint",
            crate::c_abi::gos_rt_bin_varint as *const u8,
        ),
        (
            "gos_rt_bin_put_uvarint",
            crate::c_abi::gos_rt_bin_put_uvarint as *const u8,
        ),
        (
            "gos_rt_bin_get_u16_be_at",
            crate::c_abi::gos_rt_bin_get_u16_be_at as *const u8,
        ),
        (
            "gos_rt_bin_put_u16_be_at",
            crate::c_abi::gos_rt_bin_put_u16_be_at as *const u8,
        ),
        (
            "gos_rt_bin_get_u16_le_at",
            crate::c_abi::gos_rt_bin_get_u16_le_at as *const u8,
        ),
        (
            "gos_rt_bin_put_u16_le_at",
            crate::c_abi::gos_rt_bin_put_u16_le_at as *const u8,
        ),
        (
            "gos_rt_bin_get_u32_be_at",
            crate::c_abi::gos_rt_bin_get_u32_be_at as *const u8,
        ),
        (
            "gos_rt_bin_put_u32_be_at",
            crate::c_abi::gos_rt_bin_put_u32_be_at as *const u8,
        ),
        (
            "gos_rt_bin_get_u32_le_at",
            crate::c_abi::gos_rt_bin_get_u32_le_at as *const u8,
        ),
        (
            "gos_rt_bin_put_u32_le_at",
            crate::c_abi::gos_rt_bin_put_u32_le_at as *const u8,
        ),
        (
            "gos_rt_bin_get_u64_be_at",
            crate::c_abi::gos_rt_bin_get_u64_be_at as *const u8,
        ),
        (
            "gos_rt_bin_put_u64_be_at",
            crate::c_abi::gos_rt_bin_put_u64_be_at as *const u8,
        ),
        (
            "gos_rt_bin_get_u64_le_at",
            crate::c_abi::gos_rt_bin_get_u64_le_at as *const u8,
        ),
        (
            "gos_rt_bin_put_u64_le_at",
            crate::c_abi::gos_rt_bin_put_u64_le_at as *const u8,
        ),
        (
            "gos_rt_bin_put_varint",
            crate::c_abi::gos_rt_bin_put_varint as *const u8,
        ),
        (
            "gos_rt_bits_add",
            crate::c_abi::gos_rt_bits_add as *const u8,
        ),
        (
            "gos_rt_bits_count_ones",
            crate::c_abi::gos_rt_bits_count_ones as *const u8,
        ),
        (
            "gos_rt_bits_count_zeros",
            crate::c_abi::gos_rt_bits_count_zeros as *const u8,
        ),
        (
            "gos_rt_bits_div",
            crate::c_abi::gos_rt_bits_div as *const u8,
        ),
        (
            "gos_rt_bits_leading_zeros",
            crate::c_abi::gos_rt_bits_leading_zeros as *const u8,
        ),
        (
            "gos_rt_bits_len",
            crate::c_abi::gos_rt_bits_len as *const u8,
        ),
        (
            "gos_rt_bits_mul",
            crate::c_abi::gos_rt_bits_mul as *const u8,
        ),
        (
            "gos_rt_bits_reverse_bits",
            crate::c_abi::gos_rt_bits_reverse_bits as *const u8,
        ),
        (
            "gos_rt_bits_reverse_bytes",
            crate::c_abi::gos_rt_bits_reverse_bytes as *const u8,
        ),
        (
            "gos_rt_bits_rotate_left",
            crate::c_abi::gos_rt_bits_rotate_left as *const u8,
        ),
        (
            "gos_rt_bits_rotate_right",
            crate::c_abi::gos_rt_bits_rotate_right as *const u8,
        ),
        (
            "gos_rt_bits_sub",
            crate::c_abi::gos_rt_bits_sub as *const u8,
        ),
        (
            "gos_rt_bits_trailing_zeros",
            crate::c_abi::gos_rt_bits_trailing_zeros as *const u8,
        ),
        (
            "gos_rt_blake3_hex",
            crate::c_abi::gos_rt_blake3_hex as *const u8,
        ),
        (
            "gos_rt_bool_to_str",
            crate::c_abi::gos_rt_bool_to_str as *const u8,
        ),
        (
            "gos_rt_deque_assign",
            crate::c_abi::deque::gos_rt_deque_assign as *const u8,
        ),
        (
            "gos_rt_deque_clear",
            crate::c_abi::deque::gos_rt_deque_clear as *const u8,
        ),
        (
            "gos_rt_deque_clone",
            crate::c_abi::deque::gos_rt_deque_clone as *const u8,
        ),
        (
            "gos_rt_queue_clone",
            crate::c_abi::deque::gos_rt_queue_clone as *const u8,
        ),
        (
            "gos_rt_stack_clone",
            crate::c_abi::deque::gos_rt_stack_clone as *const u8,
        ),
        (
            "gos_rt_deque_free",
            crate::c_abi::deque::gos_rt_deque_free as *const u8,
        ),
        (
            "gos_rt_deque_is_empty",
            crate::c_abi::deque::gos_rt_deque_is_empty as *const u8,
        ),
        (
            "gos_rt_set_is_empty",
            crate::c_abi::set::gos_rt_set_is_empty as *const u8,
        ),
        (
            "gos_rt_deque_len",
            crate::c_abi::deque::gos_rt_deque_len as *const u8,
        ),
        (
            "gos_rt_deque_format",
            crate::c_abi::deque::gos_rt_deque_format as *const u8,
        ),
        (
            "gos_rt_queue_format",
            crate::c_abi::deque::gos_rt_queue_format as *const u8,
        ),
        (
            "gos_rt_stack_format",
            crate::c_abi::deque::gos_rt_stack_format as *const u8,
        ),
        (
            "gos_rt_deque_new",
            crate::c_abi::deque::gos_rt_deque_new as *const u8,
        ),
        (
            "gos_rt_deque_from_vec_i64",
            crate::c_abi::deque::gos_rt_deque_from_vec_i64 as *const u8,
        ),
        (
            "gos_rt_queue_new",
            crate::c_abi::deque::gos_rt_queue_new as *const u8,
        ),
        (
            "gos_rt_queue_from_vec_i64",
            crate::c_abi::deque::gos_rt_queue_from_vec_i64 as *const u8,
        ),
        (
            "gos_rt_stack_new",
            crate::c_abi::deque::gos_rt_stack_new as *const u8,
        ),
        (
            "gos_rt_stack_from_vec_i64",
            crate::c_abi::deque::gos_rt_stack_from_vec_i64 as *const u8,
        ),
        (
            "gos_rt_deque_pop_front",
            crate::c_abi::deque::gos_rt_deque_pop_front as *const u8,
        ),
        (
            "gos_rt_deque_pop_back",
            crate::c_abi::deque::gos_rt_deque_pop_back as *const u8,
        ),
        (
            "gos_rt_deque_pop_front_into",
            crate::c_abi::deque::gos_rt_deque_pop_front_into as *const u8,
        ),
        (
            "gos_rt_deque_pop_back_into",
            crate::c_abi::deque::gos_rt_deque_pop_back_into as *const u8,
        ),
        (
            "gos_rt_deque_peek_front",
            crate::c_abi::deque::gos_rt_deque_peek_front as *const u8,
        ),
        (
            "gos_rt_deque_peek_back",
            crate::c_abi::deque::gos_rt_deque_peek_back as *const u8,
        ),
        (
            "gos_rt_deque_push_back",
            crate::c_abi::deque::gos_rt_deque_push_back as *const u8,
        ),
        (
            "gos_rt_bheap_max_format_desc",
            crate::c_abi::container_heap::gos_rt_bheap_max_format_desc as *const u8,
        ),
        (
            "gos_rt_bheap_max_from_vec_desc",
            crate::c_abi::container_heap::gos_rt_bheap_max_from_vec_desc as *const u8,
        ),
        (
            "gos_rt_bheap_max_pop_desc",
            crate::c_abi::container_heap::gos_rt_bheap_max_pop_desc as *const u8,
        ),
        (
            "gos_rt_bheap_max_pop_desc_into",
            crate::c_abi::container_heap::gos_rt_bheap_max_pop_desc_into as *const u8,
        ),
        (
            "gos_rt_bheap_min_pop_desc_into",
            crate::c_abi::container_heap::gos_rt_bheap_min_pop_desc_into as *const u8,
        ),
        (
            "gos_rt_bheap_max_push_desc",
            crate::c_abi::container_heap::gos_rt_bheap_max_push_desc as *const u8,
        ),
        (
            "gos_rt_bheap_min_format_desc",
            crate::c_abi::container_heap::gos_rt_bheap_min_format_desc as *const u8,
        ),
        (
            "gos_rt_bheap_min_from_vec_desc",
            crate::c_abi::container_heap::gos_rt_bheap_min_from_vec_desc as *const u8,
        ),
        (
            "gos_rt_bheap_min_pop_desc",
            crate::c_abi::container_heap::gos_rt_bheap_min_pop_desc as *const u8,
        ),
        (
            "gos_rt_bheap_min_push_desc",
            crate::c_abi::container_heap::gos_rt_bheap_min_push_desc as *const u8,
        ),
        (
            "gos_rt_bheap_new_typed",
            crate::c_abi::container_heap::gos_rt_bheap_new_typed as *const u8,
        ),
        (
            "gos_rt_bheap_peek_elem",
            crate::c_abi::container_heap::gos_rt_bheap_peek_elem as *const u8,
        ),
        (
            "gos_rt_set_format_tagged",
            crate::c_abi::set::gos_rt_set_format_tagged as *const u8,
        ),
        (
            "gos_rt_set_insert_ekey",
            crate::c_abi::set::gos_rt_set_insert_ekey as *const u8,
        ),
        (
            "gos_rt_set_contains_ekey",
            crate::c_abi::set::gos_rt_set_contains_ekey as *const u8,
        ),
        (
            "gos_rt_set_remove_ekey",
            crate::c_abi::set::gos_rt_set_remove_ekey as *const u8,
        ),
        (
            "gos_rt_set_to_vec_ekey",
            crate::c_abi::set::gos_rt_set_to_vec_ekey as *const u8,
        ),
        (
            "gos_rt_set_format_ekey",
            crate::c_abi::set::gos_rt_set_format_ekey as *const u8,
        ),
        ("gos_rt_dyn_nil", crate::c_abi::gos_rt_dyn_nil as *const u8),
        (
            "gos_rt_dyn_bool",
            crate::c_abi::gos_rt_dyn_bool as *const u8,
        ),
        ("gos_rt_dyn_int", crate::c_abi::gos_rt_dyn_int as *const u8),
        (
            "gos_rt_dyn_float",
            crate::c_abi::gos_rt_dyn_float as *const u8,
        ),
        (
            "gos_rt_dyn_char",
            crate::c_abi::gos_rt_dyn_char as *const u8,
        ),
        (
            "gos_rt_dyn_string",
            crate::c_abi::gos_rt_dyn_string as *const u8,
        ),
        (
            "gos_rt_dyn_bytes",
            crate::c_abi::gos_rt_dyn_bytes as *const u8,
        ),
        (
            "gos_rt_dyn_list",
            crate::c_abi::gos_rt_dyn_list as *const u8,
        ),
        ("gos_rt_dyn_map", crate::c_abi::gos_rt_dyn_map as *const u8),
        (
            "gos_rt_dyn_tagged",
            crate::c_abi::gos_rt_dyn_tagged as *const u8,
        ),
        (
            "gos_rt_dyn_kind",
            crate::c_abi::gos_rt_dyn_kind as *const u8,
        ),
        (
            "gos_rt_dyn_name",
            crate::c_abi::gos_rt_dyn_name as *const u8,
        ),
        (
            "gos_rt_dyn_kind_name",
            crate::c_abi::gos_rt_dyn_kind_name as *const u8,
        ),
        ("gos_rt_dyn_len", crate::c_abi::gos_rt_dyn_len as *const u8),
        ("gos_rt_dyn_at", crate::c_abi::gos_rt_dyn_at as *const u8),
        (
            "gos_rt_dyn_arm_index",
            crate::c_abi::gos_rt_dyn_arm_index as *const u8,
        ),
        (
            "gos_rt_dyn_field_i64",
            crate::c_abi::gos_rt_dyn_field_i64 as *const u8,
        ),
        (
            "gos_rt_dyn_field_f64",
            crate::c_abi::gos_rt_dyn_field_f64 as *const u8,
        ),
        (
            "gos_rt_dyn_field_str",
            crate::c_abi::gos_rt_dyn_field_str as *const u8,
        ),
        (
            "gos_rt_dyn_field_dyn",
            crate::c_abi::gos_rt_dyn_field_dyn as *const u8,
        ),
        (
            "gos_rt_dyn_key_at",
            crate::c_abi::gos_rt_dyn_key_at as *const u8,
        ),
        (
            "gos_rt_dyn_as_i64",
            crate::c_abi::gos_rt_dyn_as_i64 as *const u8,
        ),
        (
            "gos_rt_dyn_as_f64",
            crate::c_abi::gos_rt_dyn_as_f64 as *const u8,
        ),
        (
            "gos_rt_dyn_as_bool",
            crate::c_abi::gos_rt_dyn_as_bool as *const u8,
        ),
        (
            "gos_rt_dyn_as_char",
            crate::c_abi::gos_rt_dyn_as_char as *const u8,
        ),
        (
            "gos_rt_dyn_as_str",
            crate::c_abi::gos_rt_dyn_as_str as *const u8,
        ),
        (
            "gos_rt_dyn_as_bytes",
            crate::c_abi::gos_rt_dyn_as_bytes as *const u8,
        ),
        (
            "gos_rt_dyn_clone",
            crate::c_abi::gos_rt_dyn_clone as *const u8,
        ),
        (
            "gos_rt_dyn_free",
            crate::c_abi::gos_rt_dyn_free as *const u8,
        ),
        ("gos_rt_dyn_eq", crate::c_abi::gos_rt_dyn_eq as *const u8),
        (
            "gos_rt_dyn_format",
            crate::c_abi::gos_rt_dyn_format as *const u8,
        ),
        (
            "gos_rt_dyn_from_binding_variant",
            crate::c_abi::gos_rt_dyn_from_binding_variant as *const u8,
        ),
        (
            "gos_rt_desc_cmp",
            crate::c_abi::desc_cmp::gos_rt_desc_cmp as *const u8,
        ),
        (
            "gos_rt_deque_push_back_wide",
            crate::c_abi::deque::gos_rt_deque_push_back_wide as *const u8,
        ),
        (
            "gos_rt_deque_push_front_wide",
            crate::c_abi::deque::gos_rt_deque_push_front_wide as *const u8,
        ),
        (
            "gos_rt_deque_new_typed",
            crate::c_abi::deque::gos_rt_deque_new_typed as *const u8,
        ),
        (
            "gos_rt_deque_from_vec",
            crate::c_abi::deque::gos_rt_deque_from_vec as *const u8,
        ),
        (
            "gos_rt_deque_vec",
            crate::c_abi::deque::gos_rt_deque_vec as *const u8,
        ),
        (
            "gos_rt_deque_format_desc",
            crate::c_abi::deque::gos_rt_deque_format_desc as *const u8,
        ),
        (
            "gos_rt_queue_format_desc",
            crate::c_abi::deque::gos_rt_queue_format_desc as *const u8,
        ),
        (
            "gos_rt_stack_format_desc",
            crate::c_abi::deque::gos_rt_stack_format_desc as *const u8,
        ),
        (
            "gos_rt_deque_push_back_f64",
            crate::c_abi::deque::gos_rt_deque_push_back_f64 as *const u8,
        ),
        (
            "gos_rt_deque_push_front",
            crate::c_abi::deque::gos_rt_deque_push_front as *const u8,
        ),
        (
            "gos_rt_deque_push_front_f64",
            crate::c_abi::deque::gos_rt_deque_push_front_f64 as *const u8,
        ),
        (
            "gos_rt_bufio_read_lines_of",
            crate::c_abi::gos_rt_bufio_read_lines_of as *const u8,
        ),
        (
            "gos_rt_bufio_read_to_string",
            crate::c_abi::gos_rt_bufio_read_to_string as *const u8,
        ),
        (
            "gos_rt_bufio_scanner_new",
            crate::c_abi::gos_rt_bufio_scanner_new as *const u8,
        ),
        (
            "gos_rt_bufio_scanner_scan",
            crate::c_abi::gos_rt_bufio_scanner_scan as *const u8,
        ),
        (
            "gos_rt_bufio_scanner_text",
            crate::c_abi::gos_rt_bufio_scanner_text as *const u8,
        ),
        (
            "gos_rt_bytes_buffer_clear",
            crate::c_abi::gos_rt_bytes_buffer_clear as *const u8,
        ),
        (
            "gos_rt_bytes_buffer_is_empty",
            crate::c_abi::gos_rt_bytes_buffer_is_empty as *const u8,
        ),
        (
            "gos_rt_bytes_buffer_len",
            crate::c_abi::gos_rt_bytes_buffer_len as *const u8,
        ),
        (
            "gos_rt_bytes_buffer_new",
            crate::c_abi::gos_rt_bytes_buffer_new as *const u8,
        ),
        (
            "gos_rt_bytes_buffer_push",
            crate::c_abi::gos_rt_bytes_buffer_push as *const u8,
        ),
        (
            "gos_rt_bytes_buffer_to_string",
            crate::c_abi::gos_rt_bytes_buffer_to_string as *const u8,
        ),
        (
            "gos_rt_bytes_buffer_with_capacity",
            crate::c_abi::gos_rt_bytes_buffer_with_capacity as *const u8,
        ),
        (
            "gos_rt_bytes_buffer_write_str",
            crate::c_abi::gos_rt_bytes_buffer_write_str as *const u8,
        ),
        (
            "gos_rt_bytes_builder_as_str",
            crate::c_abi::gos_rt_bytes_builder_as_str as *const u8,
        ),
        (
            "gos_rt_bytes_builder_build",
            crate::c_abi::gos_rt_bytes_builder_build as *const u8,
        ),
        (
            "gos_rt_bytes_builder_len",
            crate::c_abi::gos_rt_bytes_builder_len as *const u8,
        ),
        (
            "gos_rt_bytes_builder_new",
            crate::c_abi::gos_rt_bytes_builder_new as *const u8,
        ),
        (
            "gos_rt_bytes_builder_with_capacity",
            crate::c_abi::gos_rt_bytes_builder_with_capacity as *const u8,
        ),
        (
            "gos_rt_bytes_builder_write",
            crate::c_abi::gos_rt_bytes_builder_write as *const u8,
        ),
        (
            "gos_rt_bytes_builder_write_char",
            crate::c_abi::gos_rt_bytes_builder_write_char as *const u8,
        ),
        (
            "gos_rt_bytes_index_of",
            crate::c_abi::gos_rt_bytes_index_of as *const u8,
        ),
        (
            "gos_rt_bytes_replace",
            crate::c_abi::gos_rt_bytes_replace as *const u8,
        ),
        (
            "gos_rt_bytes_split",
            crate::c_abi::gos_rt_bytes_split as *const u8,
        ),
        (
            "gos_rt_callback_invoke",
            crate::c_abi::gos_rt_callback_invoke as *const u8,
        ),
        (
            "gos_rt_callback_register",
            crate::c_abi::gos_rt_callback_register as *const u8,
        ),
        (
            "gos_rt_callback_unregister",
            crate::c_abi::gos_rt_callback_unregister as *const u8,
        ),
        (
            "gos_rt_chan_close",
            crate::c_abi::gos_rt_chan_close as *const u8,
        ),
        (
            "gos_rt_chan_drop",
            crate::c_abi::gos_rt_chan_drop as *const u8,
        ),
        (
            "gos_rt_chan_new",
            crate::c_abi::gos_rt_chan_new as *const u8,
        ),
        (
            "gos_rt_chan_recv",
            crate::c_abi::gos_rt_chan_recv as *const u8,
        ),
        (
            "gos_rt_chan_recv_ctx_option",
            crate::c_abi::gos_rt_chan_recv_ctx_option as *const u8,
        ),
        (
            "gos_rt_chan_recv_option",
            crate::c_abi::gos_rt_chan_recv_option as *const u8,
        ),
        (
            "gos_rt_chan_send",
            crate::c_abi::gos_rt_chan_send as *const u8,
        ),
        (
            "gos_rt_chan_set_elem_kind",
            crate::c_abi::gos_rt_chan_set_elem_kind as *const u8,
        ),
        (
            "gos_rt_chan_set_elem_desc",
            crate::c_abi::gos_rt_chan_set_elem_desc as *const u8,
        ),
        (
            "gos_rt_result_unwrap_or_str",
            crate::c_abi::vec::gos_rt_result_unwrap_or_str as *const u8,
        ),
        (
            "gos_rt_result_ok_payload_release",
            crate::c_abi::vec::gos_rt_result_ok_payload_release as *const u8,
        ),
        (
            "gos_rt_chan_try_recv",
            crate::c_abi::gos_rt_chan_try_recv as *const u8,
        ),
        (
            "gos_rt_chan_try_recv_option",
            crate::c_abi::gos_rt_chan_try_recv_option as *const u8,
        ),
        (
            "gos_rt_chan_try_send",
            crate::c_abi::gos_rt_chan_try_send as *const u8,
        ),
        (
            "gos_rt_char_to_str",
            crate::c_abi::gos_rt_char_to_str as *const u8,
        ),
        (
            "gos_rt_child_close_stdin",
            crate::c_abi::gos_rt_child_close_stdin as *const u8,
        ),
        (
            "gos_rt_child_kill",
            crate::c_abi::gos_rt_child_kill as *const u8,
        ),
        (
            "gos_rt_child_read_line",
            crate::c_abi::gos_rt_child_read_line as *const u8,
        ),
        (
            "gos_rt_child_read_stdout",
            crate::c_abi::gos_rt_child_read_stdout as *const u8,
        ),
        (
            "gos_rt_child_wait",
            crate::c_abi::gos_rt_child_wait as *const u8,
        ),
        (
            "gos_rt_child_write_stdin",
            crate::c_abi::gos_rt_child_write_stdin as *const u8,
        ),
        (
            "gos_rt_chunked_decode",
            crate::c_abi::gos_rt_chunked_decode as *const u8,
        ),
        (
            "gos_rt_chunked_encode",
            crate::c_abi::gos_rt_chunked_encode as *const u8,
        ),
        (
            "gos_rt_clamp_f64",
            crate::c_abi::gos_rt_clamp_f64 as *const u8,
        ),
        (
            "gos_rt_clamp_i64",
            crate::c_abi::gos_rt_clamp_i64 as *const u8,
        ),
        (
            "gos_rt_collect_cycles",
            crate::c_abi::gos_rt_collect_cycles as *const u8,
        ),
        (
            "gos_rt_compress_bzip2_compress",
            crate::c_abi::gos_rt_compress_bzip2_compress as *const u8,
        ),
        (
            "gos_rt_compress_bzip2_decompress",
            crate::c_abi::gos_rt_compress_bzip2_decompress as *const u8,
        ),
        (
            "gos_rt_compress_flate_compress",
            crate::c_abi::gos_rt_compress_flate_compress as *const u8,
        ),
        (
            "gos_rt_compress_flate_decompress",
            crate::c_abi::gos_rt_compress_flate_decompress as *const u8,
        ),
        (
            "gos_rt_compress_gzip_decode",
            crate::c_abi::gos_rt_compress_gzip_decode as *const u8,
        ),
        (
            "gos_rt_compress_gzip_encode",
            crate::c_abi::gos_rt_compress_gzip_encode as *const u8,
        ),
        (
            "gos_rt_compress_zlib_compress",
            crate::c_abi::gos_rt_compress_zlib_compress as *const u8,
        ),
        (
            "gos_rt_compress_zlib_decompress",
            crate::c_abi::gos_rt_compress_zlib_decompress as *const u8,
        ),
        (
            "gos_rt_compress_zstd_decode",
            crate::c_abi::gos_rt_compress_zstd_decode as *const u8,
        ),
        (
            "gos_rt_compress_zstd_encode",
            crate::c_abi::gos_rt_compress_zstd_encode as *const u8,
        ),
        (
            "gos_rt_compress_zstd_encode_level",
            crate::c_abi::gos_rt_compress_zstd_encode_level as *const u8,
        ),
        (
            "gos_rt_concat_bool",
            crate::c_abi::gos_rt_concat_bool as *const u8,
        ),
        (
            "gos_rt_concat_char",
            crate::c_abi::gos_rt_concat_char as *const u8,
        ),
        (
            "gos_rt_concat_f64",
            crate::c_abi::gos_rt_concat_f64 as *const u8,
        ),
        (
            "gos_rt_concat_f64_debug",
            crate::c_abi::gos_rt_concat_f64_debug as *const u8,
        ),
        (
            "gos_rt_concat_f64_prec",
            crate::c_abi::gos_rt_concat_f64_prec as *const u8,
        ),
        (
            "gos_rt_concat_finish",
            crate::c_abi::gos_rt_concat_finish as *const u8,
        ),
        (
            "gos_rt_concat_i64",
            crate::c_abi::gos_rt_concat_i64 as *const u8,
        ),
        (
            "gos_rt_concat_init",
            crate::c_abi::gos_rt_concat_init as *const u8,
        ),
        (
            "gos_rt_concat_str",
            crate::c_abi::gos_rt_concat_str as *const u8,
        ),
        (
            "gos_rt_concat_u64",
            crate::c_abi::gos_rt_concat_u64 as *const u8,
        ),
        (
            "gos_rt_cov_bump",
            crate::c_abi::gos_rt_cov_bump as *const u8,
        ),
        (
            "gos_rt_cov_record",
            crate::c_abi::gos_rt_cov_record as *const u8,
        ),
        (
            "gos_rt_cov_reset",
            crate::c_abi::gos_rt_cov_reset as *const u8,
        ),
        (
            "gos_rt_cov_set_enabled",
            crate::c_abi::gos_rt_cov_set_enabled as *const u8,
        ),
        (
            "gos_rt_crypto_aes256gcm_open",
            crate::c_abi::gos_rt_crypto_aes256gcm_open as *const u8,
        ),
        (
            "gos_rt_crypto_aes256gcm_seal",
            crate::c_abi::gos_rt_crypto_aes256gcm_seal as *const u8,
        ),
        (
            "gos_rt_crypto_argon2id_hash",
            crate::c_abi::gos_rt_crypto_argon2id_hash as *const u8,
        ),
        (
            "gos_rt_crypto_argon2id_verify",
            crate::c_abi::gos_rt_crypto_argon2id_verify as *const u8,
        ),
        (
            "gos_rt_crypto_chacha20poly1305_open",
            crate::c_abi::gos_rt_crypto_chacha20poly1305_open as *const u8,
        ),
        (
            "gos_rt_crypto_chacha20poly1305_seal",
            crate::c_abi::gos_rt_crypto_chacha20poly1305_seal as *const u8,
        ),
        (
            "gos_rt_crypto_ecdsa_keypair_pem",
            crate::c_abi::gos_rt_crypto_ecdsa_keypair_pem as *const u8,
        ),
        (
            "gos_rt_crypto_ecdsa_sign_pem",
            crate::c_abi::gos_rt_crypto_ecdsa_sign_pem as *const u8,
        ),
        (
            "gos_rt_crypto_ecdsa_verify_pem",
            crate::c_abi::gos_rt_crypto_ecdsa_verify_pem as *const u8,
        ),
        (
            "gos_rt_crypto_ed25519_keypair",
            crate::c_abi::gos_rt_crypto_ed25519_keypair as *const u8,
        ),
        (
            "gos_rt_crypto_ed25519_sign",
            crate::c_abi::gos_rt_crypto_ed25519_sign as *const u8,
        ),
        (
            "gos_rt_crypto_ed25519_verify",
            crate::c_abi::gos_rt_crypto_ed25519_verify as *const u8,
        ),
        (
            "gos_rt_crypto_hmac_sha256_mac",
            crate::c_abi::gos_rt_crypto_hmac_sha256_mac as *const u8,
        ),
        (
            "gos_rt_crypto_sha256_digest",
            crate::c_abi::gos_rt_crypto_sha256_digest as *const u8,
        ),
        (
            "gos_rt_crypto_sha512_digest",
            crate::c_abi::gos_rt_crypto_sha512_digest as *const u8,
        ),
        (
            "gos_rt_crypto_blake3_digest",
            crate::c_abi::gos_rt_crypto_blake3_digest as *const u8,
        ),
        (
            "gos_rt_crypto_md5",
            crate::c_abi::gos_rt_crypto_md5 as *const u8,
        ),
        (
            "gos_rt_crypto_md5_hex",
            crate::c_abi::gos_rt_crypto_md5_hex as *const u8,
        ),
        (
            "gos_rt_crypto_password_hash",
            crate::c_abi::gos_rt_crypto_password_hash as *const u8,
        ),
        (
            "gos_rt_crypto_password_needs_rehash",
            crate::c_abi::gos_rt_crypto_password_needs_rehash as *const u8,
        ),
        (
            "gos_rt_crypto_password_verify",
            crate::c_abi::gos_rt_crypto_password_verify as *const u8,
        ),
        (
            "gos_rt_crypto_pbkdf2_sha256",
            crate::c_abi::gos_rt_crypto_pbkdf2_sha256 as *const u8,
        ),
        (
            "gos_rt_crypto_rand_bytes",
            crate::c_abi::gos_rt_crypto_rand_bytes as *const u8,
        ),
        (
            "gos_rt_crypto_scrypt_interactive",
            crate::c_abi::gos_rt_crypto_scrypt_interactive as *const u8,
        ),
        (
            "gos_rt_crypto_sha1",
            crate::c_abi::gos_rt_crypto_sha1 as *const u8,
        ),
        (
            "gos_rt_crypto_sha1_hex",
            crate::c_abi::gos_rt_crypto_sha1_hex as *const u8,
        ),
        (
            "gos_rt_crypto_subtle_ct_eq",
            crate::c_abi::gos_rt_crypto_subtle_ct_eq as *const u8,
        ),
        (
            "gos_rt_csv_parse_line",
            crate::c_abi::gos_rt_csv_parse_line as *const u8,
        ),
        (
            "gos_rt_csv_read",
            crate::c_abi::gos_rt_csv_read as *const u8,
        ),
        (
            "gos_rt_csv_write",
            crate::c_abi::gos_rt_csv_write as *const u8,
        ),
        (
            "gos_rt_ctx_background",
            crate::c_abi::gos_rt_ctx_background as *const u8,
        ),
        (
            "gos_rt_ctx_cancel",
            crate::c_abi::gos_rt_ctx_cancel as *const u8,
        ),
        (
            "gos_rt_ctx_cancelled",
            crate::c_abi::gos_rt_ctx_cancelled as *const u8,
        ),
        (
            "gos_rt_ctx_done",
            crate::c_abi::gos_rt_ctx_done as *const u8,
        ),
        (
            "gos_rt_ctx_is_cancelled",
            crate::c_abi::gos_rt_ctx_is_cancelled as *const u8,
        ),
        (
            "gos_rt_ctx_with_cancel",
            crate::c_abi::gos_rt_ctx_with_cancel as *const u8,
        ),
        (
            "gos_rt_ctx_with_timeout",
            crate::c_abi::gos_rt_ctx_with_timeout as *const u8,
        ),
        (
            "gos_rt_duration_as_micros",
            crate::c_abi::gos_rt_duration_as_micros as *const u8,
        ),
        (
            "gos_rt_duration_as_millis",
            crate::c_abi::gos_rt_duration_as_millis as *const u8,
        ),
        (
            "gos_rt_duration_as_secs",
            crate::c_abi::gos_rt_duration_as_secs as *const u8,
        ),
        (
            "gos_rt_duration_from_micros",
            crate::c_abi::gos_rt_duration_from_micros as *const u8,
        ),
        (
            "gos_rt_duration_from_millis",
            crate::c_abi::gos_rt_duration_from_millis as *const u8,
        ),
        (
            "gos_rt_duration_from_secs",
            crate::c_abi::gos_rt_duration_from_secs as *const u8,
        ),
        (
            "gos_rt_encoding_ascii85_decode",
            crate::c_abi::gos_rt_encoding_ascii85_decode as *const u8,
        ),
        (
            "gos_rt_encoding_ascii85_encode",
            crate::c_abi::gos_rt_encoding_ascii85_encode as *const u8,
        ),
        (
            "gos_rt_encoding_base32_decode",
            crate::c_abi::gos_rt_encoding_base32_decode as *const u8,
        ),
        (
            "gos_rt_encoding_base32_decode_hex",
            crate::c_abi::gos_rt_encoding_base32_decode_hex as *const u8,
        ),
        (
            "gos_rt_encoding_base32_decode_string",
            crate::c_abi::gos_rt_encoding_base32_decode_string as *const u8,
        ),
        (
            "gos_rt_encoding_base32_encode",
            crate::c_abi::gos_rt_encoding_base32_encode as *const u8,
        ),
        (
            "gos_rt_encoding_base32_encode_hex",
            crate::c_abi::gos_rt_encoding_base32_encode_hex as *const u8,
        ),
        (
            "gos_rt_encoding_base32_encode_string",
            crate::c_abi::gos_rt_encoding_base32_encode_string as *const u8,
        ),
        (
            "gos_rt_encoding_base64_decode",
            crate::c_abi::gos_rt_encoding_base64_decode as *const u8,
        ),
        (
            "gos_rt_encoding_base64_encode",
            crate::c_abi::gos_rt_encoding_base64_encode as *const u8,
        ),
        (
            "gos_rt_encoding_hex_decode",
            crate::c_abi::gos_rt_encoding_hex_decode as *const u8,
        ),
        (
            "gos_rt_encoding_hex_encode",
            crate::c_abi::gos_rt_encoding_hex_encode as *const u8,
        ),
        (
            "gos_rt_encoding_xml_escape",
            crate::c_abi::gos_rt_encoding_xml_escape as *const u8,
        ),
        (
            "gos_rt_enum_unit",
            crate::c_abi::gos_rt_enum_unit as *const u8,
        ),
        (
            "gos_rt_env_vars",
            crate::c_abi::gos_rt_env_vars as *const u8,
        ),
        (
            "gos_rt_env_home_dir",
            crate::c_abi::gos_rt_env_home_dir as *const u8,
        ),
        (
            "gos_rt_env_set_current_dir",
            crate::c_abi::gos_rt_env_set_current_dir as *const u8,
        ),
        (
            "gos_rt_env_temp_dir",
            crate::c_abi::gos_rt_env_temp_dir as *const u8,
        ),
        (
            "gos_rt_eprintln",
            crate::c_abi::gos_rt_eprintln as *const u8,
        ),
        (
            "gos_rt_eprint_str",
            crate::c_abi::gos_rt_eprint_str as *const u8,
        ),
        (
            "gos_rt_error_cause",
            crate::c_abi::gos_rt_error_cause as *const u8,
        ),
        (
            "gos_rt_error_chain",
            crate::c_abi::gos_rt_error_chain as *const u8,
        ),
        (
            "gos_rt_error_display",
            crate::c_abi::gos_rt_error_display as *const u8,
        ),
        (
            "gos_rt_error_field",
            crate::c_abi::gos_rt_error_field as *const u8,
        ),
        (
            "gos_rt_error_fields",
            crate::c_abi::gos_rt_error_fields as *const u8,
        ),
        (
            "gos_rt_error_from",
            crate::c_abi::gos_rt_error_from as *const u8,
        ),
        (
            "gos_rt_error_is",
            crate::c_abi::gos_rt_error_is as *const u8,
        ),
        (
            "gos_rt_error_is_sentinel",
            crate::c_abi::gos_rt_error_is_sentinel as *const u8,
        ),
        (
            "gos_rt_error_message",
            crate::c_abi::gos_rt_error_message as *const u8,
        ),
        (
            "gos_rt_error_new",
            crate::c_abi::gos_rt_error_new as *const u8,
        ),
        (
            "gos_rt_error_with_field",
            crate::c_abi::gos_rt_error_with_field as *const u8,
        ),
        (
            "gos_rt_errors_join",
            crate::c_abi::gos_rt_errors_join as *const u8,
        ),
        (
            "gos_rt_errors_join_vec",
            crate::c_abi::gos_rt_errors_join_vec as *const u8,
        ),
        (
            "gos_rt_error_wrap",
            crate::c_abi::gos_rt_error_wrap as *const u8,
        ),
        (
            "gos_rt_exec_kill",
            crate::c_abi::gos_rt_exec_kill as *const u8,
        ),
        (
            "gos_rt_exec_kill_group",
            crate::c_abi::gos_rt_exec_kill_group as *const u8,
        ),
        (
            "gos_rt_exec_pipeline_run",
            crate::c_abi::gos_rt_exec_pipeline_run as *const u8,
        ),
        (
            "gos_rt_exec_run",
            crate::c_abi::gos_rt_exec_run as *const u8,
        ),
        (
            "gos_rt_exec_run_in",
            crate::c_abi::gos_rt_exec_run_in as *const u8,
        ),
        (
            "gos_rt_exec_signal",
            crate::c_abi::gos_rt_exec_signal as *const u8,
        ),
        (
            "gos_rt_exec_spawn",
            crate::c_abi::gos_rt_exec_spawn as *const u8,
        ),
        (
            "gos_rt_exec_spawn_piped",
            crate::c_abi::gos_rt_exec_spawn_piped as *const u8,
        ),
        (
            "gos_rt_exec_wait_timeout",
            crate::c_abi::gos_rt_exec_wait_timeout as *const u8,
        ),
        ("gos_rt_exit", crate::c_abi::gos_rt_exit as *const u8),
        (
            "gos_rt_f32_from_bits",
            crate::c_abi::gos_rt_f32_from_bits as *const u8,
        ),
        (
            "gos_rt_f32_to_bits",
            crate::c_abi::gos_rt_f32_to_bits as *const u8,
        ),
        (
            "gos_rt_f64_from_bits",
            crate::c_abi::gos_rt_f64_from_bits as *const u8,
        ),
        (
            "gos_rt_f64_prec_to_str",
            crate::c_abi::gos_rt_f64_prec_to_str as *const u8,
        ),
        (
            "gos_rt_str_prec_to_str",
            crate::c_abi::gos_rt_str_prec_to_str as *const u8,
        ),
        (
            "gos_rt_f64_to_str",
            crate::c_abi::gos_rt_f64_to_str as *const u8,
        ),
        (
            "gos_rt_f64_to_bits",
            crate::c_abi::gos_rt_f64_to_bits as *const u8,
        ),
        (
            "gos_rt_field_error_code",
            crate::c_abi::gos_rt_field_error_code as *const u8,
        ),
        (
            "gos_rt_field_error_message",
            crate::c_abi::gos_rt_field_error_message as *const u8,
        ),
        (
            "gos_rt_field_error_new",
            crate::c_abi::gos_rt_field_error_new as *const u8,
        ),
        (
            "gos_rt_field_error_path",
            crate::c_abi::gos_rt_field_error_path as *const u8,
        ),
        (
            "gos_rt_file_server_new",
            crate::c_abi::gos_rt_file_server_new as *const u8,
        ),
        (
            "gos_rt_file_server_serve",
            crate::c_abi::gos_rt_file_server_serve as *const u8,
        ),
        (
            "gos_rt_flag_cell_load_bool",
            crate::c_abi::gos_rt_flag_cell_load_bool as *const u8,
        ),
        (
            "gos_rt_flag_cell_load_f64",
            crate::c_abi::gos_rt_flag_cell_load_f64 as *const u8,
        ),
        (
            "gos_rt_flag_cell_load_i64",
            crate::c_abi::gos_rt_flag_cell_load_i64 as *const u8,
        ),
        (
            "gos_rt_flag_cell_load_str",
            crate::c_abi::gos_rt_flag_cell_load_str as *const u8,
        ),
        (
            "gos_rt_flag_cell_load_vec",
            crate::c_abi::gos_rt_flag_cell_load_vec as *const u8,
        ),
        (
            "gos_rt_flag_map_get",
            crate::c_abi::gos_rt_flag_map_get as *const u8,
        ),
        (
            "gos_rt_flag_parse",
            crate::c_abi::gos_rt_flag_parse as *const u8,
        ),
        (
            "gos_rt_flag_set_bool",
            crate::c_abi::gos_rt_flag_set_bool as *const u8,
        ),
        (
            "gos_rt_flag_set_duration",
            crate::c_abi::gos_rt_flag_set_duration as *const u8,
        ),
        (
            "gos_rt_flag_set_float",
            crate::c_abi::gos_rt_flag_set_float as *const u8,
        ),
        (
            "gos_rt_flag_set_int",
            crate::c_abi::gos_rt_flag_set_int as *const u8,
        ),
        (
            "gos_rt_flag_set_new",
            crate::c_abi::gos_rt_flag_set_new as *const u8,
        ),
        (
            "gos_rt_flag_set_parse",
            crate::c_abi::gos_rt_flag_set_parse as *const u8,
        ),
        (
            "gos_rt_flag_set_short",
            crate::c_abi::gos_rt_flag_set_short as *const u8,
        ),
        (
            "gos_rt_flag_set_string",
            crate::c_abi::gos_rt_flag_set_string as *const u8,
        ),
        (
            "gos_rt_flag_set_string_list",
            crate::c_abi::gos_rt_flag_set_string_list as *const u8,
        ),
        (
            "gos_rt_flag_set_uint",
            crate::c_abi::gos_rt_flag_set_uint as *const u8,
        ),
        (
            "gos_rt_flag_set_usage",
            crate::c_abi::gos_rt_flag_set_usage as *const u8,
        ),
        (
            "gos_rt_floatarr_slice_result",
            crate::c_abi::gos_rt_floatarr_slice_result as *const u8,
        ),
        (
            "gos_rt_flush_stdout",
            crate::c_abi::gos_rt_flush_stdout as *const u8,
        ),
        (
            "gos_rt_fmt_radix_i64",
            crate::c_abi::gos_rt_fmt_radix_i64 as *const u8,
        ),
        (
            "gos_rt_fs_canonicalize",
            crate::c_abi::gos_rt_fs_canonicalize as *const u8,
        ),
        ("gos_rt_fs_copy", crate::c_abi::gos_rt_fs_copy as *const u8),
        (
            "gos_rt_fs_create_dir",
            crate::c_abi::gos_rt_fs_create_dir as *const u8,
        ),
        (
            "gos_rt_fs_create_dir_all",
            crate::c_abi::gos_rt_fs_create_dir_all as *const u8,
        ),
        (
            "gos_rt_fs_create_dir_all_mode",
            crate::c_abi::gos_rt_fs_create_dir_all_mode as *const u8,
        ),
        (
            "gos_rt_fs_create_dir_mode",
            crate::c_abi::gos_rt_fs_create_dir_mode as *const u8,
        ),
        (
            "gos_rt_fs_file_close",
            crate::c_abi::gos_rt_fs_file_close as *const u8,
        ),
        (
            "gos_rt_fs_file_create",
            crate::c_abi::gos_rt_fs_file_create as *const u8,
        ),
        (
            "gos_rt_fs_file_flush",
            crate::c_abi::gos_rt_fs_file_flush as *const u8,
        ),
        (
            "gos_rt_fs_file_open",
            crate::c_abi::gos_rt_fs_file_open as *const u8,
        ),
        (
            "gos_rt_fs_file_read",
            crate::c_abi::gos_rt_fs_file_read as *const u8,
        ),
        (
            "gos_rt_fs_file_read_to_string",
            crate::c_abi::gos_rt_fs_file_read_to_string as *const u8,
        ),
        (
            "gos_rt_fs_file_write",
            crate::c_abi::gos_rt_fs_file_write as *const u8,
        ),
        (
            "gos_rt_fs_file_len",
            crate::c_abi::gos_rt_fs_file_len as *const u8,
        ),
        (
            "gos_rt_fs_file_read_at",
            crate::c_abi::gos_rt_fs_file_read_at as *const u8,
        ),
        (
            "gos_rt_fs_file_seek",
            crate::c_abi::gos_rt_fs_file_seek as *const u8,
        ),
        (
            "gos_rt_fs_file_set_len",
            crate::c_abi::gos_rt_fs_file_set_len as *const u8,
        ),
        (
            "gos_rt_fs_file_sync_all",
            crate::c_abi::gos_rt_fs_file_sync_all as *const u8,
        ),
        (
            "gos_rt_fs_file_sync_data",
            crate::c_abi::gos_rt_fs_file_sync_data as *const u8,
        ),
        (
            "gos_rt_fs_file_try_lock_exclusive",
            crate::c_abi::gos_rt_fs_file_try_lock_exclusive as *const u8,
        ),
        (
            "gos_rt_fs_file_try_lock_range",
            crate::c_abi::gos_rt_fs_file_try_lock_range as *const u8,
        ),
        (
            "gos_rt_fs_file_try_lock_shared",
            crate::c_abi::gos_rt_fs_file_try_lock_shared as *const u8,
        ),
        (
            "gos_rt_fs_file_unlock",
            crate::c_abi::gos_rt_fs_file_unlock as *const u8,
        ),
        (
            "gos_rt_fs_file_unlock_range",
            crate::c_abi::gos_rt_fs_file_unlock_range as *const u8,
        ),
        (
            "gos_rt_fs_file_write_at",
            crate::c_abi::gos_rt_fs_file_write_at as *const u8,
        ),
        (
            "gos_rt_fs_file_write_bytes",
            crate::c_abi::gos_rt_fs_file_write_bytes as *const u8,
        ),
        (
            "gos_rt_fs_sync_dir",
            crate::c_abi::gos_rt_fs_sync_dir as *const u8,
        ),
        (
            "gos_rt_fs_temp_dir",
            crate::c_abi::gos_rt_fs_temp_dir as *const u8,
        ),
        (
            "gos_rt_fs_temp_file",
            crate::c_abi::gos_rt_fs_temp_file as *const u8,
        ),
        (
            "gos_rt_fs_list_dir",
            crate::c_abi::gos_rt_fs_list_dir as *const u8,
        ),
        (
            "gos_rt_fs_metadata",
            crate::c_abi::gos_rt_fs_metadata as *const u8,
        ),
        (
            "gos_rt_fs_metadata_raw",
            crate::c_abi::gos_rt_fs_metadata_raw as *const u8,
        ),
        (
            "gos_rt_fs_open_options_append",
            crate::c_abi::gos_rt_fs_open_options_append as *const u8,
        ),
        (
            "gos_rt_fs_open_options_create",
            crate::c_abi::gos_rt_fs_open_options_create as *const u8,
        ),
        (
            "gos_rt_fs_open_options_create_new",
            crate::c_abi::gos_rt_fs_open_options_create_new as *const u8,
        ),
        (
            "gos_rt_fs_open_options_new",
            crate::c_abi::gos_rt_fs_open_options_new as *const u8,
        ),
        (
            "gos_rt_fs_open_options_open",
            crate::c_abi::gos_rt_fs_open_options_open as *const u8,
        ),
        (
            "gos_rt_fs_open_options_read",
            crate::c_abi::gos_rt_fs_open_options_read as *const u8,
        ),
        (
            "gos_rt_fs_open_options_truncate",
            crate::c_abi::gos_rt_fs_open_options_truncate as *const u8,
        ),
        (
            "gos_rt_fs_open_options_write",
            crate::c_abi::gos_rt_fs_open_options_write as *const u8,
        ),
        (
            "gos_rt_fs_read_bytes_result",
            crate::c_abi::gos_rt_fs_read_bytes_result as *const u8,
        ),
        (
            "gos_rt_fs_read_to_string",
            crate::c_abi::gos_rt_fs_read_to_string as *const u8,
        ),
        (
            "gos_rt_fs_read_to_string_result",
            crate::c_abi::gos_rt_fs_read_to_string_result as *const u8,
        ),
        (
            "gos_rt_fs_remove_dir",
            crate::c_abi::gos_rt_fs_remove_dir as *const u8,
        ),
        (
            "gos_rt_fs_rename",
            crate::c_abi::gos_rt_fs_rename as *const u8,
        ),
        (
            "gos_rt_fs_walk_dir",
            crate::c_abi::gos_rt_fs_walk_dir as *const u8,
        ),
        (
            "gos_rt_fs_permissions",
            crate::c_abi::gos_rt_fs_permissions as *const u8,
        ),
        (
            "gos_rt_fs_set_permissions",
            crate::c_abi::gos_rt_fs_set_permissions as *const u8,
        ),
        (
            "gos_rt_fs_write",
            crate::c_abi::gos_rt_fs_write as *const u8,
        ),
        (
            "gos_rt_fs_write_mode",
            crate::c_abi::gos_rt_fs_write_mode as *const u8,
        ),
        (
            "gos_rt_gc_alloc",
            crate::c_abi::gos_rt_gc_alloc as *const u8,
        ),
        (
            "gos_rt_gc_alloc_count",
            crate::c_abi::gos_rt_gc_alloc_count as *const u8,
        ),
        (
            "gos_rt_gc_collect",
            crate::c_abi::gos_rt_gc_collect as *const u8,
        ),
        (
            "gos_rt_gc_deregister",
            crate::c_abi::gos_rt_gc_deregister as *const u8,
        ),
        (
            "gos_rt_gc_reset",
            crate::c_abi::gos_rt_gc_reset as *const u8,
        ),
        (
            "gos_rt_goroutine_panicked",
            crate::c_abi::gos_rt_goroutine_panicked as *const u8,
        ),
        (
            "gos_rt_go_yield",
            crate::c_abi::gos_rt_go_yield as *const u8,
        ),
        (
            "gos_rt_gzip_decode",
            crate::c_abi::gos_rt_gzip_decode as *const u8,
        ),
        (
            "gos_rt_gzip_encode",
            crate::c_abi::gos_rt_gzip_encode as *const u8,
        ),
        (
            "gos_rt_hash_adler32_checksum",
            crate::c_abi::gos_rt_hash_adler32_checksum as *const u8,
        ),
        (
            "gos_rt_hash_adler32_checksum_string",
            crate::c_abi::gos_rt_hash_adler32_checksum_string as *const u8,
        ),
        (
            "gos_rt_hash_adler32_update",
            crate::c_abi::gos_rt_hash_adler32_update as *const u8,
        ),
        (
            "gos_rt_hash_crc32_checksum",
            crate::c_abi::gos_rt_hash_crc32_checksum as *const u8,
        ),
        (
            "gos_rt_hash_crc32_checksum_string",
            crate::c_abi::gos_rt_hash_crc32_checksum_string as *const u8,
        ),
        (
            "gos_rt_hash_crc32_update",
            crate::c_abi::gos_rt_hash_crc32_update as *const u8,
        ),
        (
            "gos_rt_hash_crc32_update_window",
            crate::c_abi::gos_rt_hash_crc32_update_window as *const u8,
        ),
        (
            "gos_rt_hash_fnv32",
            crate::c_abi::gos_rt_hash_fnv32 as *const u8,
        ),
        (
            "gos_rt_hash_fnv64",
            crate::c_abi::gos_rt_hash_fnv64 as *const u8,
        ),
        (
            "gos_rt_hash_fnv_string",
            crate::c_abi::gos_rt_hash_fnv_string as *const u8,
        ),
        (
            "gos_rt_heap_i64_free",
            crate::c_abi::gos_rt_heap_i64_free as *const u8,
        ),
        (
            "gos_rt_heap_i64_get",
            crate::c_abi::gos_rt_heap_i64_get as *const u8,
        ),
        (
            "gos_rt_heap_i64_len",
            crate::c_abi::gos_rt_heap_i64_len as *const u8,
        ),
        (
            "gos_rt_heap_i64_new",
            crate::c_abi::gos_rt_heap_i64_new as *const u8,
        ),
        (
            "gos_rt_heap_i64_set",
            crate::c_abi::gos_rt_heap_i64_set as *const u8,
        ),
        (
            "gos_rt_heap_i64_write_bytes_to_stdout",
            crate::c_abi::gos_rt_heap_i64_write_bytes_to_stdout as *const u8,
        ),
        (
            "gos_rt_heap_i64_write_lines_to_stdout",
            crate::c_abi::gos_rt_heap_i64_write_lines_to_stdout as *const u8,
        ),
        (
            "gos_rt_heap_u8_count_kmers",
            crate::c_abi::gos_rt_heap_u8_count_kmers as *const u8,
        ),
        (
            "gos_rt_heap_u8_count_pairs",
            crate::c_abi::gos_rt_heap_u8_count_pairs as *const u8,
        ),
        (
            "gos_rt_heap_u8_count_singles",
            crate::c_abi::gos_rt_heap_u8_count_singles as *const u8,
        ),
        (
            "gos_rt_heap_u8_free",
            crate::c_abi::gos_rt_heap_u8_free as *const u8,
        ),
        (
            "gos_rt_heap_u8_get",
            crate::c_abi::gos_rt_heap_u8_get as *const u8,
        ),
        (
            "gos_rt_heap_u8_len",
            crate::c_abi::gos_rt_heap_u8_len as *const u8,
        ),
        (
            "gos_rt_heap_u8_new",
            crate::c_abi::gos_rt_heap_u8_new as *const u8,
        ),
        (
            "gos_rt_heap_u8_set",
            crate::c_abi::gos_rt_heap_u8_set as *const u8,
        ),
        (
            "gos_rt_heap_u8_window_key",
            crate::c_abi::gos_rt_heap_u8_window_key as *const u8,
        ),
        (
            "gos_rt_heap_u8_to_string",
            crate::c_abi::gos_rt_heap_u8_to_string as *const u8,
        ),
        (
            "gos_rt_heap_u8_write_bytes_to_stdout",
            crate::c_abi::gos_rt_heap_u8_write_bytes_to_stdout as *const u8,
        ),
        (
            "gos_rt_heap_u8_write_lines_to_stdout",
            crate::c_abi::gos_rt_heap_u8_write_lines_to_stdout as *const u8,
        ),
        (
            "gos_rt_hmac_sha256_hex",
            crate::c_abi::gos_rt_hmac_sha256_hex as *const u8,
        ),
        (
            "gos_rt_html_escape",
            crate::c_abi::gos_rt_html_escape as *const u8,
        ),
        (
            "gos_rt_html_template_render_json",
            crate::c_abi::gos_rt_html_template_render_json as *const u8,
        ),
        (
            "gos_rt_html_unescape",
            crate::c_abi::gos_rt_html_unescape as *const u8,
        ),
        (
            "gos_rt_http2_bind_and_run_h2c",
            crate::c_abi::gos_rt_http2_bind_and_run_h2c as *const u8,
        ),
        (
            "gos_rt_http3_serve",
            crate::c_abi::gos_rt_http3_serve as *const u8,
        ),
        (
            "gos_rt_http_bearer_ok",
            crate::c_abi::gos_rt_http_bearer_ok as *const u8,
        ),
        (
            "gos_rt_http_client_builder_build",
            crate::c_abi::gos_rt_http_client_builder_build as *const u8,
        ),
        (
            "gos_rt_http_client_builder_cookie_jar",
            crate::c_abi::gos_rt_http_client_builder_cookie_jar as *const u8,
        ),
        (
            "gos_rt_http_client_builder_max_redirects",
            crate::c_abi::gos_rt_http_client_builder_max_redirects as *const u8,
        ),
        (
            "gos_rt_http_client_builder_new",
            crate::c_abi::gos_rt_http_client_builder_new as *const u8,
        ),
        (
            "gos_rt_http_client_builder_proxy",
            crate::c_abi::gos_rt_http_client_builder_proxy as *const u8,
        ),
        (
            "gos_rt_http_client_builder_timeout_ms",
            crate::c_abi::gos_rt_http_client_builder_timeout_ms as *const u8,
        ),
        (
            "gos_rt_http_client_delete",
            crate::c_abi::gos_rt_http_client_delete as *const u8,
        ),
        (
            "gos_rt_http_client_get",
            crate::c_abi::gos_rt_http_client_get as *const u8,
        ),
        (
            "gos_rt_http_client_head",
            crate::c_abi::gos_rt_http_client_head as *const u8,
        ),
        (
            "gos_rt_http_client_new",
            crate::c_abi::gos_rt_http_client_new as *const u8,
        ),
        (
            "gos_rt_http_client_options",
            crate::c_abi::gos_rt_http_client_options as *const u8,
        ),
        (
            "gos_rt_http_client_post",
            crate::c_abi::gos_rt_http_client_post as *const u8,
        ),
        (
            "gos_rt_http_client_put",
            crate::c_abi::gos_rt_http_client_put as *const u8,
        ),
        (
            "gos_rt_http_client_request",
            crate::c_abi::gos_rt_http_client_request as *const u8,
        ),
        (
            "gos_rt_http_client_request_bytes",
            crate::c_abi::gos_rt_http_client_request_bytes as *const u8,
        ),
        (
            "gos_rt_http_cookie_parse_header",
            crate::c_abi::gos_rt_http_cookie_parse_header as *const u8,
        ),
        (
            "gos_rt_http_cookie_serialize",
            crate::c_abi::gos_rt_http_cookie_serialize as *const u8,
        ),
        (
            "gos_rt_http_csrf_issue_token",
            crate::c_abi::gos_rt_http_csrf_issue_token as *const u8,
        ),
        (
            "gos_rt_http_csrf_verify_token",
            crate::c_abi::gos_rt_http_csrf_verify_token as *const u8,
        ),
        (
            "gos_rt_http_delete",
            crate::c_abi::gos_rt_http_delete as *const u8,
        ),
        (
            "gos_rt_http_get",
            crate::c_abi::gos_rt_http_get as *const u8,
        ),
        (
            "gos_rt_http_head",
            crate::c_abi::gos_rt_http_head as *const u8,
        ),
        (
            "gos_rt_http_options",
            crate::c_abi::gos_rt_http_options as *const u8,
        ),
        (
            "gos_rt_http_post",
            crate::c_abi::gos_rt_http_post as *const u8,
        ),
        (
            "gos_rt_http_put",
            crate::c_abi::gos_rt_http_put as *const u8,
        ),
        (
            "gos_rt_http_request",
            crate::c_abi::gos_rt_http_request as *const u8,
        ),
        (
            "gos_rt_http_request_body",
            crate::c_abi::gos_rt_http_request_body as *const u8,
        ),
        (
            "gos_rt_http_request_body_str",
            crate::c_abi::gos_rt_http_request_body_str as *const u8,
        ),
        (
            "gos_rt_http_request_bytes",
            crate::c_abi::gos_rt_http_request_bytes as *const u8,
        ),
        (
            "gos_rt_http_request_get_header",
            crate::c_abi::gos_rt_http_request_get_header as *const u8,
        ),
        (
            "gos_rt_http_request_header",
            crate::c_abi::gos_rt_http_request_header as *const u8,
        ),
        (
            "gos_rt_http_request_headers",
            crate::c_abi::gos_rt_http_request_headers as *const u8,
        ),
        (
            "gos_rt_http_request_method",
            crate::c_abi::gos_rt_http_request_method as *const u8,
        ),
        (
            "gos_rt_http_request_path",
            crate::c_abi::gos_rt_http_request_path as *const u8,
        ),
        (
            "gos_rt_http_request_path_float",
            crate::c_abi::gos_rt_http_request_path_float as *const u8,
        ),
        (
            "gos_rt_http_request_path_int",
            crate::c_abi::gos_rt_http_request_path_int as *const u8,
        ),
        (
            "gos_rt_http_request_path_value",
            crate::c_abi::gos_rt_http_request_path_value as *const u8,
        ),
        (
            "gos_rt_http_request_query",
            crate::c_abi::gos_rt_http_request_query as *const u8,
        ),
        (
            "gos_rt_http_request_query_pairs",
            crate::c_abi::gos_rt_http_request_query_pairs as *const u8,
        ),
        (
            "gos_rt_http_request_peer_addr",
            crate::c_abi::gos_rt_http_request_peer_addr as *const u8,
        ),
        (
            "gos_rt_http_request_context",
            crate::c_abi::gos_rt_http_request_context as *const u8,
        ),
        (
            "gos_rt_http_request_raw_body",
            crate::c_abi::gos_rt_http_request_raw_body as *const u8,
        ),
        (
            "gos_rt_http_request_send",
            crate::c_abi::gos_rt_http_request_send as *const u8,
        ),
        (
            "gos_rt_http_request_set_header",
            crate::c_abi::gos_rt_http_request_set_header as *const u8,
        ),
        (
            "gos_rt_http_request_set_value",
            crate::c_abi::gos_rt_http_request_set_value as *const u8,
        ),
        (
            "gos_rt_http_request_value",
            crate::c_abi::gos_rt_http_request_value as *const u8,
        ),
        (
            "gos_rt_http_request_form_value",
            crate::c_abi::gos_rt_http_request_form_value as *const u8,
        ),
        (
            "gos_rt_http_request_basic_auth",
            crate::c_abi::gos_rt_http_request_basic_auth as *const u8,
        ),
        (
            "gos_rt_http_response_body",
            crate::c_abi::gos_rt_http_response_body as *const u8,
        ),
        (
            "gos_rt_http_response_content_type",
            crate::c_abi::gos_rt_http_response_content_type as *const u8,
        ),
        (
            "gos_rt_http_response_get_header",
            crate::c_abi::gos_rt_http_response_get_header as *const u8,
        ),
        (
            "gos_rt_http_response_headers",
            crate::c_abi::gos_rt_http_response_headers as *const u8,
        ),
        (
            "gos_rt_http_response_json_new",
            crate::c_abi::gos_rt_http_response_json_new as *const u8,
        ),
        (
            "gos_rt_http_response_location",
            crate::c_abi::gos_rt_http_response_location as *const u8,
        ),
        (
            "gos_rt_http_response_raw_bytes",
            crate::c_abi::gos_rt_http_response_raw_bytes as *const u8,
        ),
        (
            "gos_rt_http_response_set_body_bytes",
            crate::c_abi::gos_rt_http_response_set_body_bytes as *const u8,
        ),
        (
            "gos_rt_http_response_set_content_type",
            crate::c_abi::gos_rt_http_response_set_content_type as *const u8,
        ),
        (
            "gos_rt_http_response_set_header",
            crate::c_abi::gos_rt_http_response_set_header as *const u8,
        ),
        (
            "gos_rt_http_response_status",
            crate::c_abi::gos_rt_http_response_status as *const u8,
        ),
        (
            "gos_rt_http_response_with_header",
            crate::c_abi::gos_rt_http_response_with_header as *const u8,
        ),
        (
            "gos_rt_http_response_text_new",
            crate::c_abi::gos_rt_http_response_text_new as *const u8,
        ),
        (
            "gos_rt_http_response_free",
            crate::c_abi::gos_rt_http_response_free as *const u8,
        ),
        (
            "gos_rt_http_response_stream_new",
            crate::c_abi::gos_rt_http_response_stream_new as *const u8,
        ),
        (
            "gos_rt_http_serve",
            crate::c_abi::gos_rt_http_serve as *const u8,
        ),
        (
            "gos_rt_http_serve_tls",
            crate::c_abi::gos_rt_http_serve_tls as *const u8,
        ),
        (
            "gos_rt_http_session_sign",
            crate::c_abi::gos_rt_http_session_sign as *const u8,
        ),
        (
            "gos_rt_http_session_verify",
            crate::c_abi::gos_rt_http_session_verify as *const u8,
        ),
        (
            "gos_rt_http_stream",
            crate::c_abi::gos_rt_http_stream as *const u8,
        ),
        (
            "gos_rt_http_stream_next_chunk",
            crate::c_abi::gos_rt_http_stream_next_chunk as *const u8,
        ),
        (
            "gos_rt_http_stream_next_line",
            crate::c_abi::gos_rt_http_stream_next_line as *const u8,
        ),
        (
            "gos_rt_i64_chars",
            crate::c_abi::gos_rt_i64_chars as *const u8,
        ),
        (
            "gos_rt_i64_to_str",
            crate::c_abi::gos_rt_i64_to_str as *const u8,
        ),
        (
            "gos_rt_int_wrapping_add",
            crate::c_abi::gos_rt_int_wrapping_add as *const u8,
        ),
        (
            "gos_rt_int_wrapping_mul",
            crate::c_abi::gos_rt_int_wrapping_mul as *const u8,
        ),
        (
            "gos_rt_intarr_slice_result",
            crate::c_abi::gos_rt_intarr_slice_result as *const u8,
        ),
        (
            "gos_rt_bytearr_slice_result",
            crate::c_abi::gos_rt_bytearr_slice_result as *const u8,
        ),
        (
            "gos_rt_io_buffer_writer",
            crate::c_abi::gos_rt_io_buffer_writer as *const u8,
        ),
        (
            "gos_rt_io_close_writer",
            crate::c_abi::gos_rt_io_close_writer as *const u8,
        ),
        (
            "gos_rt_io_contents",
            crate::c_abi::gos_rt_io_contents as *const u8,
        ),
        (
            "gos_rt_io_copy_n",
            crate::c_abi::gos_rt_io_copy_n as *const u8,
        ),
        (
            "gos_rt_io_drain",
            crate::c_abi::gos_rt_io_drain as *const u8,
        ),
        (
            "gos_rt_io_limit_reader",
            crate::c_abi::gos_rt_io_limit_reader as *const u8,
        ),
        (
            "gos_rt_io_multi_reader",
            crate::c_abi::gos_rt_io_multi_reader as *const u8,
        ),
        ("gos_rt_io_pipe", crate::c_abi::gos_rt_io_pipe as *const u8),
        (
            "gos_rt_io_string_reader",
            crate::c_abi::gos_rt_io_string_reader as *const u8,
        ),
        (
            "gos_rt_io_tee_reader",
            crate::c_abi::gos_rt_io_tee_reader as *const u8,
        ),
        (
            "gos_rt_io_write_str",
            crate::c_abi::gos_rt_io_write_str as *const u8,
        ),
        (
            "gos_rt_middleware_new_kind",
            crate::c_abi::gos_rt_middleware_new_kind as *const u8,
        ),
        (
            "gos_rt_mw_cache_immutable_for",
            crate::c_abi::gos_rt_mw_cache_immutable_for as *const u8,
        ),
        (
            "gos_rt_mw_cache_no_store",
            crate::c_abi::gos_rt_mw_cache_no_store as *const u8,
        ),
        (
            "gos_rt_mw_cors_new",
            crate::c_abi::gos_rt_mw_cors_new as *const u8,
        ),
        (
            "gos_rt_mw_cors_permissive",
            crate::c_abi::gos_rt_mw_cors_permissive as *const u8,
        ),
        (
            "gos_rt_mw_hsts_safe_default",
            crate::c_abi::gos_rt_mw_hsts_safe_default as *const u8,
        ),
        (
            "gos_rt_mw_hsts_strict",
            crate::c_abi::gos_rt_mw_hsts_strict as *const u8,
        ),
        (
            "gos_rt_mw_rate_limit_per_ip",
            crate::c_abi::gos_rt_mw_rate_limit_per_ip as *const u8,
        ),
        (
            "gos_rt_mw_security_off",
            crate::c_abi::gos_rt_mw_security_off as *const u8,
        ),
        (
            "gos_rt_mw_security_strict",
            crate::c_abi::gos_rt_mw_security_strict as *const u8,
        ),
        (
            "gos_rt_packed_bytearr_slice_result",
            crate::c_abi::gos_rt_packed_bytearr_slice_result as *const u8,
        ),
        ("gos_rt_io_copy", crate::c_abi::gos_rt_io_copy as *const u8),
        (
            "gos_rt_io_read_all",
            crate::c_abi::gos_rt_io_read_all as *const u8,
        ),
        (
            "gos_rt_io_stderr",
            crate::c_abi::gos_rt_io_stderr as *const u8,
        ),
        (
            "gos_rt_io_stdin",
            crate::c_abi::gos_rt_io_stdin as *const u8,
        ),
        (
            "gos_rt_io_stdout",
            crate::c_abi::gos_rt_io_stdout as *const u8,
        ),
        (
            "gos_rt_iter_all_i64",
            crate::c_abi::gos_rt_iter_all_i64 as *const u8,
        ),
        (
            "gos_rt_iter_all_ptr",
            crate::c_abi::gos_rt_iter_all_ptr as *const u8,
        ),
        (
            "gos_rt_iter_any_ptr",
            crate::c_abi::gos_rt_iter_any_ptr as *const u8,
        ),
        (
            "gos_rt_iter_any_f64",
            crate::c_abi::gos_rt_iter_any_f64 as *const u8,
        ),
        (
            "gos_rt_iter_any_i64",
            crate::c_abi::gos_rt_iter_any_i64 as *const u8,
        ),
        (
            "gos_rt_iter_chain_i64",
            crate::c_abi::gos_rt_iter_chain_i64 as *const u8,
        ),
        (
            "gos_rt_iter_count",
            crate::c_abi::gos_rt_iter_count as *const u8,
        ),
        (
            "gos_rt_iter_filter_i64",
            crate::c_abi::gos_rt_iter_filter_i64 as *const u8,
        ),
        (
            "gos_rt_iter_find_i64",
            crate::c_abi::gos_rt_iter_find_i64 as *const u8,
        ),
        (
            "gos_rt_iter_find_i64_flag",
            crate::c_abi::gos_rt_iter_find_i64_flag as *const u8,
        ),
        (
            "gos_rt_iter_fold_i64",
            crate::c_abi::gos_rt_iter_fold_i64 as *const u8,
        ),
        (
            "gos_rt_iter_for_each_i64",
            crate::c_abi::gos_rt_iter_for_each_i64 as *const u8,
        ),
        (
            "gos_rt_iter_for_each_ptr",
            crate::c_abi::gos_rt_iter_for_each_ptr as *const u8,
        ),
        (
            "gos_rt_iter_map_i64",
            crate::c_abi::gos_rt_iter_map_i64 as *const u8,
        ),
        (
            "gos_rt_iter_map_ptr_i64",
            crate::c_abi::gos_rt_iter_map_ptr_i64 as *const u8,
        ),
        (
            "gos_rt_iter_map_f64",
            crate::c_abi::gos_rt_iter_map_f64 as *const u8,
        ),
        (
            "gos_rt_iter_map_f64_word",
            crate::c_abi::gos_rt_iter_map_f64_word as *const u8,
        ),
        (
            "gos_rt_iter_map_word_f64",
            crate::c_abi::gos_rt_iter_map_word_f64 as *const u8,
        ),
        (
            "gos_rt_iter_filter_f64",
            crate::c_abi::gos_rt_iter_filter_f64 as *const u8,
        ),
        (
            "gos_rt_iter_for_each_f64",
            crate::c_abi::gos_rt_iter_for_each_f64 as *const u8,
        ),
        (
            "gos_rt_iter_max_i64",
            crate::c_abi::gos_rt_iter_max_i64 as *const u8,
        ),
        (
            "gos_rt_iter_min_i64",
            crate::c_abi::gos_rt_iter_min_i64 as *const u8,
        ),
        (
            "gos_rt_iter_min_f64",
            crate::c_abi::gos_rt_iter_min_f64 as *const u8,
        ),
        (
            "gos_rt_iter_max_f64",
            crate::c_abi::gos_rt_iter_max_f64 as *const u8,
        ),
        (
            "gos_rt_iter_product_f64",
            crate::c_abi::gos_rt_iter_product_f64 as *const u8,
        ),
        (
            "gos_rt_iter_product_i64",
            crate::c_abi::gos_rt_iter_product_i64 as *const u8,
        ),
        (
            "gos_rt_iter_range",
            crate::c_abi::gos_rt_iter_range as *const u8,
        ),
        (
            "gos_rt_iter_range_inclusive",
            crate::c_abi::gos_rt_iter_range_inclusive as *const u8,
        ),
        (
            "gos_rt_iter_repeat_i64",
            crate::c_abi::gos_rt_iter_repeat_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_range_i64",
            crate::c_abi::gos_rt_lazy_iter_range_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_range_from_i64",
            crate::c_abi::gos_rt_lazy_iter_range_from_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_range_inclusive_i64",
            crate::c_abi::gos_rt_lazy_iter_range_inclusive_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_repeat_i64",
            crate::c_abi::gos_rt_lazy_iter_repeat_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_once_i64",
            crate::c_abi::gos_rt_lazy_iter_once_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_take_i64",
            crate::c_abi::gos_rt_lazy_iter_take_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_step_by_i64",
            crate::c_abi::gos_rt_lazy_iter_step_by_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_skip_i64",
            crate::c_abi::gos_rt_lazy_iter_skip_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_chain_i64",
            crate::c_abi::gos_rt_lazy_iter_chain_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_enumerate_i64",
            crate::c_abi::gos_rt_lazy_iter_enumerate_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_zip_i64",
            crate::c_abi::gos_rt_lazy_iter_zip_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_map_i64",
            crate::c_abi::gos_rt_lazy_iter_map_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_filter_i64",
            crate::c_abi::gos_rt_lazy_iter_filter_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_collect_i64",
            crate::c_abi::gos_rt_lazy_iter_collect_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_collect_pair_i64",
            crate::c_abi::gos_rt_lazy_iter_collect_pair_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_count_i64",
            crate::c_abi::gos_rt_lazy_iter_count_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_count_pair_i64",
            crate::c_abi::gos_rt_lazy_iter_count_pair_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_drop_i64",
            crate::c_abi::gos_rt_lazy_iter_drop_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_drop_pair_i64",
            crate::c_abi::gos_rt_lazy_iter_drop_pair_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_sum_i64",
            crate::c_abi::gos_rt_lazy_iter_sum_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_product_i64",
            crate::c_abi::gos_rt_lazy_iter_product_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_min_i64",
            crate::c_abi::gos_rt_lazy_iter_min_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_max_i64",
            crate::c_abi::gos_rt_lazy_iter_max_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_next_i64",
            crate::c_abi::gos_rt_lazy_iter_next_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_fold_i64",
            crate::c_abi::gos_rt_lazy_iter_fold_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_from_vec_i64",
            crate::c_abi::gos_rt_lazy_iter_from_vec_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_any_i64",
            crate::c_abi::gos_rt_lazy_iter_any_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_all_f64",
            crate::c_abi::gos_rt_lazy_iter_all_f64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_any_f64",
            crate::c_abi::gos_rt_lazy_iter_any_f64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_filter_f64",
            crate::c_abi::gos_rt_lazy_iter_filter_f64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_find_f64",
            crate::c_abi::gos_rt_lazy_iter_find_f64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_fold_f64",
            crate::c_abi::gos_rt_lazy_iter_fold_f64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_fold_f64_word",
            crate::c_abi::gos_rt_lazy_iter_fold_f64_word as *const u8,
        ),
        (
            "gos_rt_lazy_iter_fold_word_f64",
            crate::c_abi::gos_rt_lazy_iter_fold_word_f64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_from_vec_f64",
            crate::c_abi::gos_rt_lazy_iter_from_vec_f64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_from_vec_aggr",
            crate::c_abi::gos_rt_lazy_iter_from_vec_aggr as *const u8,
        ),
        (
            "gos_rt_lazy_iter_collect_aggr",
            crate::c_abi::gos_rt_lazy_iter_collect_aggr as *const u8,
        ),
        (
            "gos_rt_lazy_iter_map_f64",
            crate::c_abi::gos_rt_lazy_iter_map_f64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_map_f64_word",
            crate::c_abi::gos_rt_lazy_iter_map_f64_word as *const u8,
        ),
        (
            "gos_rt_lazy_iter_map_word_f64",
            crate::c_abi::gos_rt_lazy_iter_map_word_f64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_max_f64",
            crate::c_abi::gos_rt_lazy_iter_max_f64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_min_f64",
            crate::c_abi::gos_rt_lazy_iter_min_f64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_next_f64",
            crate::c_abi::gos_rt_lazy_iter_next_f64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_once_f64",
            crate::c_abi::gos_rt_lazy_iter_once_f64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_product_f64",
            crate::c_abi::gos_rt_lazy_iter_product_f64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_repeat_f64",
            crate::c_abi::gos_rt_lazy_iter_repeat_f64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_sum_f64",
            crate::c_abi::gos_rt_lazy_iter_sum_f64 as *const u8,
        ),
        (
            "gos_rt_iter_all_f64",
            crate::c_abi::gos_rt_iter_all_f64 as *const u8,
        ),
        (
            "gos_rt_iter_filter_ptr",
            crate::c_abi::gos_rt_iter_filter_ptr as *const u8,
        ),
        (
            "gos_rt_iter_fold_f64",
            crate::c_abi::gos_rt_iter_fold_f64 as *const u8,
        ),
        (
            "gos_rt_iter_fold_f64_ptr",
            crate::c_abi::gos_rt_iter_fold_f64_ptr as *const u8,
        ),
        (
            "gos_rt_iter_fold_f64_word",
            crate::c_abi::gos_rt_iter_fold_f64_word as *const u8,
        ),
        (
            "gos_rt_iter_fold_ptr",
            crate::c_abi::gos_rt_iter_fold_ptr as *const u8,
        ),
        (
            "gos_rt_iter_fold_word_f64",
            crate::c_abi::gos_rt_iter_fold_word_f64 as *const u8,
        ),
        (
            "gos_rt_iter_map_ptr_f64",
            crate::c_abi::gos_rt_iter_map_ptr_f64 as *const u8,
        ),
        (
            "gos_rt_iter_sum_by_f64",
            crate::c_abi::gos_rt_iter_sum_by_f64 as *const u8,
        ),
        (
            "gos_rt_iter_sum_by_f64_word",
            crate::c_abi::gos_rt_iter_sum_by_f64_word as *const u8,
        ),
        (
            "gos_rt_iter_sum_by_ptr",
            crate::c_abi::gos_rt_iter_sum_by_ptr as *const u8,
        ),
        (
            "gos_rt_iter_sum_by_ptr_f64",
            crate::c_abi::gos_rt_iter_sum_by_ptr_f64 as *const u8,
        ),
        (
            "gos_rt_iter_sum_by_word_f64",
            crate::c_abi::gos_rt_iter_sum_by_word_f64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_all_i64",
            crate::c_abi::gos_rt_lazy_iter_all_i64 as *const u8,
        ),
        (
            "gos_rt_lazy_iter_find_i64",
            crate::c_abi::gos_rt_lazy_iter_find_i64 as *const u8,
        ),
        (
            "gos_rt_iter_reversed_i64",
            crate::c_abi::gos_rt_iter_reversed_i64 as *const u8,
        ),
        (
            "gos_rt_iter_skip_i64",
            crate::c_abi::gos_rt_iter_skip_i64 as *const u8,
        ),
        (
            "gos_rt_iter_sum_by_i64",
            crate::c_abi::gos_rt_iter_sum_by_i64 as *const u8,
        ),
        (
            "gos_rt_iter_sum_f64",
            crate::c_abi::gos_rt_iter_sum_f64 as *const u8,
        ),
        (
            "gos_rt_iter_sum_i64",
            crate::c_abi::gos_rt_iter_sum_i64 as *const u8,
        ),
        (
            "gos_rt_iter_take_i64",
            crate::c_abi::gos_rt_iter_take_i64 as *const u8,
        ),
        (
            "gos_rt_iter_chunk_by_size_i64",
            crate::c_abi::gos_rt_iter_chunk_by_size_i64 as *const u8,
        ),
        (
            "gos_rt_iter_dedup_i64",
            crate::c_abi::gos_rt_iter_dedup_i64 as *const u8,
        ),
        (
            "gos_rt_iter_enumerate_i64",
            crate::c_abi::gos_rt_iter_enumerate_i64 as *const u8,
        ),
        (
            "gos_rt_iter_flatten_i64",
            crate::c_abi::gos_rt_iter_flatten_i64 as *const u8,
        ),
        (
            "gos_rt_iter_unzip_i64",
            crate::c_abi::gos_rt_iter_unzip_i64 as *const u8,
        ),
        (
            "gos_rt_iter_windowed_i64",
            crate::c_abi::gos_rt_iter_windowed_i64 as *const u8,
        ),
        (
            "gos_rt_iter_zip_i64",
            crate::c_abi::gos_rt_iter_zip_i64 as *const u8,
        ),
        ("gos_rt_join", crate::c_abi::gos_rt_join as *const u8),
        (
            "gos_rt_json_array_from_scalar_vec",
            crate::c_abi::gos_rt_json_array_from_scalar_vec as *const u8,
        ),
        (
            "gos_rt_json_as_array_opt",
            crate::c_abi::gos_rt_json_as_array_opt as *const u8,
        ),
        (
            "gos_rt_json_as_bool",
            crate::c_abi::gos_rt_json_as_bool as *const u8,
        ),
        (
            "gos_rt_json_as_bool_opt",
            crate::c_abi::gos_rt_json_as_bool_opt as *const u8,
        ),
        (
            "gos_rt_json_as_f64",
            crate::c_abi::gos_rt_json_as_f64 as *const u8,
        ),
        (
            "gos_rt_json_as_f64_opt",
            crate::c_abi::gos_rt_json_as_f64_opt as *const u8,
        ),
        (
            "gos_rt_json_as_i64",
            crate::c_abi::gos_rt_json_as_i64 as *const u8,
        ),
        (
            "gos_rt_json_as_i64_opt",
            crate::c_abi::gos_rt_json_as_i64_opt as *const u8,
        ),
        (
            "gos_rt_json_as_str",
            crate::c_abi::gos_rt_json_as_str as *const u8,
        ),
        (
            "gos_rt_json_as_str_opt",
            crate::c_abi::gos_rt_json_as_str_opt as *const u8,
        ),
        ("gos_rt_json_at", crate::c_abi::gos_rt_json_at as *const u8),
        (
            "gos_rt_json_display",
            crate::c_abi::gos_rt_json_display as *const u8,
        ),
        (
            "gos_rt_json_free",
            crate::c_abi::gos_rt_json_free as *const u8,
        ),
        (
            "gos_rt_json_free_slots",
            crate::c_abi::gos_rt_json_free_slots as *const u8,
        ),
        (
            "gos_rt_json_get",
            crate::c_abi::gos_rt_json_get as *const u8,
        ),
        (
            "gos_rt_json_get_opt",
            crate::c_abi::gos_rt_json_get_opt as *const u8,
        ),
        (
            "gos_rt_json_identity",
            crate::c_abi::gos_rt_json_identity as *const u8,
        ),
        (
            "gos_rt_json_is_null",
            crate::c_abi::gos_rt_json_is_null as *const u8,
        ),
        (
            "gos_rt_json_keys_opt",
            crate::c_abi::gos_rt_json_keys_opt as *const u8,
        ),
        (
            "gos_rt_json_len",
            crate::c_abi::gos_rt_json_len as *const u8,
        ),
        (
            "gos_rt_json_parse",
            crate::c_abi::gos_rt_json_parse as *const u8,
        ),
        (
            "gos_rt_json_valid",
            crate::c_abi::gos_rt_json_valid as *const u8,
        ),
        (
            "gos_rt_json_render",
            crate::c_abi::gos_rt_json_render as *const u8,
        ),
        (
            "gos_rt_json_writer_begin_array",
            crate::c_abi::gos_rt_json_writer_begin_array as *const u8,
        ),
        (
            "gos_rt_json_writer_begin_object",
            crate::c_abi::gos_rt_json_writer_begin_object as *const u8,
        ),
        (
            "gos_rt_json_writer_bool",
            crate::c_abi::gos_rt_json_writer_bool as *const u8,
        ),
        (
            "gos_rt_json_writer_end_array",
            crate::c_abi::gos_rt_json_writer_end_array as *const u8,
        ),
        (
            "gos_rt_json_writer_end_object",
            crate::c_abi::gos_rt_json_writer_end_object as *const u8,
        ),
        (
            "gos_rt_json_writer_f64",
            crate::c_abi::gos_rt_json_writer_f64 as *const u8,
        ),
        (
            "gos_rt_json_writer_finish",
            crate::c_abi::gos_rt_json_writer_finish as *const u8,
        ),
        (
            "gos_rt_json_writer_i64",
            crate::c_abi::gos_rt_json_writer_i64 as *const u8,
        ),
        (
            "gos_rt_json_writer_key",
            crate::c_abi::gos_rt_json_writer_key as *const u8,
        ),
        (
            "gos_rt_json_writer_new",
            crate::c_abi::gos_rt_json_writer_new as *const u8,
        ),
        (
            "gos_rt_json_writer_new_pretty",
            crate::c_abi::gos_rt_json_writer_new_pretty as *const u8,
        ),
        (
            "gos_rt_json_writer_null",
            crate::c_abi::gos_rt_json_writer_null as *const u8,
        ),
        (
            "gos_rt_json_writer_str",
            crate::c_abi::gos_rt_json_writer_str as *const u8,
        ),
        (
            "gos_rt_json_writer_value",
            crate::c_abi::gos_rt_json_writer_value as *const u8,
        ),
        (
            "gos_rt_json_render_pretty",
            crate::c_abi::gos_rt_json_render_pretty as *const u8,
        ),
        (
            "gos_rt_json_set",
            crate::c_abi::gos_rt_json_set as *const u8,
        ),
        (
            "gos_rt_json_value_array",
            crate::c_abi::gos_rt_json_value_array as *const u8,
        ),
        (
            "gos_rt_json_value_array_owned",
            crate::c_abi::gos_rt_json_value_array_owned as *const u8,
        ),
        (
            "gos_rt_json_value_object_owned",
            crate::c_abi::gos_rt_json_value_object_owned as *const u8,
        ),
        (
            "gos_rt_json_value_bool",
            crate::c_abi::gos_rt_json_value_bool as *const u8,
        ),
        (
            "gos_rt_json_value_float",
            crate::c_abi::gos_rt_json_value_float as *const u8,
        ),
        (
            "gos_rt_json_value_int",
            crate::c_abi::gos_rt_json_value_int as *const u8,
        ),
        (
            "gos_rt_json_value_null",
            crate::c_abi::gos_rt_json_value_null as *const u8,
        ),
        (
            "gos_rt_json_value_object",
            crate::c_abi::gos_rt_json_value_object as *const u8,
        ),
        (
            "gos_rt_json_value_object_n",
            crate::c_abi::gos_rt_json_value_object_n as *const u8,
        ),
        (
            "gos_rt_json_value_string",
            crate::c_abi::gos_rt_json_value_string as *const u8,
        ),
        (
            "gos_rt_jwt_sign_eddsa",
            crate::c_abi::gos_rt_jwt_sign_eddsa as *const u8,
        ),
        (
            "gos_rt_jwt_sign_es256",
            crate::c_abi::gos_rt_jwt_sign_es256 as *const u8,
        ),
        (
            "gos_rt_jwt_sign_hs",
            crate::c_abi::gos_rt_jwt_sign_hs as *const u8,
        ),
        (
            "gos_rt_jwt_verify_eddsa",
            crate::c_abi::gos_rt_jwt_verify_eddsa as *const u8,
        ),
        (
            "gos_rt_jwt_verify_es256",
            crate::c_abi::gos_rt_jwt_verify_es256 as *const u8,
        ),
        (
            "gos_rt_jwt_verify_hs",
            crate::c_abi::gos_rt_jwt_verify_hs as *const u8,
        ),
        (
            "gos_rt_jwt_verify",
            crate::c_abi::crypto_jwt::gos_rt_jwt_verify as *const u8,
        ),
        (
            "gos_rt_jwt_header",
            crate::c_abi::crypto_jwt::gos_rt_jwt_header as *const u8,
        ),
        (
            "gos_rt_lcg_jump",
            crate::c_abi::gos_rt_lcg_jump as *const u8,
        ),
        ("gos_rt_len", crate::c_abi::gos_rt_len as *const u8),
        (
            "gos_rt_len_is_zero",
            crate::c_abi::gos_rt_len_is_zero as *const u8,
        ),
        (
            "gos_rt_program_start",
            crate::c_abi::gos_rt_program_start as *const u8,
        ),
        (
            "gos_rt_main_exit_code",
            crate::c_abi::gos_rt_main_exit_code as *const u8,
        ),
        (
            "gos_rt_main_exit_code_err",
            crate::c_abi::gos_rt_main_exit_code_err as *const u8,
        ),
        (
            "gos_rt_map_assign",
            crate::c_abi::gos_rt_map_assign as *const u8,
        ),
        (
            "gos_rt_map_clear",
            crate::c_abi::gos_rt_map_clear as *const u8,
        ),
        (
            "gos_rt_map_clone",
            crate::c_abi::gos_rt_map_clone as *const u8,
        ),
        (
            "gos_rt_map_contains_key_i64",
            crate::c_abi::gos_rt_map_contains_key_i64 as *const u8,
        ),
        (
            "gos_rt_map_contains_key_str",
            crate::c_abi::gos_rt_map_contains_key_str as *const u8,
        ),
        (
            "gos_rt_map_contains_key_typed_str",
            crate::c_abi::gos_rt_map_contains_key_typed_str as *const u8,
        ),
        (
            "gos_rt_map_format",
            crate::c_abi::gos_rt_map_format as *const u8,
        ),
        (
            "gos_rt_map_format_tagged",
            crate::c_abi::gos_rt_map_format_tagged as *const u8,
        ),
        (
            "gos_rt_map_free",
            crate::c_abi::gos_rt_map_free as *const u8,
        ),
        ("gos_rt_map_get", crate::c_abi::gos_rt_map_get as *const u8),
        (
            "gos_rt_map_get_i64",
            crate::c_abi::gos_rt_map_get_i64 as *const u8,
        ),
        (
            "gos_rt_map_get_i64_opt",
            crate::c_abi::gos_rt_map_get_i64_opt as *const u8,
        ),
        (
            "gos_rt_map_get_i64_str",
            crate::c_abi::gos_rt_map_get_i64_str as *const u8,
        ),
        (
            "gos_rt_map_get_or_i64",
            crate::c_abi::gos_rt_map_get_or_i64 as *const u8,
        ),
        (
            "gos_rt_map_get_or_i64_str",
            crate::c_abi::gos_rt_map_get_or_i64_str as *const u8,
        ),
        (
            "gos_rt_map_get_or_str_i64",
            crate::c_abi::gos_rt_map_get_or_str_i64 as *const u8,
        ),
        (
            "gos_rt_map_get_or_str_str",
            crate::c_abi::gos_rt_map_get_or_str_str as *const u8,
        ),
        (
            "gos_rt_map_get_or_typed_str_i64",
            crate::c_abi::gos_rt_map_get_or_typed_str_i64 as *const u8,
        ),
        (
            "gos_rt_map_get_str_i64",
            crate::c_abi::gos_rt_map_get_str_i64 as *const u8,
        ),
        (
            "gos_rt_map_get_str_opt",
            crate::c_abi::gos_rt_map_get_str_opt as *const u8,
        ),
        (
            "gos_rt_map_get_str_str",
            crate::c_abi::gos_rt_map_get_str_str as *const u8,
        ),
        (
            "gos_rt_map_get_typed_str_i64",
            crate::c_abi::gos_rt_map_get_typed_str_i64 as *const u8,
        ),
        (
            "gos_rt_map_get_typed_str_opt",
            crate::c_abi::gos_rt_map_get_typed_str_opt as *const u8,
        ),
        (
            "gos_rt_map_inc_at_str_i64",
            crate::c_abi::gos_rt_map_inc_at_str_i64 as *const u8,
        ),
        (
            "gos_rt_map_inc_i64",
            crate::c_abi::gos_rt_map_inc_i64 as *const u8,
        ),
        (
            "gos_rt_map_inc_str_i64",
            crate::c_abi::gos_rt_map_inc_str_i64 as *const u8,
        ),
        (
            "gos_rt_map_inc_typed_str_i64",
            crate::c_abi::gos_rt_map_inc_typed_str_i64 as *const u8,
        ),
        ("gos_rt_map_eq", crate::c_abi::gos_rt_map_eq as *const u8),
        (
            "gos_rt_map_insert",
            crate::c_abi::gos_rt_map_insert as *const u8,
        ),
        (
            "gos_rt_map_insert_i64_i64",
            crate::c_abi::gos_rt_map_insert_i64_i64 as *const u8,
        ),
        (
            "gos_rt_map_insert_i64_i64_opt",
            crate::c_abi::gos_rt_map_insert_i64_i64_opt as *const u8,
        ),
        (
            "gos_rt_map_insert_skey",
            crate::c_abi::gos_rt_map_insert_skey as *const u8,
        ),
        (
            "gos_rt_map_insert_skey_opt",
            crate::c_abi::gos_rt_map_insert_skey_opt as *const u8,
        ),
        (
            "gos_rt_map_get_skey_opt",
            crate::c_abi::gos_rt_map_get_skey_opt as *const u8,
        ),
        (
            "gos_rt_map_contains_skey",
            crate::c_abi::gos_rt_map_contains_skey as *const u8,
        ),
        (
            "gos_rt_map_pop_skey",
            crate::c_abi::gos_rt_map_pop_skey as *const u8,
        ),
        (
            "gos_rt_map_get_or_skey",
            crate::c_abi::gos_rt_map_get_or_skey as *const u8,
        ),
        (
            "gos_rt_map_or_insert_skey",
            crate::c_abi::gos_rt_map_or_insert_skey as *const u8,
        ),
        (
            "gos_rt_map_inc_skey",
            crate::c_abi::gos_rt_map_inc_skey as *const u8,
        ),
        (
            "gos_rt_map_insert_ekey_opt",
            crate::c_abi::gos_rt_map_insert_ekey_opt as *const u8,
        ),
        (
            "gos_rt_map_get_ekey_opt",
            crate::c_abi::gos_rt_map_get_ekey_opt as *const u8,
        ),
        (
            "gos_rt_map_contains_ekey",
            crate::c_abi::gos_rt_map_contains_ekey as *const u8,
        ),
        (
            "gos_rt_map_pop_ekey",
            crate::c_abi::gos_rt_map_pop_ekey as *const u8,
        ),
        (
            "gos_rt_map_get_or_ekey",
            crate::c_abi::gos_rt_map_get_or_ekey as *const u8,
        ),
        (
            "gos_rt_map_or_insert_ekey",
            crate::c_abi::gos_rt_map_or_insert_ekey as *const u8,
        ),
        (
            "gos_rt_map_inc_ekey",
            crate::c_abi::gos_rt_map_inc_ekey as *const u8,
        ),
        (
            "gos_rt_map_keys_ekey",
            crate::c_abi::gos_rt_map_keys_ekey as *const u8,
        ),
        (
            "gos_rt_map_insert_i64_str",
            crate::c_abi::gos_rt_map_insert_i64_str as *const u8,
        ),
        (
            "gos_rt_map_insert_i64_str_opt",
            crate::c_abi::gos_rt_map_insert_i64_str_opt as *const u8,
        ),
        (
            "gos_rt_map_insert_str_i64",
            crate::c_abi::gos_rt_map_insert_str_i64 as *const u8,
        ),
        (
            "gos_rt_map_insert_str_i64_opt",
            crate::c_abi::gos_rt_map_insert_str_i64_opt as *const u8,
        ),
        (
            "gos_rt_map_insert_str_str",
            crate::c_abi::gos_rt_map_insert_str_str as *const u8,
        ),
        (
            "gos_rt_map_insert_str_str_opt",
            crate::c_abi::gos_rt_map_insert_str_str_opt as *const u8,
        ),
        (
            "gos_rt_map_insert_typed_str_i64",
            crate::c_abi::gos_rt_map_insert_typed_str_i64 as *const u8,
        ),
        (
            "gos_rt_map_insert_typed_str_i64_opt",
            crate::c_abi::gos_rt_map_insert_typed_str_i64_opt as *const u8,
        ),
        (
            "gos_rt_map_keys_i64",
            crate::c_abi::gos_rt_map_keys_i64 as *const u8,
        ),
        (
            "gos_rt_map_keys_skey",
            crate::c_abi::gos_rt_map_keys_skey as *const u8,
        ),
        (
            "gos_rt_map_keys_str",
            crate::c_abi::gos_rt_map_keys_str as *const u8,
        ),
        (
            "gos_rt_map_keys_vec",
            crate::c_abi::gos_rt_map_keys_vec as *const u8,
        ),
        ("gos_rt_map_len", crate::c_abi::gos_rt_map_len as *const u8),
        (
            "gos_rt_map_mark_shared",
            crate::c_abi::gos_rt_map_mark_shared as *const u8,
        ),
        ("gos_rt_map_new", crate::c_abi::gos_rt_map_new as *const u8),
        (
            "gos_rt_map_new_with_capacity",
            crate::c_abi::gos_rt_map_new_with_capacity as *const u8,
        ),
        (
            "gos_rt_map_new_with_capacity_typed",
            crate::c_abi::gos_rt_map_new_with_capacity_typed as *const u8,
        ),
        (
            "gos_rt_map_or_insert_i64_i64",
            crate::c_abi::gos_rt_map_or_insert_i64_i64 as *const u8,
        ),
        (
            "gos_rt_map_or_insert_str_i64",
            crate::c_abi::gos_rt_map_or_insert_str_i64 as *const u8,
        ),
        (
            "gos_rt_map_or_insert_typed_str_i64",
            crate::c_abi::gos_rt_map_or_insert_typed_str_i64 as *const u8,
        ),
        (
            "gos_rt_map_pop_i64",
            crate::c_abi::gos_rt_map_pop_i64 as *const u8,
        ),
        (
            "gos_rt_map_pop_str",
            crate::c_abi::gos_rt_map_pop_str as *const u8,
        ),
        (
            "gos_rt_map_pop_typed_str",
            crate::c_abi::gos_rt_map_pop_typed_str as *const u8,
        ),
        (
            "gos_rt_map_remove",
            crate::c_abi::gos_rt_map_remove as *const u8,
        ),
        (
            "gos_rt_map_remove_i64",
            crate::c_abi::gos_rt_map_remove_i64 as *const u8,
        ),
        (
            "gos_rt_map_remove_str",
            crate::c_abi::gos_rt_map_remove_str as *const u8,
        ),
        (
            "gos_rt_map_remove_typed_str",
            crate::c_abi::gos_rt_map_remove_typed_str as *const u8,
        ),
        (
            "gos_rt_map_values_i64",
            crate::c_abi::gos_rt_map_values_i64 as *const u8,
        ),
        (
            "gos_rt_map_values_str",
            crate::c_abi::gos_rt_map_values_str as *const u8,
        ),
        (
            "gos_rt_map_values_vec",
            crate::c_abi::gos_rt_map_values_vec as *const u8,
        ),
        (
            "gos_rt_math_abs",
            crate::c_abi::gos_rt_math_abs as *const u8,
        ),
        (
            "gos_rt_math_abs_i64",
            crate::c_abi::gos_rt_math_abs_i64 as *const u8,
        ),
        (
            "gos_rt_math_acos",
            crate::c_abi::gos_rt_math_acos as *const u8,
        ),
        (
            "gos_rt_math_asin",
            crate::c_abi::gos_rt_math_asin as *const u8,
        ),
        (
            "gos_rt_math_atan",
            crate::c_abi::gos_rt_math_atan as *const u8,
        ),
        (
            "gos_rt_math_atan2",
            crate::c_abi::gos_rt_math_atan2 as *const u8,
        ),
        (
            "gos_rt_math_big_factorial",
            crate::c_abi::gos_rt_math_big_factorial as *const u8,
        ),
        (
            "gos_rt_math_big_int_abs",
            crate::c_abi::gos_rt_math_big_int_abs as *const u8,
        ),
        (
            "gos_rt_math_big_int_add",
            crate::c_abi::gos_rt_math_big_int_add as *const u8,
        ),
        (
            "gos_rt_math_big_int_cmp",
            crate::c_abi::gos_rt_math_big_int_cmp as *const u8,
        ),
        (
            "gos_rt_math_big_int_div",
            crate::c_abi::gos_rt_math_big_int_div as *const u8,
        ),
        (
            "gos_rt_math_big_int_from_i64",
            crate::c_abi::gos_rt_math_big_int_from_i64 as *const u8,
        ),
        (
            "gos_rt_math_big_int_from_str",
            crate::c_abi::gos_rt_math_big_int_from_str as *const u8,
        ),
        (
            "gos_rt_math_big_int_gcd",
            crate::c_abi::gos_rt_math_big_int_gcd as *const u8,
        ),
        (
            "gos_rt_math_big_int_is_negative",
            crate::c_abi::gos_rt_math_big_int_is_negative as *const u8,
        ),
        (
            "gos_rt_math_big_int_is_positive",
            crate::c_abi::gos_rt_math_big_int_is_positive as *const u8,
        ),
        (
            "gos_rt_math_big_int_is_zero",
            crate::c_abi::gos_rt_math_big_int_is_zero as *const u8,
        ),
        (
            "gos_rt_math_big_int_lcm",
            crate::c_abi::gos_rt_math_big_int_lcm as *const u8,
        ),
        (
            "gos_rt_math_big_int_mul",
            crate::c_abi::gos_rt_math_big_int_mul as *const u8,
        ),
        (
            "gos_rt_math_big_int_neg",
            crate::c_abi::gos_rt_math_big_int_neg as *const u8,
        ),
        (
            "gos_rt_math_big_int_pow",
            crate::c_abi::gos_rt_math_big_int_pow as *const u8,
        ),
        (
            "gos_rt_math_big_int_rem",
            crate::c_abi::gos_rt_math_big_int_rem as *const u8,
        ),
        (
            "gos_rt_math_big_int_sub",
            crate::c_abi::gos_rt_math_big_int_sub as *const u8,
        ),
        (
            "gos_rt_math_big_int_to_hex",
            crate::c_abi::gos_rt_math_big_int_to_hex as *const u8,
        ),
        (
            "gos_rt_math_big_int_to_i64",
            crate::c_abi::gos_rt_math_big_int_to_i64 as *const u8,
        ),
        (
            "gos_rt_math_big_int_to_str",
            crate::c_abi::gos_rt_math_big_int_to_str as *const u8,
        ),
        (
            "gos_rt_math_big_uint_add",
            crate::c_abi::gos_rt_math_big_uint_add as *const u8,
        ),
        (
            "gos_rt_math_big_uint_bit_len",
            crate::c_abi::gos_rt_math_big_uint_bit_len as *const u8,
        ),
        (
            "gos_rt_math_big_uint_from_str",
            crate::c_abi::gos_rt_math_big_uint_from_str as *const u8,
        ),
        (
            "gos_rt_math_big_uint_from_u64",
            crate::c_abi::gos_rt_math_big_uint_from_u64 as *const u8,
        ),
        (
            "gos_rt_math_big_uint_is_zero",
            crate::c_abi::gos_rt_math_big_uint_is_zero as *const u8,
        ),
        (
            "gos_rt_math_big_uint_mul",
            crate::c_abi::gos_rt_math_big_uint_mul as *const u8,
        ),
        (
            "gos_rt_math_big_uint_pow",
            crate::c_abi::gos_rt_math_big_uint_pow as *const u8,
        ),
        (
            "gos_rt_math_big_uint_pow_mod",
            crate::c_abi::gos_rt_math_big_uint_pow_mod as *const u8,
        ),
        (
            "gos_rt_math_big_uint_to_hex",
            crate::c_abi::gos_rt_math_big_uint_to_hex as *const u8,
        ),
        (
            "gos_rt_math_big_uint_to_str",
            crate::c_abi::gos_rt_math_big_uint_to_str as *const u8,
        ),
        (
            "gos_rt_math_big_uint_to_u64",
            crate::c_abi::gos_rt_math_big_uint_to_u64 as *const u8,
        ),
        (
            "gos_rt_math_cbrt",
            crate::c_abi::gos_rt_math_cbrt as *const u8,
        ),
        (
            "gos_rt_math_ceil",
            crate::c_abi::gos_rt_math_ceil as *const u8,
        ),
        (
            "gos_rt_math_copysign",
            crate::c_abi::gos_rt_math_copysign as *const u8,
        ),
        (
            "gos_rt_math_cos",
            crate::c_abi::gos_rt_math_cos as *const u8,
        ),
        (
            "gos_rt_math_cosh",
            crate::c_abi::gos_rt_math_cosh as *const u8,
        ),
        (
            "gos_rt_math_dim",
            crate::c_abi::gos_rt_math_dim as *const u8,
        ),
        (
            "gos_rt_math_exp",
            crate::c_abi::gos_rt_math_exp as *const u8,
        ),
        (
            "gos_rt_math_exp2",
            crate::c_abi::gos_rt_math_exp2 as *const u8,
        ),
        (
            "gos_rt_math_floor",
            crate::c_abi::gos_rt_math_floor as *const u8,
        ),
        (
            "gos_rt_math_fmod",
            crate::c_abi::gos_rt_math_fmod as *const u8,
        ),
        (
            "gos_rt_math_hypot",
            crate::c_abi::gos_rt_math_hypot as *const u8,
        ),
        (
            "gos_rt_math_inf",
            crate::c_abi::gos_rt_math_inf as *const u8,
        ),
        (
            "gos_rt_math_is_inf",
            crate::c_abi::gos_rt_math_is_inf as *const u8,
        ),
        (
            "gos_rt_math_is_nan",
            crate::c_abi::gos_rt_math_is_nan as *const u8,
        ),
        (
            "gos_rt_math_log",
            crate::c_abi::gos_rt_math_log as *const u8,
        ),
        (
            "gos_rt_math_log10",
            crate::c_abi::gos_rt_math_log10 as *const u8,
        ),
        (
            "gos_rt_math_log2",
            crate::c_abi::gos_rt_math_log2 as *const u8,
        ),
        (
            "gos_rt_math_nan",
            crate::c_abi::gos_rt_math_nan as *const u8,
        ),
        (
            "gos_rt_math_pow",
            crate::c_abi::gos_rt_math_pow as *const u8,
        ),
        (
            "gos_rt_math_rng_new",
            crate::c_abi::gos_rt_math_rng_new as *const u8,
        ),
        (
            "gos_rt_math_rng_next_f64",
            crate::c_abi::gos_rt_math_rng_next_f64 as *const u8,
        ),
        (
            "gos_rt_math_rng_next_u32",
            crate::c_abi::gos_rt_math_rng_next_u32 as *const u8,
        ),
        (
            "gos_rt_math_rng_next_u64",
            crate::c_abi::gos_rt_math_rng_next_u64 as *const u8,
        ),
        (
            "gos_rt_math_rng_range_u64",
            crate::c_abi::gos_rt_math_rng_range_u64 as *const u8,
        ),
        (
            "gos_rt_math_round",
            crate::c_abi::gos_rt_math_round as *const u8,
        ),
        (
            "gos_rt_math_sin",
            crate::c_abi::gos_rt_math_sin as *const u8,
        ),
        (
            "gos_rt_math_sinh",
            crate::c_abi::gos_rt_math_sinh as *const u8,
        ),
        (
            "gos_rt_math_sqrt",
            crate::c_abi::gos_rt_math_sqrt as *const u8,
        ),
        (
            "gos_rt_math_tan",
            crate::c_abi::gos_rt_math_tan as *const u8,
        ),
        (
            "gos_rt_math_tanh",
            crate::c_abi::gos_rt_math_tanh as *const u8,
        ),
        (
            "gos_rt_math_trunc",
            crate::c_abi::gos_rt_math_trunc as *const u8,
        ),
        ("gos_rt_max_f64", crate::c_abi::gos_rt_max_f64 as *const u8),
        ("gos_rt_max_i64", crate::c_abi::gos_rt_max_i64 as *const u8),
        (
            "gos_rt_metrics_counter_inc",
            crate::c_abi::gos_rt_metrics_counter_inc as *const u8,
        ),
        (
            "gos_rt_metrics_counter_new",
            crate::c_abi::gos_rt_metrics_counter_new as *const u8,
        ),
        (
            "gos_rt_metrics_counter_value",
            crate::c_abi::gos_rt_metrics_counter_value as *const u8,
        ),
        (
            "gos_rt_metrics_gauge_dec",
            crate::c_abi::gos_rt_metrics_gauge_dec as *const u8,
        ),
        (
            "gos_rt_metrics_gauge_inc",
            crate::c_abi::gos_rt_metrics_gauge_inc as *const u8,
        ),
        (
            "gos_rt_metrics_gauge_new",
            crate::c_abi::gos_rt_metrics_gauge_new as *const u8,
        ),
        (
            "gos_rt_metrics_gauge_set",
            crate::c_abi::gos_rt_metrics_gauge_set as *const u8,
        ),
        (
            "gos_rt_metrics_gauge_value",
            crate::c_abi::gos_rt_metrics_gauge_value as *const u8,
        ),
        (
            "gos_rt_metrics_histogram_count",
            crate::c_abi::gos_rt_metrics_histogram_count as *const u8,
        ),
        (
            "gos_rt_metrics_histogram_new",
            crate::c_abi::gos_rt_metrics_histogram_new as *const u8,
        ),
        (
            "gos_rt_metrics_histogram_observe",
            crate::c_abi::gos_rt_metrics_histogram_observe as *const u8,
        ),
        (
            "gos_rt_metrics_histogram_sum",
            crate::c_abi::gos_rt_metrics_histogram_sum as *const u8,
        ),
        (
            "gos_rt_metrics_registry_new",
            crate::c_abi::gos_rt_metrics_registry_new as *const u8,
        ),
        (
            "gos_rt_metrics_registry_register",
            crate::c_abi::gos_rt_metrics_registry_register as *const u8,
        ),
        (
            "gos_rt_metrics_registry_render",
            crate::c_abi::gos_rt_metrics_registry_render as *const u8,
        ),
        (
            "gos_rt_metrics_serve",
            crate::c_abi::gos_rt_metrics_serve as *const u8,
        ),
        (
            "gos_rt_middleware_new",
            crate::c_abi::gos_rt_middleware_new as *const u8,
        ),
        (
            "gos_rt_middleware_serve",
            crate::c_abi::gos_rt_middleware_serve as *const u8,
        ),
        (
            "gos_rt_mime_boundary",
            crate::c_abi::gos_rt_mime_boundary as *const u8,
        ),
        (
            "gos_rt_mime_charset",
            crate::c_abi::gos_rt_mime_charset as *const u8,
        ),
        (
            "gos_rt_mime_extension_by_type",
            crate::c_abi::gos_rt_mime_extension_by_type as *const u8,
        ),
        (
            "gos_rt_mime_is_valid",
            crate::c_abi::gos_rt_mime_is_valid as *const u8,
        ),
        (
            "gos_rt_mime_param",
            crate::c_abi::gos_rt_mime_param as *const u8,
        ),
        (
            "gos_rt_mime_parse",
            crate::c_abi::gos_rt_mime_parse as *const u8,
        ),
        (
            "gos_rt_mime_sub",
            crate::c_abi::gos_rt_mime_sub as *const u8,
        ),
        (
            "gos_rt_mime_top",
            crate::c_abi::gos_rt_mime_top as *const u8,
        ),
        (
            "gos_rt_mime_type_by_extension",
            crate::c_abi::gos_rt_mime_type_by_extension as *const u8,
        ),
        ("gos_rt_min_f64", crate::c_abi::gos_rt_min_f64 as *const u8),
        ("gos_rt_min_i64", crate::c_abi::gos_rt_min_i64 as *const u8),
        (
            "gos_rt_monotonic_ms",
            crate::c_abi::gos_rt_monotonic_ms as *const u8,
        ),
        (
            "gos_rt_monotonic_nanos",
            crate::c_abi::gos_rt_monotonic_nanos as *const u8,
        ),
        (
            "gos_rt_mutex_lock",
            crate::c_abi::gos_rt_mutex_lock as *const u8,
        ),
        (
            "gos_rt_mutex_new",
            crate::c_abi::gos_rt_mutex_new as *const u8,
        ),
        (
            "gos_rt_mutex_unlock",
            crate::c_abi::gos_rt_mutex_unlock as *const u8,
        ),
        (
            "gos_rt_mw_accepts_gzip",
            crate::c_abi::gos_rt_mw_accepts_gzip as *const u8,
        ),
        (
            "gos_rt_mw_decode_basic_auth",
            crate::c_abi::gos_rt_mw_decode_basic_auth as *const u8,
        ),
        (
            "gos_rt_mw_new_request_id",
            crate::c_abi::gos_rt_mw_new_request_id as *const u8,
        ),
        (
            "gos_rt_native_client_get",
            crate::c_abi::gos_rt_native_client_get as *const u8,
        ),
        (
            "gos_rt_native_client_new",
            crate::c_abi::gos_rt_native_client_new as *const u8,
        ),
        (
            "gos_rt_nc_delete",
            crate::c_abi::gos_rt_nc_delete as *const u8,
        ),
        ("gos_rt_nc_get", crate::c_abi::gos_rt_nc_get as *const u8),
        ("gos_rt_nc_post", crate::c_abi::gos_rt_nc_post as *const u8),
        ("gos_rt_nc_put", crate::c_abi::gos_rt_nc_put as *const u8),
        (
            "gos_rt_nested_arr_to_vec",
            crate::c_abi::gos_rt_nested_arr_to_vec as *const u8,
        ),
        (
            "gos_rt_netip_host_of",
            crate::c_abi::gos_rt_netip_host_of as *const u8,
        ),
        (
            "gos_rt_netip_is_loopback",
            crate::c_abi::gos_rt_netip_is_loopback as *const u8,
        ),
        (
            "gos_rt_netip_is_multicast",
            crate::c_abi::gos_rt_netip_is_multicast as *const u8,
        ),
        (
            "gos_rt_netip_is_private",
            crate::c_abi::gos_rt_netip_is_private as *const u8,
        ),
        (
            "gos_rt_netip_is_unspecified",
            crate::c_abi::gos_rt_netip_is_unspecified as *const u8,
        ),
        (
            "gos_rt_netip_is_v4",
            crate::c_abi::gos_rt_netip_is_v4 as *const u8,
        ),
        (
            "gos_rt_netip_is_v6",
            crate::c_abi::gos_rt_netip_is_v6 as *const u8,
        ),
        (
            "gos_rt_netip_is_valid",
            crate::c_abi::gos_rt_netip_is_valid as *const u8,
        ),
        (
            "gos_rt_netip_join_addr_port",
            crate::c_abi::gos_rt_netip_join_addr_port as *const u8,
        ),
        (
            "gos_rt_netip_normalize",
            crate::c_abi::gos_rt_netip_normalize as *const u8,
        ),
        (
            "gos_rt_netip_port_of",
            crate::c_abi::gos_rt_netip_port_of as *const u8,
        ),
        (
            "gos_rt_net_ip_octets",
            crate::c_abi::gos_rt_net_ip_octets as *const u8,
        ),
        (
            "gos_rt_net_ip_parse",
            crate::c_abi::gos_rt_net_ip_parse as *const u8,
        ),
        (
            "gos_rt_net_resolve",
            crate::c_abi::gos_rt_net_resolve as *const u8,
        ),
        ("gos_rt_now_ns", crate::c_abi::gos_rt_now_ns as *const u8),
        (
            "gos_rt_once_call",
            crate::c_abi::gos_rt_once_call as *const u8,
        ),
        (
            "gos_rt_once_new",
            crate::c_abi::gos_rt_once_new as *const u8,
        ),
        (
            "gos_rt_option_default_f64",
            crate::c_abi::gos_rt_option_default_f64 as *const u8,
        ),
        (
            "gos_rt_option_default_i64",
            crate::c_abi::gos_rt_option_default_i64 as *const u8,
        ),
        (
            "gos_rt_option_is_none",
            crate::c_abi::gos_rt_option_is_none as *const u8,
        ),
        (
            "gos_rt_option_is_some",
            crate::c_abi::gos_rt_option_is_some as *const u8,
        ),
        (
            "gos_rt_option_map_i64",
            crate::c_abi::gos_rt_option_map_i64 as *const u8,
        ),
        ("gos_rt_os_arch", crate::c_abi::gos_rt_os_arch as *const u8),
        ("gos_rt_os_args", crate::c_abi::gos_rt_os_args as *const u8),
        ("gos_rt_os_cwd", crate::c_abi::gos_rt_os_cwd as *const u8),
        ("gos_rt_os_env", crate::c_abi::gos_rt_os_env as *const u8),
        (
            "gos_rt_os_exists",
            crate::c_abi::gos_rt_os_exists as *const u8,
        ),
        (
            "gos_rt_os_family",
            crate::c_abi::gos_rt_os_family as *const u8,
        ),
        (
            "gos_rt_os_file_size",
            crate::c_abi::gos_rt_os_file_size as *const u8,
        ),
        (
            "gos_rt_os_is_dir",
            crate::c_abi::gos_rt_os_is_dir as *const u8,
        ),
        (
            "gos_rt_os_is_file",
            crate::c_abi::gos_rt_os_is_file as *const u8,
        ),
        (
            "gos_rt_os_is_symlink",
            crate::c_abi::gos_rt_os_is_symlink as *const u8,
        ),
        (
            "gos_rt_os_mkdir_all_result",
            crate::c_abi::gos_rt_os_mkdir_all_result as *const u8,
        ),
        (
            "gos_rt_os_program_name",
            crate::c_abi::gos_rt_os_program_name as *const u8,
        ),
        (
            "gos_rt_os_read_dir",
            crate::c_abi::gos_rt_os_read_dir as *const u8,
        ),
        (
            "gos_rt_os_remove_dir_all_result",
            crate::c_abi::gos_rt_os_remove_dir_all_result as *const u8,
        ),
        (
            "gos_rt_os_remove_file",
            crate::c_abi::gos_rt_os_remove_file as *const u8,
        ),
        (
            "gos_rt_os_remove_file_result",
            crate::c_abi::gos_rt_os_remove_file_result as *const u8,
        ),
        (
            "gos_rt_os_set_env",
            crate::c_abi::gos_rt_os_set_env as *const u8,
        ),
        (
            "gos_rt_os_unset_env",
            crate::c_abi::gos_rt_os_unset_env as *const u8,
        ),
        (
            "gos_rt_os_user_current_gid",
            crate::c_abi::gos_rt_os_user_current_gid as *const u8,
        ),
        (
            "gos_rt_os_user_current_home",
            crate::c_abi::gos_rt_os_user_current_home as *const u8,
        ),
        (
            "gos_rt_os_user_current_name",
            crate::c_abi::gos_rt_os_user_current_name as *const u8,
        ),
        (
            "gos_rt_os_user_current_uid",
            crate::c_abi::gos_rt_os_user_current_uid as *const u8,
        ),
        (
            "gos_rt_os_user_lookup_name",
            crate::c_abi::gos_rt_os_user_lookup_name as *const u8,
        ),
        (
            "gos_rt_os_user_lookup_uid",
            crate::c_abi::gos_rt_os_user_lookup_uid as *const u8,
        ),
        (
            "gos_rt_os_write_file_bytes_result",
            crate::c_abi::gos_rt_os_write_file_bytes_result as *const u8,
        ),
        (
            "gos_rt_os_write_file_result",
            crate::c_abi::gos_rt_os_write_file_result as *const u8,
        ),
        ("gos_rt_panic", crate::c_abi::gos_rt_panic as *const u8),
        (
            "gos_rt_panic_oob",
            crate::c_abi::gos_rt_panic_oob as *const u8,
        ),
        (
            "gos_rt_parse_f64",
            crate::c_abi::gos_rt_parse_f64 as *const u8,
        ),
        (
            "gos_rt_parse_i64",
            crate::c_abi::gos_rt_parse_i64 as *const u8,
        ),
        (
            "gos_rt_parse_i64_result",
            crate::c_abi::gos_rt_parse_i64_result as *const u8,
        ),
        (
            "gos_rt_path_base",
            crate::c_abi::gos_rt_path_base as *const u8,
        ),
        (
            "gos_rt_path_clean",
            crate::c_abi::gos_rt_path_clean as *const u8,
        ),
        (
            "gos_rt_path_components",
            crate::c_abi::gos_rt_path_components as *const u8,
        ),
        (
            "gos_rt_path_glob",
            crate::c_abi::gos_rt_path_glob as *const u8,
        ),
        (
            "gos_rt_path_matches",
            crate::c_abi::gos_rt_path_matches as *const u8,
        ),
        (
            "gos_rt_path_prefixes",
            crate::c_abi::gos_rt_path_prefixes as *const u8,
        ),
        (
            "gos_rt_path_unique_prefixes",
            crate::c_abi::gos_rt_path_unique_prefixes as *const u8,
        ),
        (
            "gos_rt_path_dir",
            crate::c_abi::gos_rt_path_dir as *const u8,
        ),
        (
            "gos_rt_path_ext",
            crate::c_abi::gos_rt_path_ext as *const u8,
        ),
        (
            "gos_rt_path_file_name",
            crate::c_abi::gos_rt_path_file_name as *const u8,
        ),
        (
            "gos_rt_path_has_prefix",
            crate::c_abi::gos_rt_path_has_prefix as *const u8,
        ),
        (
            "gos_rt_path_is_absolute",
            crate::c_abi::gos_rt_path_is_absolute as *const u8,
        ),
        (
            "gos_rt_path_join",
            crate::c_abi::gos_rt_path_join as *const u8,
        ),
        (
            "gos_rt_path_parent",
            crate::c_abi::gos_rt_path_parent as *const u8,
        ),
        (
            "gos_rt_path_split",
            crate::c_abi::gos_rt_path_split as *const u8,
        ),
        (
            "gos_rt_path_stem",
            crate::c_abi::gos_rt_path_stem as *const u8,
        ),
        (
            "gos_rt_pem_decode_all_raw",
            crate::c_abi::gos_rt_pem_decode_all_raw as *const u8,
        ),
        (
            "gos_rt_pem_decode_raw",
            crate::c_abi::gos_rt_pem_decode_raw as *const u8,
        ),
        (
            "gos_rt_pem_encode_raw",
            crate::c_abi::gos_rt_pem_encode_raw as *const u8,
        ),
        (
            "gos_rt_preempt_check",
            crate::preempt::gos_rt_preempt_check as *const u8,
        ),
        (
            "gos_rt_preempt_check_and_yield",
            crate::preempt::gos_rt_preempt_check_and_yield as *const u8,
        ),
        (
            "gos_rt_print_bool",
            crate::c_abi::gos_rt_print_bool as *const u8,
        ),
        (
            "gos_rt_print_char",
            crate::c_abi::gos_rt_print_char as *const u8,
        ),
        (
            "gos_rt_print_f64",
            crate::c_abi::gos_rt_print_f64 as *const u8,
        ),
        (
            "gos_rt_print_i64",
            crate::c_abi::gos_rt_print_i64 as *const u8,
        ),
        (
            "gos_rt_println_fn_f64",
            crate::c_abi::gos_rt_println_fn_f64 as *const u8,
        ),
        (
            "gos_rt_println_fn_i64",
            crate::c_abi::gos_rt_println_fn_i64 as *const u8,
        ),
        (
            "gos_rt_println_fn_str_word",
            crate::c_abi::gos_rt_println_fn_str_word as *const u8,
        ),
        ("gos_rt_println", crate::c_abi::gos_rt_println as *const u8),
        (
            "gos_rt_print_str",
            crate::c_abi::gos_rt_print_str as *const u8,
        ),
        (
            "gos_rt_print_u64",
            crate::c_abi::gos_rt_print_u64 as *const u8,
        ),
        (
            "gos_rt_process_abort",
            crate::c_abi::gos_rt_process_abort as *const u8,
        ),
        (
            "gos_rt_process_id",
            crate::c_abi::gos_rt_process_id as *const u8,
        ),
        (
            "gos_rt_proxy_forward",
            crate::c_abi::gos_rt_proxy_forward as *const u8,
        ),
        (
            "gos_rt_proxy_forward_url",
            crate::c_abi::gos_rt_proxy_forward_url as *const u8,
        ),
        (
            "gos_rt_proxy_new",
            crate::c_abi::gos_rt_proxy_new as *const u8,
        ),
        (
            "gos_rt_race_access",
            crate::race::gos_rt_race_access as *const u8,
        ),
        (
            "gos_rt_rc_alloc",
            crate::c_abi::gos_rt_rc_alloc as *const u8,
        ),
        (
            "gos_rt_rc_alloc_copy",
            crate::c_abi::rc::gos_rt_rc_alloc_copy as *const u8,
        ),
        (
            "gos_rt_rc_alloc_move",
            crate::c_abi::rc::gos_rt_rc_alloc_move as *const u8,
        ),
        (
            "gos_rt_rc_alloc_reuse",
            crate::c_abi::rc::gos_rt_rc_alloc_reuse as *const u8,
        ),
        (
            "gos_rt_rc_drop_reuse",
            crate::c_abi::rc::gos_rt_rc_drop_reuse as *const u8,
        ),
        (
            "gos_rt_aggr_release_children",
            crate::c_abi::rc::gos_rt_aggr_release_children as *const u8,
        ),
        (
            "gos_rt_aggr_retain_children",
            crate::c_abi::rc::gos_rt_aggr_retain_children as *const u8,
        ),
        (
            "gos_rt_enum_box_aggr",
            crate::c_abi::rc::gos_rt_enum_box_aggr as *const u8,
        ),
        (
            "gos_rt_rc_weak_cell",
            crate::c_abi::rc::gos_rt_rc_weak_cell as *const u8,
        ),
        (
            "gos_rt_rc_retain_children",
            crate::c_abi::rc::gos_rt_rc_retain_children as *const u8,
        ),
        (
            "gos_rt_option_slot_retain",
            crate::c_abi::rc::gos_rt_option_slot_retain as *const u8,
        ),
        (
            "gos_rt_option_slot_release",
            crate::c_abi::rc::gos_rt_option_slot_release as *const u8,
        ),
        (
            "gos_rt_vec_mark_rc_elems",
            crate::c_abi::vec::gos_rt_vec_mark_rc_elems as *const u8,
        ),
        (
            "gos_rt_vec_compact_elems",
            crate::c_abi::vec::gos_rt_vec_compact_elems as *const u8,
        ),
        (
            "gos_rt_vec_mark_vec_elems",
            crate::c_abi::vec::gos_rt_vec_mark_vec_elems as *const u8,
        ),
        (
            "gos_rt_vec_set_elem_meta",
            crate::c_abi::vec::gos_rt_vec_set_elem_meta as *const u8,
        ),
        (
            "gos_rt_vec_set_slot_children",
            crate::c_abi::vec::gos_rt_vec_set_slot_children as *const u8,
        ),
        (
            "gos_rt_vec_borrow_arr",
            crate::c_abi::vec::gos_rt_vec_borrow_arr as *const u8,
        ),
        (
            "gos_rt_vec_borrow_packed_arr",
            crate::c_abi::vec::gos_rt_vec_borrow_packed_arr as *const u8,
        ),
        (
            "gos_rt_map_set_blob_values",
            crate::c_abi::map::gos_rt_map_set_blob_values as *const u8,
        ),
        (
            "gos_rt_map_field_release",
            crate::c_abi::map::gos_rt_map_field_release as *const u8,
        ),
        (
            "gos_rt_map_field_clone",
            crate::c_abi::map::gos_rt_map_field_clone as *const u8,
        ),
        (
            "gos_rt_set_field_clone",
            crate::c_abi::set::gos_rt_set_field_clone as *const u8,
        ),
        (
            "gos_rt_set_field_release",
            crate::c_abi::set::gos_rt_set_field_release as *const u8,
        ),
        (
            "gos_rt_deque_field_clone",
            crate::c_abi::deque::gos_rt_deque_field_clone as *const u8,
        ),
        (
            "gos_rt_deque_field_release",
            crate::c_abi::deque::gos_rt_deque_field_release as *const u8,
        ),
        (
            "gos_rt_bheap_field_clone",
            crate::c_abi::container_heap::gos_rt_bheap_field_clone as *const u8,
        ),
        (
            "gos_rt_bheap_field_release",
            crate::c_abi::container_heap::gos_rt_bheap_field_release as *const u8,
        ),
        (
            "gos_rt_map_set_vec_values",
            crate::c_abi::map::gos_rt_map_set_vec_values as *const u8,
        ),
        (
            "gos_rt_set_panic_hook",
            crate::c_abi::panic::gos_rt_set_panic_hook as *const u8,
        ),
        (
            "gos_rt_rc_alloc_tagged",
            crate::c_abi::rc::gos_rt_rc_alloc_tagged as *const u8,
        ),
        (
            "gos_rt_aggr_zero_guarded",
            crate::c_abi::rc::gos_rt_aggr_zero_guarded as *const u8,
        ),
        (
            "gos_rt_rc_downgrade",
            crate::c_abi::gos_rt_rc_downgrade as *const u8,
        ),
        (
            "gos_rt_rc_mark_shared",
            crate::c_abi::gos_rt_rc_mark_shared as *const u8,
        ),
        (
            "gos_rt_rc_release",
            crate::c_abi::gos_rt_rc_release as *const u8,
        ),
        (
            "gos_rt_rc_retain",
            crate::c_abi::gos_rt_rc_retain as *const u8,
        ),
        (
            "gos_rt_rc_weak_release",
            crate::c_abi::gos_rt_rc_weak_release as *const u8,
        ),
        (
            "gos_rt_rc_weak_retain",
            crate::c_abi::gos_rt_rc_weak_retain as *const u8,
        ),
        (
            "gos_rt_rc_weak_upgrade",
            crate::c_abi::gos_rt_rc_weak_upgrade as *const u8,
        ),
        (
            "gos_rt_rc_weak_upgrade_opt",
            crate::c_abi::gos_rt_rc_weak_upgrade_opt as *const u8,
        ),
        (
            "gos_rt_regex_captures",
            crate::c_abi::gos_rt_regex_captures as *const u8,
        ),
        (
            "gos_rt_regex_captures_all",
            crate::c_abi::gos_rt_regex_captures_all as *const u8,
        ),
        (
            "gos_rt_regex_compile",
            crate::c_abi::gos_rt_regex_compile as *const u8,
        ),
        (
            "gos_rt_regex_compile_result",
            crate::c_abi::gos_rt_regex_compile_result as *const u8,
        ),
        (
            "gos_rt_regex_find",
            crate::c_abi::gos_rt_regex_find as *const u8,
        ),
        (
            "gos_rt_regex_find_all",
            crate::c_abi::gos_rt_regex_find_all as *const u8,
        ),
        (
            "gos_rt_regex_find_opt",
            crate::c_abi::gos_rt_regex_find_opt as *const u8,
        ),
        (
            "gos_rt_regex_is_match",
            crate::c_abi::gos_rt_regex_is_match as *const u8,
        ),
        (
            "gos_rt_regex_count",
            crate::c_abi::gos_rt_regex_count as *const u8,
        ),
        (
            "gos_rt_regex_replace",
            crate::c_abi::gos_rt_regex_replace as *const u8,
        ),
        (
            "gos_rt_regex_replace_all",
            crate::c_abi::gos_rt_regex_replace_all as *const u8,
        ),
        (
            "gos_rt_regex_split",
            crate::c_abi::gos_rt_regex_split as *const u8,
        ),
        (
            "gos_rt_arena_pop",
            crate::c_abi::gos_rt_arena_pop as *const u8,
        ),
        (
            "gos_rt_arena_push",
            crate::c_abi::gos_rt_arena_push as *const u8,
        ),
        (
            "gos_rt_result_dbg",
            crate::c_abi::gos_rt_result_dbg as *const u8,
        ),
        (
            "gos_rt_result_default",
            crate::c_abi::gos_rt_result_default as *const u8,
        ),
        (
            "gos_rt_result_default_f64",
            crate::c_abi::gos_rt_result_default_f64 as *const u8,
        ),
        (
            "gos_rt_result_default_with",
            crate::c_abi::gos_rt_result_default_with as *const u8,
        ),
        (
            "gos_rt_debug_option",
            crate::c_abi::gos_rt_debug_option as *const u8,
        ),
        (
            "gos_rt_debug_option_fmt",
            crate::c_abi::gos_rt_debug_option_fmt as *const u8,
        ),
        (
            "gos_rt_debug_result",
            crate::c_abi::gos_rt_debug_result as *const u8,
        ),
        (
            "gos_rt_debug_result_fmt",
            crate::c_abi::gos_rt_debug_result_fmt as *const u8,
        ),
        (
            "gos_rt_result_disc",
            crate::c_abi::gos_rt_result_disc as *const u8,
        ),
        (
            "gos_rt_result_err",
            crate::c_abi::gos_rt_result_err as *const u8,
        ),
        (
            "gos_rt_result_is_err",
            crate::c_abi::gos_rt_result_is_err as *const u8,
        ),
        (
            "gos_rt_result_is_ok",
            crate::c_abi::gos_rt_result_is_ok as *const u8,
        ),
        (
            "gos_rt_result_map",
            crate::c_abi::gos_rt_result_map as *const u8,
        ),
        (
            "gos_rt_result_map_bare",
            crate::c_abi::gos_rt_result_map_bare as *const u8,
        ),
        (
            "gos_rt_iter_count_by_i64",
            crate::c_abi::gos_rt_iter_count_by_i64 as *const u8,
        ),
        (
            "gos_rt_iter_filter_map_i64",
            crate::c_abi::gos_rt_iter_filter_map_i64 as *const u8,
        ),
        (
            "gos_rt_iter_find_map_i64",
            crate::c_abi::gos_rt_iter_find_map_i64 as *const u8,
        ),
        (
            "gos_rt_iter_flat_map_arr_i64",
            crate::c_abi::gos_rt_iter_flat_map_arr_i64 as *const u8,
        ),
        (
            "gos_rt_iter_flat_map_i64",
            crate::c_abi::gos_rt_iter_flat_map_i64 as *const u8,
        ),
        (
            "gos_rt_iter_group_by_i64",
            crate::c_abi::gos_rt_iter_group_by_i64 as *const u8,
        ),
        (
            "gos_rt_iter_max_by_i64",
            crate::c_abi::gos_rt_iter_max_by_i64 as *const u8,
        ),
        (
            "gos_rt_iter_max_by_key_f64",
            crate::c_abi::gos_rt_iter_max_by_key_f64 as *const u8,
        ),
        (
            "gos_rt_iter_max_by_key_i64",
            crate::c_abi::gos_rt_iter_max_by_key_i64 as *const u8,
        ),
        (
            "gos_rt_iter_max_by_key_ptr",
            crate::c_abi::gos_rt_iter_max_by_key_ptr as *const u8,
        ),
        (
            "gos_rt_iter_min_by_i64",
            crate::c_abi::gos_rt_iter_min_by_i64 as *const u8,
        ),
        (
            "gos_rt_iter_min_by_key_f64",
            crate::c_abi::gos_rt_iter_min_by_key_f64 as *const u8,
        ),
        (
            "gos_rt_iter_min_by_key_i64",
            crate::c_abi::gos_rt_iter_min_by_key_i64 as *const u8,
        ),
        (
            "gos_rt_iter_min_by_key_ptr",
            crate::c_abi::gos_rt_iter_min_by_key_ptr as *const u8,
        ),
        (
            "gos_rt_iter_partition_i64",
            crate::c_abi::gos_rt_iter_partition_i64 as *const u8,
        ),
        (
            "gos_rt_iter_position_i64",
            crate::c_abi::gos_rt_iter_position_i64 as *const u8,
        ),
        (
            "gos_rt_iter_position_ptr",
            crate::c_abi::gos_rt_iter_position_ptr as *const u8,
        ),
        (
            "gos_rt_iter_product_by_i64",
            crate::c_abi::gos_rt_iter_product_by_i64 as *const u8,
        ),
        (
            "gos_rt_iter_reduce_i64",
            crate::c_abi::gos_rt_iter_reduce_i64 as *const u8,
        ),
        (
            "gos_rt_iter_scan_i64",
            crate::c_abi::gos_rt_iter_scan_i64 as *const u8,
        ),
        (
            "gos_rt_iter_skip_while_i64",
            crate::c_abi::gos_rt_iter_skip_while_i64 as *const u8,
        ),
        (
            "gos_rt_iter_sorted_by_i64",
            crate::c_abi::gos_rt_iter_sorted_by_i64 as *const u8,
        ),
        (
            "gos_rt_iter_sorted_by_key_f64",
            crate::c_abi::gos_rt_iter_sorted_by_key_f64 as *const u8,
        ),
        (
            "gos_rt_iter_sorted_by_key_i64",
            crate::c_abi::gos_rt_iter_sorted_by_key_i64 as *const u8,
        ),
        (
            "gos_rt_iter_take_while_i64",
            crate::c_abi::gos_rt_iter_take_while_i64 as *const u8,
        ),
        (
            "gos_rt_option_and_then",
            crate::c_abi::gos_rt_option_and_then as *const u8,
        ),
        (
            "gos_rt_option_default_with",
            crate::c_abi::gos_rt_option_default_with as *const u8,
        ),
        (
            "gos_rt_option_filter",
            crate::c_abi::gos_rt_option_filter as *const u8,
        ),
        (
            "gos_rt_option_flatten",
            crate::c_abi::gos_rt_option_flatten as *const u8,
        ),
        (
            "gos_rt_option_iter",
            crate::c_abi::gos_rt_option_iter as *const u8,
        ),
        (
            "gos_rt_option_or",
            crate::c_abi::gos_rt_option_or as *const u8,
        ),
        (
            "gos_rt_option_or_else",
            crate::c_abi::gos_rt_option_or_else as *const u8,
        ),
        (
            "gos_rt_option_zip",
            crate::c_abi::gos_rt_option_zip as *const u8,
        ),
        (
            "gos_rt_result_and_then",
            crate::c_abi::gos_rt_result_and_then as *const u8,
        ),
        (
            "gos_rt_result_or_else",
            crate::c_abi::gos_rt_result_or_else as *const u8,
        ),
        (
            "gos_rt_result_to_opt_err",
            crate::c_abi::gos_rt_result_to_opt_err as *const u8,
        ),
        (
            "gos_rt_result_to_opt_ok",
            crate::c_abi::gos_rt_result_to_opt_ok as *const u8,
        ),
        (
            "gos_rt_result_map_err",
            crate::c_abi::gos_rt_result_map_err as *const u8,
        ),
        (
            "gos_rt_result_map_err_bare",
            crate::c_abi::gos_rt_result_map_err_bare as *const u8,
        ),
        (
            "gos_rt_result_map_i64",
            crate::c_abi::gos_rt_result_map_i64 as *const u8,
        ),
        (
            "gos_rt_result_new",
            crate::c_abi::gos_rt_result_new as *const u8,
        ),
        (
            "gos_rt_result_new_owned",
            crate::c_abi::gos_rt_result_new_owned as *const u8,
        ),
        (
            "gos_rt_result_new_f64",
            crate::c_abi::gos_rt_result_new_f64 as *const u8,
        ),
        (
            "gos_rt_result_ok",
            crate::c_abi::gos_rt_result_ok as *const u8,
        ),
        (
            "gos_rt_result_ok_or",
            crate::c_abi::gos_rt_result_ok_or as *const u8,
        ),
        (
            "gos_rt_result_ok_or_else",
            crate::c_abi::gos_rt_result_ok_or_else as *const u8,
        ),
        (
            "gos_rt_result_payload",
            crate::c_abi::gos_rt_result_payload as *const u8,
        ),
        (
            "gos_rt_result_payload_f64",
            crate::c_abi::gos_rt_result_payload_f64 as *const u8,
        ),
        (
            "gos_rt_result_payload_i128",
            crate::c_abi::gos_rt_result_payload_i128 as *const u8,
        ),
        (
            "gos_rt_option_unwrap",
            crate::c_abi::gos_rt_option_unwrap as *const u8,
        ),
        (
            "gos_rt_result_unwrap",
            crate::c_abi::gos_rt_result_unwrap as *const u8,
        ),
        (
            "gos_rt_result_unwrap_or",
            crate::c_abi::gos_rt_result_unwrap_or as *const u8,
        ),
        (
            "gos_rt_result_unwrap_or_vec",
            crate::c_abi::vec::gos_rt_result_unwrap_or_vec as *const u8,
        ),
        (
            "gos_rt_result_unwrap_or_carrier",
            crate::c_abi::vec::gos_rt_result_unwrap_or_carrier as *const u8,
        ),
        (
            "gos_rt_router_add",
            crate::c_abi::gos_rt_router_add as *const u8,
        ),
        (
            "gos_rt_router_add_fn",
            crate::c_abi::gos_rt_router_add_fn as *const u8,
        ),
        (
            "gos_rt_router_add_pattern",
            crate::c_abi::gos_rt_router_add_pattern as *const u8,
        ),
        (
            "gos_rt_router_lookup",
            crate::c_abi::gos_rt_router_lookup as *const u8,
        ),
        (
            "gos_rt_router_delete",
            crate::c_abi::gos_rt_router_delete as *const u8,
        ),
        (
            "gos_rt_router_delete_fn",
            crate::c_abi::gos_rt_router_delete_fn as *const u8,
        ),
        (
            "gos_rt_router_get",
            crate::c_abi::gos_rt_router_get as *const u8,
        ),
        (
            "gos_rt_router_get_fn",
            crate::c_abi::gos_rt_router_get_fn as *const u8,
        ),
        (
            "gos_rt_router_head",
            crate::c_abi::gos_rt_router_head as *const u8,
        ),
        (
            "gos_rt_router_head_fn",
            crate::c_abi::gos_rt_router_head_fn as *const u8,
        ),
        (
            "gos_rt_router_new",
            crate::c_abi::gos_rt_router_new as *const u8,
        ),
        (
            "gos_rt_router_options",
            crate::c_abi::gos_rt_router_options as *const u8,
        ),
        (
            "gos_rt_router_options_fn",
            crate::c_abi::gos_rt_router_options_fn as *const u8,
        ),
        (
            "gos_rt_router_patch",
            crate::c_abi::gos_rt_router_patch as *const u8,
        ),
        (
            "gos_rt_router_patch_fn",
            crate::c_abi::gos_rt_router_patch_fn as *const u8,
        ),
        (
            "gos_rt_router_post",
            crate::c_abi::gos_rt_router_post as *const u8,
        ),
        (
            "gos_rt_router_post_fn",
            crate::c_abi::gos_rt_router_post_fn as *const u8,
        ),
        (
            "gos_rt_router_put",
            crate::c_abi::gos_rt_router_put as *const u8,
        ),
        (
            "gos_rt_router_put_fn",
            crate::c_abi::gos_rt_router_put_fn as *const u8,
        ),
        (
            "gos_rt_router_serve",
            crate::c_abi::gos_rt_router_serve as *const u8,
        ),
        (
            "gos_rt_pprof_cpu_profile",
            crate::c_abi::gos_rt_pprof_cpu_profile as *const u8,
        ),
        (
            "gos_rt_pprof_heap_profile",
            crate::c_abi::gos_rt_pprof_heap_profile as *const u8,
        ),
        (
            "gos_rt_pprof_goroutine_profile",
            crate::c_abi::gos_rt_pprof_goroutine_profile as *const u8,
        ),
        (
            "gos_rt_pprof_mutex_profile",
            crate::c_abi::gos_rt_pprof_mutex_profile as *const u8,
        ),
        (
            "gos_rt_pprof_block_profile",
            crate::c_abi::gos_rt_pprof_block_profile as *const u8,
        ),
        (
            "gos_rt_pprof_execution_trace",
            crate::c_abi::gos_rt_pprof_execution_trace as *const u8,
        ),
        (
            "gos_rt_pprof_route",
            crate::c_abi::gos_rt_pprof_route as *const u8,
        ),
        (
            "gos_rt_runtime_scheduler_stats_json",
            crate::c_abi::gos_rt_runtime_scheduler_stats_json as *const u8,
        ),
        (
            "gos_rt_runtime_cycle_collection_supported",
            crate::c_abi::gos_rt_runtime_cycle_collection_supported as *const u8,
        ),
        (
            "gos_rt_rwlock_get",
            crate::c_abi::gos_rt_rwlock_get as *const u8,
        ),
        (
            "gos_rt_rwlock_new",
            crate::c_abi::gos_rt_rwlock_new as *const u8,
        ),
        (
            "gos_rt_rwlock_set",
            crate::c_abi::gos_rt_rwlock_set as *const u8,
        ),
        (
            "gos_rt_rwlock_with_read",
            crate::c_abi::gos_rt_rwlock_with_read as *const u8,
        ),
        (
            "gos_rt_rwlock_with_write",
            crate::c_abi::gos_rt_rwlock_with_write as *const u8,
        ),
        (
            "gos_rt_shared_get",
            crate::c_abi::gos_rt_shared_get as *const u8,
        ),
        (
            "gos_rt_shared_new",
            crate::c_abi::gos_rt_shared_new as *const u8,
        ),
        (
            "gos_rt_shared_set",
            crate::c_abi::gos_rt_shared_set as *const u8,
        ),
        (
            "gos_rt_shared_update",
            crate::c_abi::gos_rt_shared_update as *const u8,
        ),
        (
            "gos_rt_shared_with",
            crate::c_abi::gos_rt_shared_with as *const u8,
        ),
        (
            "gos_rt_select_arm_default",
            crate::c_abi::gos_rt_select_arm_default as *const u8,
        ),
        (
            "gos_rt_select_arm_recv",
            crate::c_abi::gos_rt_select_arm_recv as *const u8,
        ),
        (
            "gos_rt_select_arm_send",
            crate::c_abi::gos_rt_select_arm_send as *const u8,
        ),
        (
            "gos_rt_select_free",
            crate::c_abi::gos_rt_select_free as *const u8,
        ),
        (
            "gos_rt_select_new",
            crate::c_abi::gos_rt_select_new as *const u8,
        ),
        (
            "gos_rt_select_value",
            crate::c_abi::gos_rt_select_value as *const u8,
        ),
        (
            "gos_rt_select_wait",
            crate::c_abi::gos_rt_select_wait as *const u8,
        ),
        (
            "gos_rt_set_args",
            crate::c_abi::gos_rt_set_args as *const u8,
        ),
        (
            "gos_rt_set_assign",
            crate::c_abi::gos_rt_set_assign as *const u8,
        ),
        (
            "gos_rt_set_clear",
            crate::c_abi::gos_rt_set_clear as *const u8,
        ),
        (
            "gos_rt_set_clone",
            crate::c_abi::gos_rt_set_clone as *const u8,
        ),
        (
            "gos_rt_set_contains",
            crate::c_abi::gos_rt_set_contains as *const u8,
        ),
        (
            "gos_rt_set_contains_i64",
            crate::c_abi::gos_rt_set_contains_i64 as *const u8,
        ),
        (
            "gos_rt_set_contains_skey",
            crate::c_abi::gos_rt_set_contains_skey as *const u8,
        ),
        (
            "gos_rt_btree_set_new",
            crate::c_abi::gos_rt_btree_set_new as *const u8,
        ),
        (
            "gos_rt_set_format_desc",
            crate::c_abi::gos_rt_set_format_desc as *const u8,
        ),
        (
            "gos_rt_set_format_i64",
            crate::c_abi::gos_rt_set_format_i64 as *const u8,
        ),
        (
            "gos_rt_set_format_string",
            crate::c_abi::gos_rt_set_format_string as *const u8,
        ),
        (
            "gos_rt_set_format_u64",
            crate::c_abi::gos_rt_set_format_u64 as *const u8,
        ),
        ("gos_rt_set_eq", crate::c_abi::gos_rt_set_eq as *const u8),
        (
            "gos_rt_set_free",
            crate::c_abi::gos_rt_set_free as *const u8,
        ),
        (
            "gos_rt_set_insert",
            crate::c_abi::gos_rt_set_insert as *const u8,
        ),
        (
            "gos_rt_set_insert_i64",
            crate::c_abi::gos_rt_set_insert_i64 as *const u8,
        ),
        (
            "gos_rt_set_insert_skey",
            crate::c_abi::gos_rt_set_insert_skey as *const u8,
        ),
        ("gos_rt_set_len", crate::c_abi::gos_rt_set_len as *const u8),
        ("gos_rt_set_new", crate::c_abi::gos_rt_set_new as *const u8),
        (
            "gos_rt_set_remove",
            crate::c_abi::gos_rt_set_remove as *const u8,
        ),
        (
            "gos_rt_set_remove_i64",
            crate::c_abi::gos_rt_set_remove_i64 as *const u8,
        ),
        (
            "gos_rt_set_remove_skey",
            crate::c_abi::gos_rt_set_remove_skey as *const u8,
        ),
        (
            "gos_rt_set_to_vec",
            crate::c_abi::gos_rt_set_to_vec as *const u8,
        ),
        (
            "gos_rt_set_to_vec_i64",
            crate::c_abi::gos_rt_set_to_vec_i64 as *const u8,
        ),
        (
            "gos_rt_set_to_vec_skey",
            crate::c_abi::gos_rt_set_to_vec_skey as *const u8,
        ),
        (
            "gos_rt_set_union",
            crate::c_abi::gos_rt_set_union as *const u8,
        ),
        (
            "gos_rt_set_intersection",
            crate::c_abi::gos_rt_set_intersection as *const u8,
        ),
        (
            "gos_rt_set_intersection_skey",
            crate::c_abi::gos_rt_set_intersection_skey as *const u8,
        ),
        (
            "gos_rt_set_intersection_to_vec",
            crate::c_abi::gos_rt_set_intersection_to_vec as *const u8,
        ),
        (
            "gos_rt_set_intersection_to_vec_i64",
            crate::c_abi::gos_rt_set_intersection_to_vec_i64 as *const u8,
        ),
        (
            "gos_rt_set_intersection_to_vec_skey",
            crate::c_abi::gos_rt_set_intersection_to_vec_skey as *const u8,
        ),
        (
            "gos_rt_set_difference",
            crate::c_abi::gos_rt_set_difference as *const u8,
        ),
        (
            "gos_rt_set_symmetric_difference",
            crate::c_abi::gos_rt_set_symmetric_difference as *const u8,
        ),
        (
            "gos_rt_set_is_subset",
            crate::c_abi::gos_rt_set_is_subset as *const u8,
        ),
        (
            "gos_rt_set_is_superset",
            crate::c_abi::gos_rt_set_is_superset as *const u8,
        ),
        (
            "gos_rt_set_is_disjoint",
            crate::c_abi::gos_rt_set_is_disjoint as *const u8,
        ),
        (
            "gos_rt_sha256_hex",
            crate::c_abi::gos_rt_sha256_hex as *const u8,
        ),
        (
            "gos_rt_sha512_hex",
            crate::c_abi::gos_rt_sha512_hex as *const u8,
        ),
        (
            "gos_rt_signal_on",
            crate::c_abi::gos_rt_signal_on as *const u8,
        ),
        (
            "gos_rt_signal_try_wait",
            crate::c_abi::gos_rt_signal_try_wait as *const u8,
        ),
        (
            "gos_rt_signal_wait",
            crate::c_abi::gos_rt_signal_wait as *const u8,
        ),
        (
            "gos_rt_sleep_ms",
            crate::c_abi::gos_rt_sleep_ms as *const u8,
        ),
        (
            "gos_rt_sleep_ms_ctx",
            crate::c_abi::gos_rt_sleep_ms_ctx as *const u8,
        ),
        (
            "gos_rt_sleep_ns",
            crate::c_abi::gos_rt_sleep_ns as *const u8,
        ),
        (
            "gos_rt_slog_debug",
            crate::c_abi::gos_rt_slog_debug as *const u8,
        ),
        (
            "gos_rt_slog_error",
            crate::c_abi::gos_rt_slog_error as *const u8,
        ),
        (
            "gos_rt_slog_info",
            crate::c_abi::gos_rt_slog_info as *const u8,
        ),
        (
            "gos_rt_slog_warn",
            crate::c_abi::gos_rt_slog_warn as *const u8,
        ),
        ("gos_rt_spawn", crate::c_abi::gos_rt_spawn as *const u8),
        (
            "gos_rt_spawn_ex",
            crate::c_abi::gos_rt_spawn_ex as *const u8,
        ),
        (
            "gos_rt_cohort_push",
            crate::c_abi::cohort::gos_rt_cohort_push as *const u8,
        ),
        (
            "gos_rt_cohort_join",
            crate::c_abi::cohort::gos_rt_cohort_join as *const u8,
        ),
        (
            "gos_rt_cohort_pop",
            crate::c_abi::cohort::gos_rt_cohort_pop as *const u8,
        ),
        (
            "gos_rt_lifecycle_ready",
            crate::c_abi::lifecycle::gos_rt_lifecycle_ready as *const u8,
        ),
        (
            "gos_rt_lifecycle_set_ready",
            crate::c_abi::lifecycle::gos_rt_lifecycle_set_ready as *const u8,
        ),
        (
            "gos_rt_lifecycle_is_ready",
            crate::c_abi::lifecycle::gos_rt_lifecycle_is_ready as *const u8,
        ),
        (
            "gos_rt_lifecycle_shutdown",
            crate::c_abi::lifecycle::gos_rt_lifecycle_shutdown as *const u8,
        ),
        (
            "gos_rt_lifecycle_is_shutting_down",
            crate::c_abi::lifecycle::gos_rt_lifecycle_is_shutting_down as *const u8,
        ),
        (
            "gos_rt_lifecycle_await_shutdown",
            crate::c_abi::lifecycle::gos_rt_lifecycle_await_shutdown as *const u8,
        ),
        (
            "gos_rt_lifecycle_notify_status",
            crate::c_abi::lifecycle::gos_rt_lifecycle_notify_status as *const u8,
        ),
        (
            "gos_rt_http_server_new",
            crate::c_abi::http_server_handle::gos_rt_http_server_new as *const u8,
        ),
        (
            "gos_rt_time_freeze",
            crate::c_abi::gos_rt_time_freeze as *const u8,
        ),
        (
            "gos_rt_smtp_send",
            crate::c_abi::gos_rt_smtp_send as *const u8,
        ),
        (
            "gos_rt_smtp_send_auth",
            crate::c_abi::gos_rt_smtp_send_auth as *const u8,
        ),
        (
            "gos_rt_httptest_record",
            crate::c_abi::testing::gos_rt_httptest_record as *const u8,
        ),
        (
            "gos_rt_time_advance",
            crate::c_abi::gos_rt_time_advance as *const u8,
        ),
        (
            "gos_rt_time_unfreeze",
            crate::c_abi::gos_rt_time_unfreeze as *const u8,
        ),
        (
            "gos_rt_time_is_frozen",
            crate::c_abi::gos_rt_time_is_frozen as *const u8,
        ),
        (
            "gos_rt_http_response_stream_open",
            crate::c_abi::http_stream_writer::gos_rt_http_response_stream_open as *const u8,
        ),
        (
            "gos_rt_http_response_stream_write",
            crate::c_abi::http_stream_writer::gos_rt_http_response_stream_write as *const u8,
        ),
        (
            "gos_rt_http_response_stream_write_bytes",
            crate::c_abi::http_stream_writer::gos_rt_http_response_stream_write_bytes as *const u8,
        ),
        (
            "gos_rt_http_response_stream_close",
            crate::c_abi::http_stream_writer::gos_rt_http_response_stream_close as *const u8,
        ),
        (
            "gos_rt_http_response_stream_is_open",
            crate::c_abi::http_stream_writer::gos_rt_http_response_stream_is_open as *const u8,
        ),
        (
            "gos_rt_http_server_read_header_timeout_ms",
            crate::c_abi::http_server_handle::gos_rt_http_server_read_header_timeout_ms
                as *const u8,
        ),
        (
            "gos_rt_http_server_request_timeout_ms",
            crate::c_abi::http_server_handle::gos_rt_http_server_request_timeout_ms as *const u8,
        ),
        (
            "gos_rt_http_server_read_body_timeout_ms",
            crate::c_abi::http_server_handle::gos_rt_http_server_read_body_timeout_ms as *const u8,
        ),
        (
            "gos_rt_http_server_write_timeout_ms",
            crate::c_abi::http_server_handle::gos_rt_http_server_write_timeout_ms as *const u8,
        ),
        (
            "gos_rt_http_server_idle_timeout_ms",
            crate::c_abi::http_server_handle::gos_rt_http_server_idle_timeout_ms as *const u8,
        ),
        (
            "gos_rt_http_server_max_header_bytes",
            crate::c_abi::http_server_handle::gos_rt_http_server_max_header_bytes as *const u8,
        ),
        (
            "gos_rt_http_server_max_body_bytes",
            crate::c_abi::http_server_handle::gos_rt_http_server_max_body_bytes as *const u8,
        ),
        (
            "gos_rt_http_server_max_connections",
            crate::c_abi::http_server_handle::gos_rt_http_server_max_connections as *const u8,
        ),
        (
            "gos_rt_http_server_server_name",
            crate::c_abi::http_server_handle::gos_rt_http_server_server_name as *const u8,
        ),
        (
            "gos_rt_http_server_listen",
            crate::c_abi::http_server_handle::gos_rt_http_server_listen as *const u8,
        ),
        (
            "gos_rt_http_server_addr",
            crate::c_abi::http_server_handle::gos_rt_http_server_addr as *const u8,
        ),
        (
            "gos_rt_http_server_serve",
            crate::c_abi::http_server_handle::gos_rt_http_server_serve as *const u8,
        ),
        (
            "gos_rt_http_server_shutdown",
            crate::c_abi::http_server_handle::gos_rt_http_server_shutdown as *const u8,
        ),
        (
            "gos_rt_cohorts",
            crate::c_abi::cohort::gos_rt_cohorts as *const u8,
        ),
        (
            "gos_rt_cohort_root",
            crate::c_abi::cohort::gos_rt_cohort_root as *const u8,
        ),
        (
            "gos_rt_cohort_cancelled",
            crate::c_abi::cohort::gos_rt_cohort_cancelled as *const u8,
        ),
        (
            "gos_rt_cohort_cancel",
            crate::c_abi::cohort::gos_rt_cohort_cancel as *const u8,
        ),
        (
            "gos_rt_sort_binary_search_f64",
            crate::c_abi::gos_rt_sort_binary_search_f64 as *const u8,
        ),
        (
            "gos_rt_sort_binary_search_i64",
            crate::c_abi::gos_rt_sort_binary_search_i64 as *const u8,
        ),
        (
            "gos_rt_sort_binary_search_str",
            crate::c_abi::gos_rt_sort_binary_search_str as *const u8,
        ),
        (
            "gos_rt_sort_partition_point_f64",
            crate::c_abi::gos_rt_sort_partition_point_f64 as *const u8,
        ),
        (
            "gos_rt_sort_partition_point_i64",
            crate::c_abi::gos_rt_sort_partition_point_i64 as *const u8,
        ),
        (
            "gos_rt_sort_partition_point_str",
            crate::c_abi::gos_rt_sort_partition_point_str as *const u8,
        ),
        (
            "gos_rt_sort_stable_f64",
            crate::c_abi::gos_rt_sort_stable_f64 as *const u8,
        ),
        (
            "gos_rt_sort_stable_i64",
            crate::c_abi::gos_rt_sort_stable_i64 as *const u8,
        ),
        (
            "gos_rt_sort_stable_str",
            crate::c_abi::gos_rt_sort_stable_str as *const u8,
        ),
        (
            "gos_rt_sort_stable_aggr",
            crate::c_abi::gos_rt_sort_stable_aggr as *const u8,
        ),
        (
            "gos_rt_sort_binary_search_aggr",
            crate::c_abi::gos_rt_sort_binary_search_aggr as *const u8,
        ),
        (
            "gos_rt_sort_partition_point_aggr",
            crate::c_abi::gos_rt_sort_partition_point_aggr as *const u8,
        ),
        (
            "gos_rt_sql_conn_begin",
            crate::c_abi::gos_rt_sql_conn_begin as *const u8,
        ),
        (
            "gos_rt_sql_conn_begin_with",
            crate::c_abi::gos_rt_sql_conn_begin_with as *const u8,
        ),
        (
            "gos_rt_sql_conn_execute",
            crate::c_abi::gos_rt_sql_conn_execute as *const u8,
        ),
        (
            "gos_rt_sql_conn_interrupt",
            crate::c_abi::gos_rt_sql_conn_interrupt as *const u8,
        ),
        (
            "gos_rt_sql_conn_ping",
            crate::c_abi::gos_rt_sql_conn_ping as *const u8,
        ),
        (
            "gos_rt_sql_conn_query",
            crate::c_abi::gos_rt_sql_conn_query as *const u8,
        ),
        (
            "gos_rt_sql_conn_set_busy_timeout",
            crate::c_abi::gos_rt_sql_conn_set_busy_timeout as *const u8,
        ),
        (
            "gos_rt_sql_drivers",
            crate::c_abi::gos_rt_sql_drivers as *const u8,
        ),
        (
            "gos_rt_sql_conn_copy_in",
            crate::c_abi::gos_rt_sql_conn_copy_in as *const u8,
        ),
        (
            "gos_rt_sql_conn_copy_out_run",
            crate::c_abi::gos_rt_sql_conn_copy_out_run as *const u8,
        ),
        (
            "gos_rt_sql_conn_copy_out_take",
            crate::c_abi::gos_rt_sql_conn_copy_out_take as *const u8,
        ),
        (
            "gos_rt_sql_conn_listen",
            crate::c_abi::gos_rt_sql_conn_listen as *const u8,
        ),
        (
            "gos_rt_sql_conn_poll_notification",
            crate::c_abi::gos_rt_sql_conn_poll_notification as *const u8,
        ),
        (
            "gos_rt_sql_conn_prepare",
            crate::c_abi::gos_rt_sql_conn_prepare as *const u8,
        ),
        (
            "gos_rt_sql_conn_unlisten",
            crate::c_abi::gos_rt_sql_conn_unlisten as *const u8,
        ),
        (
            "gos_rt_sql_migrate_up",
            crate::c_abi::gos_rt_sql_migrate_up as *const u8,
        ),
        (
            "gos_rt_sql_register_native",
            crate::c_abi::gos_rt_sql_register_native as *const u8,
        ),
        (
            "gos_rt_sql_native_url",
            crate::c_abi::gos_rt_sql_native_url as *const u8,
        ),
        (
            "gos_rt_sql_native_sql",
            crate::c_abi::gos_rt_sql_native_sql as *const u8,
        ),
        (
            "gos_rt_sql_native_parent",
            crate::c_abi::gos_rt_sql_native_parent as *const u8,
        ),
        (
            "gos_rt_sql_native_out_handle",
            crate::c_abi::gos_rt_sql_native_out_handle as *const u8,
        ),
        (
            "gos_rt_sql_native_iso",
            crate::c_abi::gos_rt_sql_native_iso as *const u8,
        ),
        (
            "gos_rt_sql_native_timeout",
            crate::c_abi::gos_rt_sql_native_timeout as *const u8,
        ),
        (
            "gos_rt_sql_native_channel",
            crate::c_abi::gos_rt_sql_native_channel as *const u8,
        ),
        (
            "gos_rt_sql_native_param_count",
            crate::c_abi::gos_rt_sql_native_param_count as *const u8,
        ),
        (
            "gos_rt_sql_native_param",
            crate::c_abi::gos_rt_sql_native_param as *const u8,
        ),
        (
            "gos_rt_sql_native_data",
            crate::c_abi::gos_rt_sql_native_data as *const u8,
        ),
        (
            "gos_rt_sql_native_push_column",
            crate::c_abi::gos_rt_sql_native_push_column as *const u8,
        ),
        (
            "gos_rt_sql_native_push_value",
            crate::c_abi::gos_rt_sql_native_push_value as *const u8,
        ),
        (
            "gos_rt_sql_native_row_ready",
            crate::c_abi::gos_rt_sql_native_row_ready as *const u8,
        ),
        (
            "gos_rt_sql_native_set_error",
            crate::c_abi::gos_rt_sql_native_set_error as *const u8,
        ),
        (
            "gos_rt_sql_native_emit_bytes",
            crate::c_abi::gos_rt_sql_native_emit_bytes as *const u8,
        ),
        (
            "gos_rt_sql_native_set_notification",
            crate::c_abi::gos_rt_sql_native_set_notification as *const u8,
        ),
        (
            "gos_rt_sql_native_set_handle",
            crate::c_abi::gos_rt_sql_native_set_handle as *const u8,
        ),
        (
            "gos_rt_sql_native_handle",
            crate::c_abi::gos_rt_sql_native_handle as *const u8,
        ),
        (
            "gos_rt_sql_native_value_null",
            crate::c_abi::gos_rt_sql_native_value_null as *const u8,
        ),
        (
            "gos_rt_sql_native_value_bool",
            crate::c_abi::gos_rt_sql_native_value_bool as *const u8,
        ),
        (
            "gos_rt_sql_native_value_int",
            crate::c_abi::gos_rt_sql_native_value_int as *const u8,
        ),
        (
            "gos_rt_sql_native_value_float",
            crate::c_abi::gos_rt_sql_native_value_float as *const u8,
        ),
        (
            "gos_rt_sql_native_value_text",
            crate::c_abi::gos_rt_sql_native_value_text as *const u8,
        ),
        (
            "gos_rt_sql_native_value_blob",
            crate::c_abi::gos_rt_sql_native_value_blob as *const u8,
        ),
        (
            "gos_rt_sql_native_value_kind",
            crate::c_abi::gos_rt_sql_native_value_kind as *const u8,
        ),
        (
            "gos_rt_sql_native_value_int_of",
            crate::c_abi::gos_rt_sql_native_value_int_of as *const u8,
        ),
        (
            "gos_rt_sql_native_value_float_of",
            crate::c_abi::gos_rt_sql_native_value_float_of as *const u8,
        ),
        (
            "gos_rt_sql_native_value_text_of",
            crate::c_abi::gos_rt_sql_native_value_text_of as *const u8,
        ),
        (
            "gos_rt_sql_native_value_blob_of",
            crate::c_abi::gos_rt_sql_native_value_blob_of as *const u8,
        ),
        (
            "gos_rt_sql_notification_channel",
            crate::c_abi::gos_rt_sql_notification_channel as *const u8,
        ),
        (
            "gos_rt_sql_notification_payload",
            crate::c_abi::gos_rt_sql_notification_payload as *const u8,
        ),
        (
            "gos_rt_sql_notification_pid",
            crate::c_abi::gos_rt_sql_notification_pid as *const u8,
        ),
        (
            "gos_rt_sql_pool_close_idle",
            crate::c_abi::gos_rt_sql_pool_close_idle as *const u8,
        ),
        (
            "gos_rt_sql_pool_get",
            crate::c_abi::gos_rt_sql_pool_get as *const u8,
        ),
        (
            "gos_rt_sql_pool_idle",
            crate::c_abi::gos_rt_sql_pool_idle as *const u8,
        ),
        (
            "gos_rt_sql_pool_live",
            crate::c_abi::gos_rt_sql_pool_live as *const u8,
        ),
        (
            "gos_rt_sql_pool_new",
            crate::c_abi::gos_rt_sql_pool_new as *const u8,
        ),
        (
            "gos_rt_sql_stmt_close",
            crate::c_abi::gos_rt_sql_stmt_close as *const u8,
        ),
        (
            "gos_rt_sql_stmt_execute",
            crate::c_abi::gos_rt_sql_stmt_execute as *const u8,
        ),
        (
            "gos_rt_sql_stmt_query",
            crate::c_abi::gos_rt_sql_stmt_query as *const u8,
        ),
        (
            "gos_rt_sql_tx_execute_params",
            crate::c_abi::gos_rt_sql_tx_execute_params as *const u8,
        ),
        (
            "gos_rt_sql_tx_query_params",
            crate::c_abi::gos_rt_sql_tx_query_params as *const u8,
        ),
        (
            "gos_rt_sql_conn_close",
            crate::c_abi::gos_rt_sql_conn_close as *const u8,
        ),
        (
            "gos_rt_sql_conn_execute_params",
            crate::c_abi::gos_rt_sql_conn_execute_params as *const u8,
        ),
        (
            "gos_rt_sql_conn_query_params",
            crate::c_abi::gos_rt_sql_conn_query_params as *const u8,
        ),
        (
            "gos_rt_sql_last_error",
            crate::c_abi::gos_rt_sql_last_error as *const u8,
        ),
        (
            "gos_rt_sql_params_new",
            crate::c_abi::gos_rt_sql_params_new as *const u8,
        ),
        (
            "gos_rt_sql_params_push_blob",
            crate::c_abi::gos_rt_sql_params_push_blob as *const u8,
        ),
        (
            "gos_rt_sql_params_push_bool",
            crate::c_abi::gos_rt_sql_params_push_bool as *const u8,
        ),
        (
            "gos_rt_sql_params_push_float",
            crate::c_abi::gos_rt_sql_params_push_float as *const u8,
        ),
        (
            "gos_rt_sql_params_push_int",
            crate::c_abi::gos_rt_sql_params_push_int as *const u8,
        ),
        (
            "gos_rt_sql_params_push_null",
            crate::c_abi::gos_rt_sql_params_push_null as *const u8,
        ),
        (
            "gos_rt_sql_params_push_text",
            crate::c_abi::gos_rt_sql_params_push_text as *const u8,
        ),
        (
            "gos_rt_sql_row_get_blob_vec",
            crate::c_abi::gos_rt_sql_row_get_blob_vec as *const u8,
        ),
        (
            "gos_rt_sql_row_get_bool_i64",
            crate::c_abi::gos_rt_sql_row_get_bool_i64 as *const u8,
        ),
        (
            "gos_rt_sql_row_kind",
            crate::c_abi::gos_rt_sql_row_kind as *const u8,
        ),
        (
            "gos_rt_sql_open",
            crate::c_abi::gos_rt_sql_open as *const u8,
        ),
        (
            "gos_rt_sql_row_get_blob",
            crate::c_abi::gos_rt_sql_row_get_blob as *const u8,
        ),
        (
            "gos_rt_sql_row_get_bool",
            crate::c_abi::gos_rt_sql_row_get_bool as *const u8,
        ),
        (
            "gos_rt_sql_row_get_f64",
            crate::c_abi::gos_rt_sql_row_get_f64 as *const u8,
        ),
        (
            "gos_rt_sql_row_get_i64",
            crate::c_abi::gos_rt_sql_row_get_i64 as *const u8,
        ),
        (
            "gos_rt_sql_row_get_opt_bool",
            crate::c_abi::gos_rt_sql_row_get_opt_bool as *const u8,
        ),
        (
            "gos_rt_sql_row_get_opt_f64",
            crate::c_abi::gos_rt_sql_row_get_opt_f64 as *const u8,
        ),
        (
            "gos_rt_sql_row_get_opt_i64",
            crate::c_abi::gos_rt_sql_row_get_opt_i64 as *const u8,
        ),
        (
            "gos_rt_sql_row_get_opt_text",
            crate::c_abi::gos_rt_sql_row_get_opt_text as *const u8,
        ),
        (
            "gos_rt_sql_row_get_text",
            crate::c_abi::gos_rt_sql_row_get_text as *const u8,
        ),
        (
            "gos_rt_sql_row_is_null",
            crate::c_abi::gos_rt_sql_row_is_null as *const u8,
        ),
        (
            "gos_rt_sql_rows_close",
            crate::c_abi::gos_rt_sql_rows_close as *const u8,
        ),
        (
            "gos_rt_sql_rows_columns",
            crate::c_abi::gos_rt_sql_rows_columns as *const u8,
        ),
        (
            "gos_rt_sql_rows_next_row",
            crate::c_abi::gos_rt_sql_rows_next_row as *const u8,
        ),
        (
            "gos_rt_sql_row_width",
            crate::c_abi::gos_rt_sql_row_width as *const u8,
        ),
        (
            "gos_rt_sql_tx_commit",
            crate::c_abi::gos_rt_sql_tx_commit as *const u8,
        ),
        (
            "gos_rt_sql_tx_execute",
            crate::c_abi::gos_rt_sql_tx_execute as *const u8,
        ),
        (
            "gos_rt_sql_tx_release_savepoint",
            crate::c_abi::gos_rt_sql_tx_release_savepoint as *const u8,
        ),
        (
            "gos_rt_sql_tx_rollback",
            crate::c_abi::gos_rt_sql_tx_rollback as *const u8,
        ),
        (
            "gos_rt_sql_tx_rollback_to_savepoint",
            crate::c_abi::gos_rt_sql_tx_rollback_to_savepoint as *const u8,
        ),
        (
            "gos_rt_sql_tx_savepoint",
            crate::c_abi::gos_rt_sql_tx_savepoint as *const u8,
        ),
        (
            "gos_rt_sql_value_bool",
            crate::c_abi::gos_rt_sql_value_bool as *const u8,
        ),
        (
            "gos_rt_sql_value_float",
            crate::c_abi::gos_rt_sql_value_float as *const u8,
        ),
        (
            "gos_rt_sql_value_int",
            crate::c_abi::gos_rt_sql_value_int as *const u8,
        ),
        (
            "gos_rt_sql_value_null",
            crate::c_abi::gos_rt_sql_value_null as *const u8,
        ),
        (
            "gos_rt_sql_value_text",
            crate::c_abi::gos_rt_sql_value_text as *const u8,
        ),
        (
            "gos_rt_sse_encode_comment",
            crate::c_abi::gos_rt_sse_encode_comment as *const u8,
        ),
        (
            "gos_rt_sse_encode_event",
            crate::c_abi::gos_rt_sse_encode_event as *const u8,
        ),
        (
            "gos_rt_sse_encode_retry",
            crate::c_abi::gos_rt_sse_encode_retry as *const u8,
        ),
        (
            "gos_rt_stack_pop",
            crate::c_abi::gos_rt_stack_pop as *const u8,
        ),
        (
            "gos_rt_stack_push",
            crate::c_abi::gos_rt_stack_push as *const u8,
        ),
        (
            "gos_rt_stack_set_line",
            crate::c_abi::gos_rt_stack_set_line as *const u8,
        ),
        (
            "gos_rt_static_mime_for_path",
            crate::c_abi::gos_rt_static_mime_for_path as *const u8,
        ),
        (
            "gos_rt_static_serve_file",
            crate::c_abi::gos_rt_static_serve_file as *const u8,
        ),
        (
            "gos_rt_stdout_acquire",
            crate::c_abi::gos_rt_stdout_acquire as *const u8,
        ),
        (
            "gos_rt_stdout_release",
            crate::c_abi::gos_rt_stdout_release as *const u8,
        ),
        (
            "gos_rt_str_as_bytes",
            crate::c_abi::gos_rt_str_as_bytes as *const u8,
        ),
        (
            "gos_rt_str_clear",
            crate::c_abi::gos_rt_str_clear as *const u8,
        ),
        (
            "gos_rt_str_clone",
            crate::c_abi::gos_rt_str_clone as *const u8,
        ),
        (
            "gos_rt_str_with_capacity",
            crate::c_abi::gos_rt_str_with_capacity as *const u8,
        ),
        (
            "gos_rt_str_byte_at",
            crate::c_abi::gos_rt_str_byte_at as *const u8,
        ),
        (
            "gos_rt_str_byte_len",
            crate::c_abi::gos_rt_str_byte_len as *const u8,
        ),
        (
            "gos_rt_str_char_at",
            crate::c_abi::gos_rt_str_char_at as *const u8,
        ),
        (
            "gos_rt_str_center",
            crate::c_abi::gos_rt_str_center as *const u8,
        ),
        (
            "gos_rt_str_compare",
            crate::c_abi::gos_rt_str_compare as *const u8,
        ),
        (
            "gos_rt_str_concat",
            crate::c_abi::gos_rt_str_concat as *const u8,
        ),
        (
            "gos_rt_str_concat_drop_a",
            crate::c_abi::gos_rt_str_concat_drop_a as *const u8,
        ),
        (
            "gos_rt_str_append_bytes",
            crate::c_abi::gos_rt_str_append_bytes as *const u8,
        ),
        (
            "gos_rt_str_append_i64",
            crate::c_abi::gos_rt_str_append_i64 as *const u8,
        ),
        (
            "gos_rt_str_append_f64",
            crate::c_abi::gos_rt_str_append_f64 as *const u8,
        ),
        (
            "gos_rt_str_contains",
            crate::c_abi::gos_rt_str_contains as *const u8,
        ),
        (
            "gos_rt_str_contains_any",
            crate::c_abi::gos_rt_str_contains_any as *const u8,
        ),
        (
            "gos_rt_str_contains_rune",
            crate::c_abi::gos_rt_str_contains_rune as *const u8,
        ),
        (
            "gos_rt_strconv_atoi",
            crate::c_abi::gos_rt_strconv_atoi as *const u8,
        ),
        (
            "gos_rt_strconv_format_bool",
            crate::c_abi::gos_rt_strconv_format_bool as *const u8,
        ),
        (
            "gos_rt_strconv_format_f64",
            crate::c_abi::gos_rt_strconv_format_f64 as *const u8,
        ),
        (
            "gos_rt_strconv_format_i64",
            crate::c_abi::gos_rt_strconv_format_i64 as *const u8,
        ),
        (
            "gos_rt_strconv_itoa",
            crate::c_abi::gos_rt_strconv_itoa as *const u8,
        ),
        (
            "gos_rt_strconv_parse_bool",
            crate::c_abi::gos_rt_strconv_parse_bool as *const u8,
        ),
        (
            "gos_rt_strconv_parse_f64",
            crate::c_abi::gos_rt_strconv_parse_f64 as *const u8,
        ),
        (
            "gos_rt_strconv_parse_f64_bytes",
            crate::c_abi::gos_rt_strconv_parse_f64_bytes as *const u8,
        ),
        (
            "gos_rt_strconv_parse_f64_range",
            crate::c_abi::gos_rt_strconv_parse_f64_range as *const u8,
        ),
        (
            "gos_rt_strconv_parse_i64",
            crate::c_abi::gos_rt_strconv_parse_i64 as *const u8,
        ),
        (
            "gos_rt_strconv_parse_i64_bytes",
            crate::c_abi::gos_rt_strconv_parse_i64_bytes as *const u8,
        ),
        (
            "gos_rt_strconv_parse_i64_radix",
            crate::c_abi::gos_rt_strconv_parse_i64_radix as *const u8,
        ),
        (
            "gos_rt_strconv_parse_i64_range",
            crate::c_abi::gos_rt_strconv_parse_i64_range as *const u8,
        ),
        (
            "gos_rt_strconv_parse_u64",
            crate::c_abi::gos_rt_strconv_parse_u64 as *const u8,
        ),
        (
            "gos_rt_strconv_format_i64_radix",
            crate::c_abi::gos_rt_strconv_format_i64_radix as *const u8,
        ),
        (
            "gos_rt_strconv_quote",
            crate::c_abi::gos_rt_strconv_quote as *const u8,
        ),
        (
            "gos_rt_strconv_unquote",
            crate::c_abi::gos_rt_strconv_unquote as *const u8,
        ),
        (
            "gos_rt_str_count",
            crate::c_abi::gos_rt_str_count as *const u8,
        ),
        (
            "gos_rt_str_chars",
            crate::c_abi::gos_rt_str_chars as *const u8,
        ),
        (
            "gos_rt_stream_flush",
            crate::c_abi::gos_rt_stream_flush as *const u8,
        ),
        (
            "gos_rt_stream_read_line",
            crate::c_abi::gos_rt_stream_read_line as *const u8,
        ),
        (
            "gos_rt_stream_next_line",
            crate::c_abi::gos_rt_stream_next_line as *const u8,
        ),
        (
            "gos_rt_stream_read_to_string",
            crate::c_abi::gos_rt_stream_read_to_string as *const u8,
        ),
        (
            "gos_rt_stream_write_byte",
            crate::c_abi::gos_rt_stream_write_byte as *const u8,
        ),
        (
            "gos_rt_stream_write_byte_array",
            crate::c_abi::gos_rt_stream_write_byte_array as *const u8,
        ),
        (
            "gos_rt_stream_write_str",
            crate::c_abi::gos_rt_stream_write_str as *const u8,
        ),
        (
            "gos_rt_str_ends_with",
            crate::c_abi::gos_rt_str_ends_with as *const u8,
        ),
        ("gos_rt_str_eq", crate::c_abi::gos_rt_str_eq as *const u8),
        (
            "gos_rt_str_equal_fold",
            crate::c_abi::gos_rt_str_equal_fold as *const u8,
        ),
        (
            "gos_rt_str_fields",
            crate::c_abi::gos_rt_str_fields as *const u8,
        ),
        (
            "gos_rt_str_find",
            crate::c_abi::gos_rt_str_find as *const u8,
        ),
        (
            "gos_rt_str_find_opt",
            crate::c_abi::gos_rt_str_find_opt as *const u8,
        ),
        (
            "gos_rt_str_first_codepoint",
            crate::c_abi::gos_rt_str_first_codepoint as *const u8,
        ),
        (
            "gos_rt_str_free",
            crate::c_abi::gos_rt_str_free as *const u8,
        ),
        (
            "gos_rt_str_free_typed",
            crate::c_abi::gos_rt_str_free_typed as *const u8,
        ),
        (
            "gos_rt_str_index_any",
            crate::c_abi::gos_rt_str_index_any as *const u8,
        ),
        (
            "gos_rt_str_index_rune",
            crate::c_abi::gos_rt_str_index_rune as *const u8,
        ),
        (
            "gos_rt_strings_join",
            crate::c_abi::gos_rt_strings_join as *const u8,
        ),
        (
            "gos_rt_str_is_empty",
            crate::c_abi::gos_rt_str_is_empty as *const u8,
        ),
        (
            "gos_rt_str_last_index_any",
            crate::c_abi::gos_rt_str_last_index_any as *const u8,
        ),
        ("gos_rt_str_len", crate::c_abi::gos_rt_str_len as *const u8),
        (
            "gos_rt_str_lines",
            crate::c_abi::gos_rt_str_lines as *const u8,
        ),
        (
            "gos_rt_str_lstrip_chars",
            crate::c_abi::gos_rt_str_lstrip_chars as *const u8,
        ),
        (
            "gos_rt_str_pad_left",
            crate::c_abi::gos_rt_str_pad_left as *const u8,
        ),
        ("gos_rt_fmt_pad", crate::c_abi::gos_rt_fmt_pad as *const u8),
        (
            "gos_rt_fmt_pad_i64",
            crate::c_abi::gos_rt_fmt_pad_i64 as *const u8,
        ),
        (
            "gos_rt_concat_pad_i64",
            crate::c_abi::gos_rt_concat_pad_i64 as *const u8,
        ),
        (
            "gos_rt_str_pad_right",
            crate::c_abi::gos_rt_str_pad_right as *const u8,
        ),
        (
            "gos_rt_str_push_utf8",
            crate::c_abi::string::gos_rt_str_push_utf8 as *const u8,
        ),
        (
            "gos_rt_str_push_byte",
            crate::c_abi::gos_rt_str_push_byte as *const u8,
        ),
        (
            "gos_rt_str_push_char",
            crate::c_abi::gos_rt_str_push_char as *const u8,
        ),
        (
            "gos_rt_str_repeat",
            crate::c_abi::gos_rt_str_repeat as *const u8,
        ),
        (
            "gos_rt_str_replace",
            crate::c_abi::gos_rt_str_replace as *const u8,
        ),
        (
            "gos_rt_str_replacen",
            crate::c_abi::gos_rt_str_replacen as *const u8,
        ),
        (
            "gos_rt_str_retain_typed",
            crate::c_abi::gos_rt_str_retain_typed as *const u8,
        ),
        (
            "gos_rt_str_rfind_opt",
            crate::c_abi::gos_rt_str_rfind_opt as *const u8,
        ),
        (
            "gos_rt_str_rsplit_once",
            crate::c_abi::gos_rt_str_rsplit_once as *const u8,
        ),
        (
            "gos_rt_str_rstrip_chars",
            crate::c_abi::gos_rt_str_rstrip_chars as *const u8,
        ),
        (
            "gos_rt_str_slice",
            crate::c_abi::gos_rt_str_slice as *const u8,
        ),
        (
            "gos_rt_str_truncate",
            crate::c_abi::gos_rt_str_truncate as *const u8,
        ),
        (
            "gos_rt_str_split",
            crate::c_abi::gos_rt_str_split as *const u8,
        ),
        (
            "gos_rt_str_splitn",
            crate::c_abi::gos_rt_str_splitn as *const u8,
        ),
        (
            "gos_rt_str_split_once",
            crate::c_abi::gos_rt_str_split_once as *const u8,
        ),
        (
            "gos_rt_str_split_whitespace",
            crate::c_abi::gos_rt_str_split_whitespace as *const u8,
        ),
        (
            "gos_rt_str_starts_with",
            crate::c_abi::gos_rt_str_starts_with as *const u8,
        ),
        (
            "gos_rt_str_strip_chars",
            crate::c_abi::gos_rt_str_strip_chars as *const u8,
        ),
        (
            "gos_rt_str_strip_prefix",
            crate::c_abi::gos_rt_str_strip_prefix as *const u8,
        ),
        (
            "gos_rt_str_strip_suffix",
            crate::c_abi::gos_rt_str_strip_suffix as *const u8,
        ),
        (
            "gos_rt_str_substring",
            crate::c_abi::gos_rt_str_substring as *const u8,
        ),
        (
            "gos_rt_string_from_utf8",
            crate::c_abi::gos_rt_string_from_utf8 as *const u8,
        ),
        (
            "gos_rt_str_to_bool_opt",
            crate::c_abi::gos_rt_str_to_bool_opt as *const u8,
        ),
        (
            "gos_rt_str_to_f64_opt",
            crate::c_abi::gos_rt_str_to_f64_opt as *const u8,
        ),
        (
            "gos_rt_str_to_i64_opt",
            crate::c_abi::gos_rt_str_to_i64_opt as *const u8,
        ),
        (
            "gos_rt_str_to_lower",
            crate::c_abi::gos_rt_str_to_lower as *const u8,
        ),
        (
            "gos_rt_str_to_title",
            crate::c_abi::gos_rt_str_to_title as *const u8,
        ),
        (
            "gos_rt_str_to_upper",
            crate::c_abi::gos_rt_str_to_upper as *const u8,
        ),
        (
            "gos_rt_str_trim",
            crate::c_abi::gos_rt_str_trim as *const u8,
        ),
        (
            "gos_rt_str_trim_end",
            crate::c_abi::gos_rt_str_trim_end as *const u8,
        ),
        (
            "gos_rt_str_trim_matches",
            crate::c_abi::gos_rt_str_trim_matches as *const u8,
        ),
        (
            "gos_rt_str_trim_start",
            crate::c_abi::gos_rt_str_trim_start as *const u8,
        ),
        (
            "gos_rt_str_zfill",
            crate::c_abi::gos_rt_str_zfill as *const u8,
        ),
        (
            "gos_rt_sync_i64_add",
            crate::c_abi::gos_rt_sync_i64_add as *const u8,
        ),
        (
            "gos_rt_sync_i64_drop",
            crate::c_abi::gos_rt_sync_i64_drop as *const u8,
        ),
        (
            "gos_rt_sync_i64_get",
            crate::c_abi::gos_rt_sync_i64_get as *const u8,
        ),
        (
            "gos_rt_sync_i64_len",
            crate::c_abi::gos_rt_sync_i64_len as *const u8,
        ),
        (
            "gos_rt_sync_i64_new",
            crate::c_abi::gos_rt_sync_i64_new as *const u8,
        ),
        (
            "gos_rt_sync_i64_push",
            crate::c_abi::gos_rt_sync_i64_push as *const u8,
        ),
        (
            "gos_rt_sync_i64_set",
            crate::c_abi::gos_rt_sync_i64_set as *const u8,
        ),
        (
            "gos_rt_sync_map_contains",
            crate::c_abi::gos_rt_sync_map_contains as *const u8,
        ),
        (
            "gos_rt_sync_map_delete",
            crate::c_abi::gos_rt_sync_map_delete as *const u8,
        ),
        (
            "gos_rt_sync_map_get",
            crate::c_abi::gos_rt_sync_map_get as *const u8,
        ),
        (
            "gos_rt_sync_map_keys",
            crate::c_abi::gos_rt_sync_map_keys as *const u8,
        ),
        (
            "gos_rt_sync_map_len",
            crate::c_abi::gos_rt_sync_map_len as *const u8,
        ),
        (
            "gos_rt_sync_map_new",
            crate::c_abi::gos_rt_sync_map_new as *const u8,
        ),
        (
            "gos_rt_sync_map_set",
            crate::c_abi::gos_rt_sync_map_set as *const u8,
        ),
        (
            "gos_rt_sync_u8_drop",
            crate::c_abi::gos_rt_sync_u8_drop as *const u8,
        ),
        (
            "gos_rt_sync_u8_get",
            crate::c_abi::gos_rt_sync_u8_get as *const u8,
        ),
        (
            "gos_rt_sync_u8_len",
            crate::c_abi::gos_rt_sync_u8_len as *const u8,
        ),
        (
            "gos_rt_sync_u8_new",
            crate::c_abi::gos_rt_sync_u8_new as *const u8,
        ),
        (
            "gos_rt_sync_u8_push",
            crate::c_abi::gos_rt_sync_u8_push as *const u8,
        ),
        (
            "gos_rt_sync_u8_set",
            crate::c_abi::gos_rt_sync_u8_set as *const u8,
        ),
        (
            "gos_rt_tar_read_raw",
            crate::c_abi::gos_rt_tar_read_raw as *const u8,
        ),
        (
            "gos_rt_tar_write",
            crate::c_abi::gos_rt_tar_write as *const u8,
        ),
        (
            "gos_rt_tcp_listener_accept",
            crate::c_abi::gos_rt_tcp_listener_accept as *const u8,
        ),
        (
            "gos_rt_tcp_listener_bind",
            crate::c_abi::gos_rt_tcp_listener_bind as *const u8,
        ),
        (
            "gos_rt_unix_listener_bind",
            crate::c_abi::gos_rt_unix_listener_bind as *const u8,
        ),
        (
            "gos_rt_unix_listener_accept",
            crate::c_abi::gos_rt_unix_listener_accept as *const u8,
        ),
        (
            "gos_rt_unix_listener_close",
            crate::c_abi::gos_rt_unix_listener_close as *const u8,
        ),
        (
            "gos_rt_unix_stream_connect",
            crate::c_abi::gos_rt_unix_stream_connect as *const u8,
        ),
        (
            "gos_rt_unix_stream_read",
            crate::c_abi::gos_rt_unix_stream_read as *const u8,
        ),
        (
            "gos_rt_unix_stream_read_to_string",
            crate::c_abi::gos_rt_unix_stream_read_to_string as *const u8,
        ),
        (
            "gos_rt_unix_stream_write",
            crate::c_abi::gos_rt_unix_stream_write as *const u8,
        ),
        (
            "gos_rt_unix_stream_close",
            crate::c_abi::gos_rt_unix_stream_close as *const u8,
        ),
        (
            "gos_rt_tcp_listener_close",
            crate::c_abi::gos_rt_tcp_listener_close as *const u8,
        ),
        (
            "gos_rt_tcp_listener_local_addr",
            crate::c_abi::gos_rt_tcp_listener_local_addr as *const u8,
        ),
        (
            "gos_rt_tcp_tls_peer_cert",
            crate::c_abi::gos_rt_tcp_tls_peer_cert as *const u8,
        ),
        (
            "gos_rt_tcp_start_tls",
            crate::c_abi::gos_rt_tcp_start_tls as *const u8,
        ),
        (
            "gos_rt_tcp_start_tls_ca",
            crate::c_abi::gos_rt_tcp_start_tls_ca as *const u8,
        ),
        (
            "gos_rt_tcp_start_tls_insecure",
            crate::c_abi::gos_rt_tcp_start_tls_insecure as *const u8,
        ),
        (
            "gos_rt_tcp_stream_close",
            crate::c_abi::gos_rt_tcp_stream_close as *const u8,
        ),
        (
            "gos_rt_tcp_stream_clear_read_timeout",
            crate::c_abi::gos_rt_tcp_stream_clear_read_timeout as *const u8,
        ),
        (
            "gos_rt_tcp_stream_clear_write_timeout",
            crate::c_abi::gos_rt_tcp_stream_clear_write_timeout as *const u8,
        ),
        (
            "gos_rt_tcp_stream_connect",
            crate::c_abi::gos_rt_tcp_stream_connect as *const u8,
        ),
        (
            "gos_rt_tcp_stream_read",
            crate::c_abi::gos_rt_tcp_stream_read as *const u8,
        ),
        (
            "gos_rt_tcp_stream_read_to_string",
            crate::c_abi::gos_rt_tcp_stream_read_to_string as *const u8,
        ),
        (
            "gos_rt_tcp_stream_set_read_timeout_ms",
            crate::c_abi::gos_rt_tcp_stream_set_read_timeout_ms as *const u8,
        ),
        (
            "gos_rt_tcp_stream_set_write_timeout_ms",
            crate::c_abi::gos_rt_tcp_stream_set_write_timeout_ms as *const u8,
        ),
        (
            "gos_rt_tcp_stream_set_nodelay",
            crate::c_abi::gos_rt_tcp_stream_set_nodelay as *const u8,
        ),
        (
            "gos_rt_tcp_stream_read_into",
            crate::c_abi::gos_rt_tcp_stream_read_into as *const u8,
        ),
        (
            "gos_rt_tcp_stream_write",
            crate::c_abi::gos_rt_tcp_stream_write as *const u8,
        ),
        (
            "gos_rt_testing_check",
            crate::c_abi::gos_rt_testing_check as *const u8,
        ),
        (
            "gos_rt_testing_check_eq_i64",
            crate::c_abi::gos_rt_testing_check_eq_i64 as *const u8,
        ),
        (
            "gos_rt_testing_wait_for_scheduler_idle",
            crate::c_abi::gos_rt_testing_wait_for_scheduler_idle as *const u8,
        ),
        (
            "gos_rt_httptest_server",
            crate::c_abi::gos_rt_httptest_server as *const u8,
        ),
        (
            "gos_rt_image_new",
            crate::c_abi::gos_rt_image_new as *const u8,
        ),
        (
            "gos_rt_image_filled",
            crate::c_abi::gos_rt_image_filled as *const u8,
        ),
        (
            "gos_rt_image_decode_base64",
            crate::c_abi::gos_rt_image_decode_base64 as *const u8,
        ),
        (
            "gos_rt_image_width",
            crate::c_abi::gos_rt_image_width as *const u8,
        ),
        (
            "gos_rt_image_height",
            crate::c_abi::gos_rt_image_height as *const u8,
        ),
        (
            "gos_rt_image_pixel",
            crate::c_abi::gos_rt_image_pixel as *const u8,
        ),
        (
            "gos_rt_image_set_pixel",
            crate::c_abi::gos_rt_image_set_pixel as *const u8,
        ),
        (
            "gos_rt_image_encode_png_base64",
            crate::c_abi::gos_rt_image_encode_png_base64 as *const u8,
        ),
        (
            "gos_rt_image_encode_jpeg_base64",
            crate::c_abi::gos_rt_image_encode_jpeg_base64 as *const u8,
        ),
        (
            "gos_rt_thread_num_cpus",
            crate::c_abi::gos_rt_thread_num_cpus as *const u8,
        ),
        (
            "gos_rt_time_format_rfc3339",
            crate::c_abi::gos_rt_time_format_rfc3339 as *const u8,
        ),
        (
            "gos_rt_time_add_date_raw",
            crate::c_abi::gos_rt_time_add_date_raw as *const u8,
        ),
        (
            "gos_rt_time_civil_raw",
            crate::c_abi::gos_rt_time_civil_raw as *const u8,
        ),
        (
            "gos_rt_time_fixed_location_raw",
            crate::c_abi::gos_rt_time_fixed_location_raw as *const u8,
        ),
        (
            "gos_rt_time_format_in_raw",
            crate::c_abi::gos_rt_time_format_in_raw as *const u8,
        ),
        (
            "gos_rt_time_location_raw",
            crate::c_abi::gos_rt_time_location_raw as *const u8,
        ),
        (
            "gos_rt_time_now",
            crate::c_abi::gos_rt_time_now as *const u8,
        ),
        (
            "gos_rt_time_now_ms",
            crate::c_abi::gos_rt_time_now_ms as *const u8,
        ),
        (
            "gos_rt_time_now_nanos",
            crate::c_abi::gos_rt_time_now_nanos as *const u8,
        ),
        (
            "gos_rt_time_parse_rfc3339",
            crate::c_abi::gos_rt_time_parse_rfc3339 as *const u8,
        ),
        (
            "gos_rt_time_resolve_raw",
            crate::c_abi::gos_rt_time_resolve_raw as *const u8,
        ),
        (
            "gos_rt_time_since_ms",
            crate::c_abi::gos_rt_time_since_ms as *const u8,
        ),
        (
            "gos_rt_toml_from_json",
            crate::c_abi::gos_rt_toml_from_json as *const u8,
        ),
        (
            "gos_rt_toml_is_valid",
            crate::c_abi::gos_rt_toml_is_valid as *const u8,
        ),
        (
            "gos_rt_toml_pretty",
            crate::c_abi::gos_rt_toml_pretty as *const u8,
        ),
        (
            "gos_rt_toml_to_json",
            crate::c_abi::gos_rt_toml_to_json as *const u8,
        ),
        (
            "gos_rt_trace_ended_to_otlp_json",
            crate::c_abi::gos_rt_trace_ended_to_otlp_json as *const u8,
        ),
        (
            "gos_rt_trace_span_end",
            crate::c_abi::gos_rt_trace_span_end as *const u8,
        ),
        (
            "gos_rt_trace_span_set_attribute",
            crate::c_abi::gos_rt_trace_span_set_attribute as *const u8,
        ),
        (
            "gos_rt_trace_span_set_status",
            crate::c_abi::gos_rt_trace_span_set_status as *const u8,
        ),
        (
            "gos_rt_trace_tracer_new",
            crate::c_abi::gos_rt_trace_tracer_new as *const u8,
        ),
        (
            "gos_rt_trace_tracer_start_span",
            crate::c_abi::gos_rt_trace_tracer_start_span as *const u8,
        ),
        (
            "gos_rt_tuple_format",
            crate::c_abi::gos_rt_tuple_format as *const u8,
        ),
        (
            "gos_rt_tuple_format_desc",
            crate::c_abi::gos_rt_tuple_format_desc as *const u8,
        ),
        (
            "gos_rt_lazy_iter_str_bytes",
            crate::c_abi::gos_rt_lazy_iter_str_bytes as *const u8,
        ),
        (
            "gos_rt_lazy_iter_str_chars",
            crate::c_abi::gos_rt_lazy_iter_str_chars as *const u8,
        ),
        (
            "gos_rt_map_format_desc",
            crate::c_abi::gos_rt_map_format_desc as *const u8,
        ),
        (
            "gos_rt_vec_format_desc",
            crate::c_abi::gos_rt_vec_format_desc as *const u8,
        ),
        (
            "gos_rt_vec_format_map",
            crate::c_abi::gos_rt_vec_format_map as *const u8,
        ),
        (
            "gos_rt_vec_format_tuple",
            crate::c_abi::gos_rt_vec_format_tuple as *const u8,
        ),
        (
            "gos_rt_vec_format_u64",
            crate::c_abi::btmap::gos_rt_vec_format_u64 as *const u8,
        ),
        (
            "gos_rt_tuple_cmp",
            crate::c_abi::gos_rt_tuple_cmp as *const u8,
        ),
        ("gos_rt_vec_eq", crate::c_abi::gos_rt_vec_eq as *const u8),
        (
            "gos_rt_enum_struct_eq",
            crate::c_abi::gos_rt_enum_struct_eq as *const u8,
        ),
        (
            "gos_rt_u64_to_str",
            crate::c_abi::gos_rt_u64_to_str as *const u8,
        ),
        (
            "gos_rt_udp_bind",
            crate::c_abi::gos_rt_udp_bind as *const u8,
        ),
        (
            "gos_rt_udp_close",
            crate::c_abi::gos_rt_udp_close as *const u8,
        ),
        (
            "gos_rt_udp_local_addr",
            crate::c_abi::gos_rt_udp_local_addr as *const u8,
        ),
        (
            "gos_rt_udp_recv_from",
            crate::c_abi::gos_rt_udp_recv_from as *const u8,
        ),
        (
            "gos_rt_udp_send_to",
            crate::c_abi::gos_rt_udp_send_to as *const u8,
        ),
        (
            "gos_rt_unicode_combining_class",
            crate::c_abi::gos_rt_unicode_combining_class as *const u8,
        ),
        (
            "gos_rt_unicode_fold_case",
            crate::c_abi::gos_rt_unicode_fold_case as *const u8,
        ),
        (
            "gos_rt_unicode_grapheme_count",
            crate::c_abi::gos_rt_unicode_grapheme_count as *const u8,
        ),
        (
            "gos_rt_unicode_graphemes",
            crate::c_abi::gos_rt_unicode_graphemes as *const u8,
        ),
        (
            "gos_rt_unicode_is_assigned",
            crate::c_abi::gos_rt_unicode_is_assigned as *const u8,
        ),
        (
            "gos_rt_unicode_is_control",
            crate::c_abi::gos_rt_unicode_is_control as *const u8,
        ),
        (
            "gos_rt_unicode_is_digit",
            crate::c_abi::gos_rt_unicode_is_digit as *const u8,
        ),
        (
            "gos_rt_unicode_is_graphic",
            crate::c_abi::gos_rt_unicode_is_graphic as *const u8,
        ),
        (
            "gos_rt_unicode_is_letter",
            crate::c_abi::gos_rt_unicode_is_letter as *const u8,
        ),
        (
            "gos_rt_unicode_is_lower",
            crate::c_abi::gos_rt_unicode_is_lower as *const u8,
        ),
        (
            "gos_rt_unicode_is_mark",
            crate::c_abi::gos_rt_unicode_is_mark as *const u8,
        ),
        (
            "gos_rt_unicode_is_nfc",
            crate::c_abi::gos_rt_unicode_is_nfc as *const u8,
        ),
        (
            "gos_rt_unicode_is_nfd",
            crate::c_abi::gos_rt_unicode_is_nfd as *const u8,
        ),
        (
            "gos_rt_unicode_is_nfkc",
            crate::c_abi::gos_rt_unicode_is_nfkc as *const u8,
        ),
        (
            "gos_rt_unicode_is_nfkd",
            crate::c_abi::gos_rt_unicode_is_nfkd as *const u8,
        ),
        (
            "gos_rt_unicode_is_number",
            crate::c_abi::gos_rt_unicode_is_number as *const u8,
        ),
        (
            "gos_rt_unicode_is_print",
            crate::c_abi::gos_rt_unicode_is_print as *const u8,
        ),
        (
            "gos_rt_unicode_is_punct",
            crate::c_abi::gos_rt_unicode_is_punct as *const u8,
        ),
        (
            "gos_rt_unicode_is_space",
            crate::c_abi::gos_rt_unicode_is_space as *const u8,
        ),
        (
            "gos_rt_unicode_is_symbol",
            crate::c_abi::gos_rt_unicode_is_symbol as *const u8,
        ),
        (
            "gos_rt_unicode_is_title",
            crate::c_abi::gos_rt_unicode_is_title as *const u8,
        ),
        (
            "gos_rt_unicode_is_upper",
            crate::c_abi::gos_rt_unicode_is_upper as *const u8,
        ),
        (
            "gos_rt_unicode_nfc",
            crate::c_abi::gos_rt_unicode_nfc as *const u8,
        ),
        (
            "gos_rt_unicode_nfd",
            crate::c_abi::gos_rt_unicode_nfd as *const u8,
        ),
        (
            "gos_rt_unicode_nfkc",
            crate::c_abi::gos_rt_unicode_nfkc as *const u8,
        ),
        (
            "gos_rt_unicode_nfkd",
            crate::c_abi::gos_rt_unicode_nfkd as *const u8,
        ),
        (
            "gos_rt_unicode_sentence_count",
            crate::c_abi::gos_rt_unicode_sentence_count as *const u8,
        ),
        (
            "gos_rt_unicode_sentences",
            crate::c_abi::gos_rt_unicode_sentences as *const u8,
        ),
        (
            "gos_rt_unicode_simple_fold",
            crate::c_abi::gos_rt_unicode_simple_fold as *const u8,
        ),
        (
            "gos_rt_unicode_to_lower",
            crate::c_abi::gos_rt_unicode_to_lower as *const u8,
        ),
        (
            "gos_rt_unicode_to_lower_str",
            crate::c_abi::gos_rt_unicode_to_lower_str as *const u8,
        ),
        (
            "gos_rt_unicode_to_title",
            crate::c_abi::gos_rt_unicode_to_title as *const u8,
        ),
        (
            "gos_rt_unicode_to_upper",
            crate::c_abi::gos_rt_unicode_to_upper as *const u8,
        ),
        (
            "gos_rt_unicode_to_upper_str",
            crate::c_abi::gos_rt_unicode_to_upper_str as *const u8,
        ),
        (
            "gos_rt_unicode_word_bounds",
            crate::c_abi::gos_rt_unicode_word_bounds as *const u8,
        ),
        (
            "gos_rt_unicode_word_count",
            crate::c_abi::gos_rt_unicode_word_count as *const u8,
        ),
        (
            "gos_rt_unicode_words",
            crate::c_abi::gos_rt_unicode_words as *const u8,
        ),
        (
            "gos_rt_url_path_escape",
            crate::c_abi::gos_rt_url_path_escape as *const u8,
        ),
        (
            "gos_rt_url_path_unescape",
            crate::c_abi::gos_rt_url_path_unescape as *const u8,
        ),
        (
            "gos_rt_url_query_escape",
            crate::c_abi::gos_rt_url_query_escape as *const u8,
        ),
        (
            "gos_rt_url_query_unescape",
            crate::c_abi::gos_rt_url_query_unescape as *const u8,
        ),
        (
            "gos_rt_utf16_decode_surrogate_pair",
            crate::c_abi::gos_rt_utf16_decode_surrogate_pair as *const u8,
        ),
        (
            "gos_rt_utf16_decode_to_string",
            crate::c_abi::gos_rt_utf16_decode_to_string as *const u8,
        ),
        (
            "gos_rt_utf16_encode_string",
            crate::c_abi::gos_rt_utf16_encode_string as *const u8,
        ),
        (
            "gos_rt_utf16_is_surrogate",
            crate::c_abi::gos_rt_utf16_is_surrogate as *const u8,
        ),
        (
            "gos_rt_utf16_rune_len",
            crate::c_abi::gos_rt_utf16_rune_len as *const u8,
        ),
        (
            "gos_rt_utf8_append_rune",
            crate::c_abi::gos_rt_utf8_append_rune as *const u8,
        ),
        (
            "gos_rt_utf8_count_runes",
            crate::c_abi::gos_rt_utf8_count_runes as *const u8,
        ),
        (
            "gos_rt_utf8_decode_last_rune",
            crate::c_abi::gos_rt_utf8_decode_last_rune as *const u8,
        ),
        (
            "gos_rt_utf8_decode_last_rune_in_string",
            crate::c_abi::gos_rt_utf8_decode_last_rune_in_string as *const u8,
        ),
        (
            "gos_rt_utf8_decode_rune",
            crate::c_abi::gos_rt_utf8_decode_rune as *const u8,
        ),
        (
            "gos_rt_utf8_decode_rune_in_string",
            crate::c_abi::gos_rt_utf8_decode_rune_in_string as *const u8,
        ),
        (
            "gos_rt_utf8_full_rune_in_string",
            crate::c_abi::gos_rt_utf8_full_rune_in_string as *const u8,
        ),
        (
            "gos_rt_utf8_full_rune",
            crate::c_abi::gos_rt_utf8_full_rune as *const u8,
        ),
        (
            "gos_rt_utf8_is_valid",
            crate::c_abi::gos_rt_utf8_is_valid as *const u8,
        ),
        (
            "gos_rt_utf8_rune_count_bytes",
            crate::c_abi::gos_rt_utf8_rune_count_bytes as *const u8,
        ),
        (
            "gos_rt_utf8_rune_count_in_string",
            crate::c_abi::gos_rt_utf8_rune_count_in_string as *const u8,
        ),
        (
            "gos_rt_utf8_rune_len",
            crate::c_abi::gos_rt_utf8_rune_len as *const u8,
        ),
        (
            "gos_rt_utf8_rune_start",
            crate::c_abi::gos_rt_utf8_rune_start as *const u8,
        ),
        (
            "gos_rt_utf8_valid_rune",
            crate::c_abi::gos_rt_utf8_valid_rune as *const u8,
        ),
        (
            "gos_rt_utf8_valid_string",
            crate::c_abi::gos_rt_utf8_valid_string as *const u8,
        ),
        (
            "gos_rt_uuid_is_valid",
            crate::c_abi::gos_rt_uuid_is_valid as *const u8,
        ),
        (
            "gos_rt_uuid_normalize",
            crate::c_abi::gos_rt_uuid_normalize as *const u8,
        ),
        (
            "gos_rt_uuid_simple",
            crate::c_abi::gos_rt_uuid_simple as *const u8,
        ),
        ("gos_rt_uuid_v4", crate::c_abi::gos_rt_uuid_v4 as *const u8),
        ("gos_rt_uuid_v7", crate::c_abi::gos_rt_uuid_v7 as *const u8),
        (
            "gos_rt_validate_errors_add",
            crate::c_abi::gos_rt_validate_errors_add as *const u8,
        ),
        (
            "gos_rt_validate_errors_collect",
            crate::c_abi::gos_rt_validate_errors_collect as *const u8,
        ),
        (
            "gos_rt_validate_errors_count",
            crate::c_abi::gos_rt_validate_errors_count as *const u8,
        ),
        (
            "gos_rt_validate_errors_get",
            crate::c_abi::gos_rt_validate_errors_get as *const u8,
        ),
        (
            "gos_rt_validate_errors_is_empty",
            crate::c_abi::gos_rt_validate_errors_is_empty as *const u8,
        ),
        (
            "gos_rt_validate_errors_len",
            crate::c_abi::gos_rt_validate_errors_len as *const u8,
        ),
        (
            "gos_rt_validate_errors_new",
            crate::c_abi::gos_rt_validate_errors_new as *const u8,
        ),
        (
            "gos_rt_vec_clone",
            crate::c_abi::gos_rt_vec_clone as *const u8,
        ),
        (
            "gos_rt_vec_contains_i64",
            crate::c_abi::gos_rt_vec_contains_i64 as *const u8,
        ),
        (
            "gos_rt_vec_contains_str",
            crate::c_abi::gos_rt_vec_contains_str as *const u8,
        ),
        (
            "gos_rt_vec_count_of_i64",
            crate::c_abi::gos_rt_vec_count_of_i64 as *const u8,
        ),
        (
            "gos_rt_vec_count_of_str",
            crate::c_abi::gos_rt_vec_count_of_str as *const u8,
        ),
        (
            "gos_rt_vec_first",
            crate::c_abi::gos_rt_vec_first as *const u8,
        ),
        (
            "gos_rt_vec_format_adt",
            crate::c_abi::gos_rt_vec_format_adt as *const u8,
        ),
        (
            "gos_rt_vec_format_bool",
            crate::c_abi::gos_rt_vec_format_bool as *const u8,
        ),
        (
            "gos_rt_vec_format_char",
            crate::c_abi::gos_rt_vec_format_char as *const u8,
        ),
        (
            "gos_rt_vec_format_f64",
            crate::c_abi::gos_rt_vec_format_f64 as *const u8,
        ),
        (
            "gos_rt_vec_format_i64",
            crate::c_abi::gos_rt_vec_format_i64 as *const u8,
        ),
        (
            "gos_rt_vec_format_string",
            crate::c_abi::gos_rt_vec_format_string as *const u8,
        ),
        (
            "gos_rt_vec_format_vec_f64",
            crate::c_abi::gos_rt_vec_format_vec_f64 as *const u8,
        ),
        (
            "gos_rt_vec_format_vec_i64",
            crate::c_abi::gos_rt_vec_format_vec_i64 as *const u8,
        ),
        (
            "gos_rt_vec_format_vec_string",
            crate::c_abi::gos_rt_vec_format_vec_string as *const u8,
        ),
        (
            "gos_rt_vec_free",
            crate::c_abi::gos_rt_vec_free as *const u8,
        ),
        (
            "gos_rt_vec_retain",
            crate::c_abi::gos_rt_vec_retain as *const u8,
        ),
        (
            "gos_rt_vec_mark_shared",
            crate::c_abi::gos_rt_vec_mark_shared as *const u8,
        ),
        (
            "gos_rt_vec_from_arr",
            crate::c_abi::gos_rt_vec_from_arr as *const u8,
        ),
        (
            "gos_rt_vec_from_packed_arr",
            crate::c_abi::gos_rt_vec_from_packed_arr as *const u8,
        ),
        (
            "gos_rt_vec_get_i128",
            crate::c_abi::gos_rt_vec_get_i128 as *const u8,
        ),
        (
            "gos_rt_vec_set_i128",
            crate::c_abi::gos_rt_vec_set_i128 as *const u8,
        ),
        (
            "gos_rt_vec_get_opt",
            crate::c_abi::gos_rt_vec_get_opt as *const u8,
        ),
        (
            "gos_rt_vec_get_i64",
            crate::c_abi::gos_rt_vec_get_i64 as *const u8,
        ),
        (
            "gos_rt_vec_get_i64_unchecked",
            crate::c_abi::gos_rt_vec_get_i64_unchecked as *const u8,
        ),
        (
            "gos_rt_vec_get_ptr",
            crate::c_abi::gos_rt_vec_get_ptr as *const u8,
        ),
        (
            "gos_rt_vec_index_of_i64",
            crate::c_abi::gos_rt_vec_index_of_i64 as *const u8,
        ),
        (
            "gos_rt_vec_index_of_str",
            crate::c_abi::gos_rt_vec_index_of_str as *const u8,
        ),
        (
            "gos_rt_vec_insert_at",
            crate::c_abi::gos_rt_vec_insert_at as *const u8,
        ),
        (
            "gos_rt_vec_assign",
            crate::c_abi::gos_rt_vec_assign as *const u8,
        ),
        (
            "gos_rt_vec_clear",
            crate::c_abi::gos_rt_vec_clear as *const u8,
        ),
        (
            "gos_rt_vec_extend",
            crate::c_abi::gos_rt_vec_extend as *const u8,
        ),
        (
            "gos_rt_vec_insert_safe",
            crate::c_abi::gos_rt_vec_insert_safe as *const u8,
        ),
        (
            "gos_rt_vec_insert_slots_safe",
            crate::c_abi::gos_rt_vec_insert_slots_safe as *const u8,
        ),
        (
            "gos_rt_vec_join_bool",
            crate::c_abi::gos_rt_vec_join_bool as *const u8,
        ),
        (
            "gos_rt_vec_join_char",
            crate::c_abi::gos_rt_vec_join_char as *const u8,
        ),
        (
            "gos_rt_vec_join_f64",
            crate::c_abi::gos_rt_vec_join_f64 as *const u8,
        ),
        (
            "gos_rt_vec_join_i64",
            crate::c_abi::gos_rt_vec_join_i64 as *const u8,
        ),
        (
            "gos_rt_vec_last",
            crate::c_abi::gos_rt_vec_last as *const u8,
        ),
        (
            "gos_rt_vec_capacity",
            crate::c_abi::gos_rt_vec_capacity as *const u8,
        ),
        ("gos_rt_vec_len", crate::c_abi::gos_rt_vec_len as *const u8),
        ("gos_rt_vec_new", crate::c_abi::gos_rt_vec_new as *const u8),
        (
            "gos_rt_vec_new_typed",
            crate::c_abi::gos_rt_vec_new_typed as *const u8,
        ),
        ("gos_rt_vec_pop", crate::c_abi::gos_rt_vec_pop as *const u8),
        (
            "gos_rt_vec_pop_into",
            crate::c_abi::gos_rt_vec_pop_into as *const u8,
        ),
        (
            "gos_rt_vec_pop_opt",
            crate::c_abi::gos_rt_vec_pop_opt as *const u8,
        ),
        (
            "gos_rt_vec_binary_search_f64",
            crate::c_abi::gos_rt_vec_binary_search_f64 as *const u8,
        ),
        (
            "gos_rt_vec_binary_search_i64",
            crate::c_abi::gos_rt_vec_binary_search_i64 as *const u8,
        ),
        (
            "gos_rt_vec_binary_search_str",
            crate::c_abi::gos_rt_vec_binary_search_str as *const u8,
        ),
        (
            "gos_rt_vec_copy_from_slice",
            crate::c_abi::gos_rt_vec_copy_from_slice as *const u8,
        ),
        (
            "gos_rt_vec_copy_within",
            crate::c_abi::gos_rt_vec_copy_within as *const u8,
        ),
        (
            "gos_rt_vec_push",
            crate::c_abi::gos_rt_vec_push as *const u8,
        ),
        (
            "gos_rt_vec_push_i128",
            crate::c_abi::gos_rt_vec_push_i128 as *const u8,
        ),
        (
            "gos_rt_vec_push_i64",
            crate::c_abi::gos_rt_vec_push_i64 as *const u8,
        ),
        (
            "gos_rt_vec_repeat_primitive",
            crate::c_abi::gos_rt_vec_repeat_primitive as *const u8,
        ),
        (
            "gos_rt_vec_reserve_at_least",
            crate::c_abi::gos_rt_vec_reserve_at_least as *const u8,
        ),
        (
            "gos_rt_vec_reserve_exact",
            crate::c_abi::gos_rt_vec_reserve_exact as *const u8,
        ),
        (
            "gos_rt_vec_remove_at",
            crate::c_abi::gos_rt_vec_remove_at as *const u8,
        ),
        (
            "gos_rt_vec_remove_safe",
            crate::c_abi::gos_rt_vec_remove_safe as *const u8,
        ),
        (
            "gos_rt_vec_truncate",
            crate::c_abi::gos_rt_vec_truncate as *const u8,
        ),
        (
            "gos_rt_vec_reverse",
            crate::c_abi::gos_rt_vec_reverse as *const u8,
        ),
        (
            "gos_rt_set_from_vec_i64",
            crate::c_abi::gos_rt_set_from_vec_i64 as *const u8,
        ),
        (
            "gos_rt_set_from_vec_str",
            crate::c_abi::gos_rt_set_from_vec_str as *const u8,
        ),
        (
            "gos_rt_btree_set_from_vec_i64",
            crate::c_abi::gos_rt_btree_set_from_vec_i64 as *const u8,
        ),
        (
            "gos_rt_btree_set_from_vec_str",
            crate::c_abi::gos_rt_btree_set_from_vec_str as *const u8,
        ),
        (
            "gos_rt_vec_reversed",
            crate::c_abi::gos_rt_vec_reversed as *const u8,
        ),
        (
            "gos_rt_vec_set_slots",
            crate::c_abi::gos_rt_vec_set_slots as *const u8,
        ),
        (
            "gos_rt_vec_set_i64",
            crate::c_abi::gos_rt_vec_set_i64 as *const u8,
        ),
        (
            "gos_rt_vec_set_i64_unchecked",
            crate::c_abi::gos_rt_vec_set_i64_unchecked as *const u8,
        ),
        (
            "gos_rt_vec_swap_i64",
            crate::c_abi::gos_rt_vec_swap_i64 as *const u8,
        ),
        (
            "gos_rt_vec_swap_safe",
            crate::c_abi::gos_rt_vec_swap_safe as *const u8,
        ),
        (
            "gos_rt_vec_slice",
            crate::c_abi::gos_rt_vec_slice as *const u8,
        ),
        (
            "gos_rt_vec_slice_result",
            crate::c_abi::gos_rt_vec_slice_result as *const u8,
        ),
        (
            "gos_rt_vec_sort_by_aggr",
            crate::c_abi::gos_rt_vec_sort_by_aggr as *const u8,
        ),
        (
            "gos_rt_vec_sort_by_f64",
            crate::c_abi::gos_rt_vec_sort_by_f64 as *const u8,
        ),
        (
            "gos_rt_vec_sort_by_i64",
            crate::c_abi::gos_rt_vec_sort_by_i64 as *const u8,
        ),
        (
            "gos_rt_vec_sort_i64",
            crate::c_abi::gos_rt_vec_sort_i64 as *const u8,
        ),
        (
            "gos_rt_vec_sort_str",
            crate::c_abi::gos_rt_vec_sort_str as *const u8,
        ),
        (
            "gos_rt_vec_sort_tuple",
            crate::c_abi::gos_rt_vec_sort_tuple as *const u8,
        ),
        (
            "gos_rt_vec_step_by",
            crate::c_abi::gos_rt_vec_step_by as *const u8,
        ),
        (
            "gos_rt_vec_skip",
            crate::c_abi::gos_rt_vec_skip as *const u8,
        ),
        (
            "gos_rt_vec_take",
            crate::c_abi::gos_rt_vec_take as *const u8,
        ),
        (
            "gos_rt_vec_with_capacity",
            crate::c_abi::gos_rt_vec_with_capacity as *const u8,
        ),
        (
            "gos_rt_vec_with_capacity_typed",
            crate::c_abi::gos_rt_vec_with_capacity_typed as *const u8,
        ),
        ("gos_rt_wg_add", crate::c_abi::gos_rt_wg_add as *const u8),
        ("gos_rt_wg_done", crate::c_abi::gos_rt_wg_done as *const u8),
        (
            "gos_rt_wg_error",
            crate::c_abi::gos_rt_wg_error as *const u8,
        ),
        (
            "gos_rt_wg_error_clear",
            crate::c_abi::gos_rt_wg_error_clear as *const u8,
        ),
        ("gos_rt_wg_new", crate::c_abi::gos_rt_wg_new as *const u8),
        ("gos_rt_wg_wait", crate::c_abi::gos_rt_wg_wait as *const u8),
        (
            "gos_rt_wg_wait_ctx",
            crate::c_abi::gos_rt_wg_wait_ctx as *const u8,
        ),
        (
            "gos_rt_chan_recv_ctx",
            crate::c_abi::gos_rt_chan_recv_ctx as *const u8,
        ),
        (
            "gos_rt_ws_accept",
            crate::c_abi::gos_rt_ws_accept as *const u8,
        ),
        (
            "gos_rt_ws_accept_key",
            crate::c_abi::gos_rt_ws_accept_key as *const u8,
        ),
        (
            "gos_rt_ws_close",
            crate::c_abi::gos_rt_ws_close as *const u8,
        ),
        (
            "gos_rt_ws_frame_text",
            crate::c_abi::gos_rt_ws_frame_text as *const u8,
        ),
        (
            "gos_rt_ws_is_upgrade",
            crate::c_abi::gos_rt_ws_is_upgrade as *const u8,
        ),
        ("gos_rt_ws_recv", crate::c_abi::gos_rt_ws_recv as *const u8),
        (
            "gos_rt_ws_send_binary",
            crate::c_abi::gos_rt_ws_send_binary as *const u8,
        ),
        (
            "gos_rt_ws_send_text",
            crate::c_abi::gos_rt_ws_send_text as *const u8,
        ),
        (
            "gos_rt_ws_serve",
            crate::c_abi::gos_rt_ws_serve as *const u8,
        ),
        (
            "gos_rt_ws_serve_connect",
            crate::c_abi::gos_rt_ws_serve_connect as *const u8,
        ),
        (
            "gos_rt_x509_parse_pem_raw",
            crate::c_abi::gos_rt_x509_parse_pem_raw as *const u8,
        ),
        (
            "gos_rt_x509_verify_server_certificate_with_crls",
            crate::c_abi::gos_rt_x509_verify_server_certificate_with_crls as *const u8,
        ),
        (
            "gos_rt_xml_encode",
            crate::c_abi::gos_rt_xml_encode as *const u8,
        ),
        (
            "gos_rt_xml_parse",
            crate::c_abi::gos_rt_xml_parse as *const u8,
        ),
        (
            "gos_rt_yaml_encode",
            crate::c_abi::gos_rt_yaml_encode as *const u8,
        ),
        (
            "gos_rt_yaml_from_json",
            crate::c_abi::gos_rt_yaml_from_json as *const u8,
        ),
        (
            "gos_rt_yaml_is_valid",
            crate::c_abi::gos_rt_yaml_is_valid as *const u8,
        ),
        (
            "gos_rt_yaml_parse",
            crate::c_abi::gos_rt_yaml_parse as *const u8,
        ),
        (
            "gos_rt_yaml_parse_all",
            crate::c_abi::gos_rt_yaml_parse_all as *const u8,
        ),
        (
            "gos_rt_yaml_to_json",
            crate::c_abi::gos_rt_yaml_to_json as *const u8,
        ),
        (
            "gos_rt_zip_read_raw",
            crate::c_abi::gos_rt_zip_read_raw as *const u8,
        ),
        (
            "gos_rt_zip_write",
            crate::c_abi::gos_rt_zip_write as *const u8,
        ),
        (
            "gos_rt_iter_count_by_f64",
            crate::c_abi::gos_rt_iter_count_by_f64 as *const u8,
        ),
        (
            "gos_rt_iter_count_by_ptr",
            crate::c_abi::gos_rt_iter_count_by_ptr as *const u8,
        ),
        (
            "gos_rt_iter_filter_map_f64",
            crate::c_abi::gos_rt_iter_filter_map_f64 as *const u8,
        ),
        (
            "gos_rt_iter_filter_map_ptr",
            crate::c_abi::gos_rt_iter_filter_map_ptr as *const u8,
        ),
        (
            "gos_rt_iter_find_map_f64",
            crate::c_abi::gos_rt_iter_find_map_f64 as *const u8,
        ),
        (
            "gos_rt_iter_find_map_ptr",
            crate::c_abi::gos_rt_iter_find_map_ptr as *const u8,
        ),
        (
            "gos_rt_iter_flat_map_f64",
            crate::c_abi::gos_rt_iter_flat_map_f64 as *const u8,
        ),
        (
            "gos_rt_iter_flat_map_ptr",
            crate::c_abi::gos_rt_iter_flat_map_ptr as *const u8,
        ),
        (
            "gos_rt_iter_group_by_f64",
            crate::c_abi::gos_rt_iter_group_by_f64 as *const u8,
        ),
        (
            "gos_rt_iter_group_by_ptr",
            crate::c_abi::gos_rt_iter_group_by_ptr as *const u8,
        ),
        (
            "gos_rt_iter_max_by_f64",
            crate::c_abi::gos_rt_iter_max_by_f64 as *const u8,
        ),
        (
            "gos_rt_iter_max_by_ptr",
            crate::c_abi::gos_rt_iter_max_by_ptr as *const u8,
        ),
        (
            "gos_rt_iter_min_by_f64",
            crate::c_abi::gos_rt_iter_min_by_f64 as *const u8,
        ),
        (
            "gos_rt_iter_min_by_ptr",
            crate::c_abi::gos_rt_iter_min_by_ptr as *const u8,
        ),
        (
            "gos_rt_iter_partition_f64",
            crate::c_abi::gos_rt_iter_partition_f64 as *const u8,
        ),
        (
            "gos_rt_iter_partition_ptr",
            crate::c_abi::gos_rt_iter_partition_ptr as *const u8,
        ),
        (
            "gos_rt_iter_position_f64",
            crate::c_abi::gos_rt_iter_position_f64 as *const u8,
        ),
        (
            "gos_rt_iter_product_by_f64",
            crate::c_abi::gos_rt_iter_product_by_f64 as *const u8,
        ),
        (
            "gos_rt_iter_product_by_ptr",
            crate::c_abi::gos_rt_iter_product_by_ptr as *const u8,
        ),
        (
            "gos_rt_iter_reduce_f64",
            crate::c_abi::gos_rt_iter_reduce_f64 as *const u8,
        ),
        (
            "gos_rt_iter_reduce_ptr",
            crate::c_abi::gos_rt_iter_reduce_ptr as *const u8,
        ),
        (
            "gos_rt_iter_scan_f64",
            crate::c_abi::gos_rt_iter_scan_f64 as *const u8,
        ),
        (
            "gos_rt_iter_scan_ptr",
            crate::c_abi::gos_rt_iter_scan_ptr as *const u8,
        ),
        (
            "gos_rt_iter_skip_while_f64",
            crate::c_abi::gos_rt_iter_skip_while_f64 as *const u8,
        ),
        (
            "gos_rt_iter_skip_while_ptr",
            crate::c_abi::gos_rt_iter_skip_while_ptr as *const u8,
        ),
        (
            "gos_rt_iter_sorted_by_f64",
            crate::c_abi::gos_rt_iter_sorted_by_f64 as *const u8,
        ),
        (
            "gos_rt_iter_sorted_by_key_ptr",
            crate::c_abi::gos_rt_iter_sorted_by_key_ptr as *const u8,
        ),
        (
            "gos_rt_iter_sorted_by_ptr",
            crate::c_abi::gos_rt_iter_sorted_by_ptr as *const u8,
        ),
        (
            "gos_rt_iter_take_while_f64",
            crate::c_abi::gos_rt_iter_take_while_f64 as *const u8,
        ),
        (
            "gos_rt_iter_take_while_ptr",
            crate::c_abi::gos_rt_iter_take_while_ptr as *const u8,
        ),
        (
            "gos_rt_iter_find_f64",
            crate::c_abi::gos_rt_iter_find_f64 as *const u8,
        ),
        (
            "gos_rt_iter_find_ptr",
            crate::c_abi::gos_rt_iter_find_ptr as *const u8,
        ),
        (
            "gos_rt_iter_find_f64_flag",
            crate::c_abi::gos_rt_iter_find_f64_flag as *const u8,
        ),
        (
            "gos_rt_iter_find_ptr_flag",
            crate::c_abi::gos_rt_iter_find_ptr_flag as *const u8,
        ),
        (
            "gos_rt_iter_scan_word_f64",
            crate::c_abi::gos_rt_iter_scan_word_f64 as *const u8,
        ),
        (
            "gos_rt_iter_scan_f64_word",
            crate::c_abi::gos_rt_iter_scan_f64_word as *const u8,
        ),
        (
            "gos_rt_iter_scan_ptr_f64",
            crate::c_abi::gos_rt_iter_scan_ptr_f64 as *const u8,
        ),
        (
            "gos_rt_iter_flat_map_arr_f64",
            crate::c_abi::gos_rt_iter_flat_map_arr_f64 as *const u8,
        ),
        (
            "gos_rt_iter_flat_map_arr_ptr",
            crate::c_abi::gos_rt_iter_flat_map_arr_ptr as *const u8,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::runtime_symbol_addrs;
    use std::collections::HashSet;

    #[test]
    fn symbol_table_covers_registry() {
        let registered: HashSet<&str> =
            runtime_symbol_addrs().into_iter().map(|(n, _)| n).collect();
        let missing: Vec<&str> = gossamer_abi::REGISTRY
            .iter()
            .map(|e| e.name)
            .filter(|n| !registered.contains(n))
            .collect();
        assert!(
            missing.is_empty(),
            "runtime_symbol_addrs() is missing {} registry symbol(s); the Cranelift JIT \
             would fail to resolve them and collapse the whole program to the interpreter. \
             Add them to symbol_table.rs: {missing:?}",
            missing.len()
        );
    }

    #[test]
    fn symbol_table_has_no_duplicates() {
        let names: Vec<&str> = runtime_symbol_addrs().into_iter().map(|(n, _)| n).collect();
        let unique: HashSet<&str> = names.iter().copied().collect();
        assert_eq!(
            names.len(),
            unique.len(),
            "duplicate entries in runtime_symbol_addrs()"
        );
    }
}
