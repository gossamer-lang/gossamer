//! Lowering `database::sql` free functions.

use super::*;

impl<'a> Builder<'a> {
    pub(super) fn lower_sql_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // database::sql leaf intrinsics (called from injected
            // Gossamer wrappers; scalar/string-shaped, sentinel
            // error convention with gos_rt_sql_last_error).
            "__gos_sql_open_raw" => (
                "gos_rt_sql_open",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_last_error_raw" => ("gos_rt_sql_last_error", self.tcx.string_ty()),
            "__gos_sql_drivers_raw" => ("gos_rt_sql_drivers", self.tcx.string_ty()),
            "__gos_sql_params_new_raw" => (
                "gos_rt_sql_params_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_params_push_null_raw" => (
                "gos_rt_sql_params_push_null",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_params_push_bool_raw" => (
                "gos_rt_sql_params_push_bool",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_params_push_int_raw" => (
                "gos_rt_sql_params_push_int",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_params_push_float_raw" => (
                "gos_rt_sql_params_push_float",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_params_push_text_raw" => (
                "gos_rt_sql_params_push_text",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_params_push_blob_raw" => (
                "gos_rt_sql_params_push_blob",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_execute_raw" => (
                "gos_rt_sql_conn_execute_params",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_query_raw" => (
                "gos_rt_sql_conn_query_params",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_begin_raw" => (
                "gos_rt_sql_conn_begin",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_begin_with_raw" => (
                "gos_rt_sql_conn_begin_with",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_ping_raw" => (
                "gos_rt_sql_conn_ping",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_set_busy_timeout_raw" => (
                "gos_rt_sql_conn_set_busy_timeout",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_interrupt_raw" => (
                "gos_rt_sql_conn_interrupt",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_close_raw" => (
                "gos_rt_sql_conn_close",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_rows_next_row_raw" => (
                "gos_rt_sql_rows_next_row",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_rows_close_raw" => (
                "gos_rt_sql_rows_close",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_rows_columns_raw" => ("gos_rt_sql_rows_columns", self.tcx.string_ty()),
            "__gos_sql_row_kind_raw" => (
                "gos_rt_sql_row_kind",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_row_get_i64_raw" => (
                "gos_rt_sql_row_get_i64",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            _ => return None,
        })
    }

    pub(super) fn lower_sql_2_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "__gos_sql_row_get_f64_raw" => (
                "gos_rt_sql_row_get_f64",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "__gos_sql_row_get_bool_raw" => (
                "gos_rt_sql_row_get_bool_i64",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_row_get_text_raw" => ("gos_rt_sql_row_get_text", self.tcx.string_ty()),
            "__gos_sql_row_get_blob_raw" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_sql_row_get_blob_vec",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "__gos_sql_row_width_raw" => (
                "gos_rt_sql_row_width",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_tx_commit_raw" => (
                "gos_rt_sql_tx_commit",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_tx_rollback_raw" => (
                "gos_rt_sql_tx_rollback",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_tx_execute_raw" => (
                "gos_rt_sql_tx_execute",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_tx_savepoint_raw" => (
                "gos_rt_sql_tx_savepoint",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_tx_release_savepoint_raw" => (
                "gos_rt_sql_tx_release_savepoint",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_tx_rollback_to_savepoint_raw" => (
                "gos_rt_sql_tx_rollback_to_savepoint",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_tx_execute_params_raw" => (
                "gos_rt_sql_tx_execute_params",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_tx_query_params_raw" => (
                "gos_rt_sql_tx_query_params",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_prepare_raw" => (
                "gos_rt_sql_conn_prepare",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_stmt_execute_raw" => (
                "gos_rt_sql_stmt_execute",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_stmt_query_raw" => (
                "gos_rt_sql_stmt_query",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_stmt_close_raw" => (
                "gos_rt_sql_stmt_close",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_copy_in_raw" => (
                "gos_rt_sql_conn_copy_in",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_copy_out_run_raw" => (
                "gos_rt_sql_conn_copy_out_run",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_copy_out_take_raw" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_sql_conn_copy_out_take",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            _ => return None,
        })
    }

    pub(super) fn lower_sql_3_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "__gos_sql_conn_listen_raw" => (
                "gos_rt_sql_conn_listen",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_unlisten_raw" => (
                "gos_rt_sql_conn_unlisten",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_conn_poll_notification_raw" => (
                "gos_rt_sql_conn_poll_notification",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_notification_channel_raw" => {
                ("gos_rt_sql_notification_channel", self.tcx.string_ty())
            }
            "__gos_sql_notification_payload_raw" => {
                ("gos_rt_sql_notification_payload", self.tcx.string_ty())
            }
            "__gos_sql_notification_pid_raw" => (
                "gos_rt_sql_notification_pid",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_pool_new_raw" => (
                "gos_rt_sql_pool_new",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_pool_get_raw" => (
                "gos_rt_sql_pool_get",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_pool_live_raw" => (
                "gos_rt_sql_pool_live",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_pool_idle_raw" => (
                "gos_rt_sql_pool_idle",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_pool_close_idle_raw" => (
                "gos_rt_sql_pool_close_idle",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_migrate_up_raw" => (
                "gos_rt_sql_migrate_up",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // Gossamer-native driver side-channel helpers. The `.gos`
            // driver reads inputs / writes outputs through these; the
            // writers return unit, the readers their slot field type,
            // and the value constructors / accessors traffic in
            // sql::Value handles (i64).
            "__gos_sql_native_url" => ("gos_rt_sql_native_url", self.tcx.string_ty()),
            "__gos_sql_native_sql" => ("gos_rt_sql_native_sql", self.tcx.string_ty()),
            "__gos_sql_native_parent" => (
                "gos_rt_sql_native_parent",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_out_handle" => (
                "gos_rt_sql_native_out_handle",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_iso" => (
                "gos_rt_sql_native_iso",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_timeout" => (
                "gos_rt_sql_native_timeout",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_channel" => ("gos_rt_sql_native_channel", self.tcx.string_ty()),
            "__gos_sql_native_param_count" => (
                "gos_rt_sql_native_param_count",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_param" => (
                "gos_rt_sql_native_param",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_data" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_sql_native_data",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "__gos_sql_native_push_column" => ("gos_rt_sql_native_push_column", self.tcx.unit()),
            _ => return None,
        })
    }

    pub(super) fn lower_sql_4_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "__gos_sql_native_push_value" => ("gos_rt_sql_native_push_value", self.tcx.unit()),
            "__gos_sql_native_row_ready" => ("gos_rt_sql_native_row_ready", self.tcx.unit()),
            "__gos_sql_native_set_error" => ("gos_rt_sql_native_set_error", self.tcx.unit()),
            "__gos_sql_native_emit_bytes" => ("gos_rt_sql_native_emit_bytes", self.tcx.unit()),
            "__gos_sql_native_set_notification" => {
                ("gos_rt_sql_native_set_notification", self.tcx.unit())
            }
            "__gos_sql_native_set_handle" => ("gos_rt_sql_native_set_handle", self.tcx.unit()),
            "__gos_sql_native_handle" => (
                "gos_rt_sql_native_handle",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_null" => (
                "gos_rt_sql_native_value_null",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_bool" => (
                "gos_rt_sql_native_value_bool",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_int" => (
                "gos_rt_sql_native_value_int",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_float" => (
                "gos_rt_sql_native_value_float",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_text" => (
                "gos_rt_sql_native_value_text",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_blob" => (
                "gos_rt_sql_native_value_blob",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_kind" => (
                "gos_rt_sql_native_value_kind",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_int_of" => (
                "gos_rt_sql_native_value_int_of",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "__gos_sql_native_value_float_of" => (
                "gos_rt_sql_native_value_float_of",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "__gos_sql_native_value_text_of" => {
                ("gos_rt_sql_native_value_text_of", self.tcx.string_ty())
            }
            "__gos_sql_native_value_blob_of" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_sql_native_value_blob_of",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            _ => return None,
        })
    }
}
