//! TCP timeout runtime helper smoke tests.

#![cfg(not(target_arch = "wasm32"))]

use gossamer_runtime::c_abi::{
    gos_rt_result_disc, gos_rt_tcp_stream_clear_read_timeout,
    gos_rt_tcp_stream_clear_write_timeout, gos_rt_tcp_stream_set_read_timeout_ms,
    gos_rt_tcp_stream_set_write_timeout_ms,
};

#[test]
fn tcp_timeout_helpers_report_a_null_handle_as_an_error() {
    // A handle the program holds always names its socket; the null handle,
    // which names none, is the error path.
    // SAFETY: the null handle is accepted by every helper and never dereferenced.
    unsafe {
        assert_eq!(
            gos_rt_result_disc(gos_rt_tcp_stream_set_read_timeout_ms(0, 10)),
            1
        );
        assert_eq!(
            gos_rt_result_disc(gos_rt_tcp_stream_set_write_timeout_ms(0, 10)),
            1
        );
        assert_eq!(
            gos_rt_result_disc(gos_rt_tcp_stream_clear_read_timeout(0)),
            1
        );
        assert_eq!(
            gos_rt_result_disc(gos_rt_tcp_stream_clear_write_timeout(0)),
            1
        );
    }
}
