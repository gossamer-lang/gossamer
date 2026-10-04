//! Lowering `utf8`, `unicode`, `encoding`, `strings`, `strconv`, `compress`, and codec free functions.

use super::*;

impl<'a> Builder<'a> {
    pub(super) fn lower_utf8_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // utf8::decode_rune family - (char, i64) by-value tuple.
            "utf8::decode_rune"
            | "utf8::decode_rune_in_string"
            | "utf8::decode_last_rune"
            | "utf8::decode_last_rune_in_string" => {
                let c = self.tcx.char_ty();
                let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![c, i]));
                let sym = match joined {
                    "utf8::decode_rune" => "gos_rt_utf8_decode_rune",
                    "utf8::decode_rune_in_string" => "gos_rt_utf8_decode_rune_in_string",
                    "utf8::decode_last_rune" => "gos_rt_utf8_decode_last_rune",
                    _ => "gos_rt_utf8_decode_last_rune_in_string",
                };
                (sym, tup)
            }
            "utf8::append_rune" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_utf8_append_rune",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            // ---------------------------------------------------------------
            // std::utf8 - counting, length, and validity helpers.
            "utf8::rune_count_in_string" => (
                "gos_rt_utf8_rune_count_in_string",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // `rune_count`, `is_valid` and `full_rune` take `Vec<u8>`; their
            // `*_in_string` twins take a `String`. Each has to reach the shim
            // whose parameter is the pointer the call actually passes.
            "utf8::rune_count" => (
                "gos_rt_utf8_rune_count_bytes",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "utf8::rune_len" => (
                "gos_rt_utf8_rune_len",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "utf8::valid_rune" => ("gos_rt_utf8_valid_rune", self.tcx.bool_ty()),
            "utf8::valid_string" => ("gos_rt_utf8_valid_string", self.tcx.bool_ty()),
            "utf8::is_valid" => ("gos_rt_utf8_is_valid", self.tcx.bool_ty()),
            "utf8::full_rune_in_string" => ("gos_rt_utf8_full_rune_in_string", self.tcx.bool_ty()),
            "utf8::full_rune" => ("gos_rt_utf8_full_rune", self.tcx.bool_ty()),
            "utf8::rune_start" => ("gos_rt_utf8_rune_start", self.tcx.bool_ty()),
            _ => return None,
        })
    }

    pub(super) fn lower_unicode_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // ---------------------------------------------------------------
            // std::unicode - general-category predicates, casing,
            // normalization, segmentation. Char args lower as u32,
            // string args as `*const c_char`, bool results as i64
            // (auto-truncated to i1 by the LLVM lowerer). Vec<String>
            // returns route through `gos_rt_unicode_*` helpers that
            // build a GosVec with `elem_kind = STRING`.
            "unicode::is_letter" => ("gos_rt_unicode_is_letter", self.tcx.bool_ty()),
            "unicode::char_width" => (
                "gos_rt_unicode_char_width",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "unicode::str_width" => (
                "gos_rt_unicode_str_width",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "unicode::is_digit" => ("gos_rt_unicode_is_digit", self.tcx.bool_ty()),
            "unicode::is_number" => ("gos_rt_unicode_is_number", self.tcx.bool_ty()),
            "unicode::is_space" => ("gos_rt_unicode_is_space", self.tcx.bool_ty()),
            "unicode::is_upper" => ("gos_rt_unicode_is_upper", self.tcx.bool_ty()),
            "unicode::is_lower" => ("gos_rt_unicode_is_lower", self.tcx.bool_ty()),
            "unicode::is_title" => ("gos_rt_unicode_is_title", self.tcx.bool_ty()),
            "unicode::is_punct" => ("gos_rt_unicode_is_punct", self.tcx.bool_ty()),
            "unicode::is_symbol" => ("gos_rt_unicode_is_symbol", self.tcx.bool_ty()),
            "unicode::is_mark" => ("gos_rt_unicode_is_mark", self.tcx.bool_ty()),
            "unicode::is_print" => ("gos_rt_unicode_is_print", self.tcx.bool_ty()),
            "unicode::is_graphic" => ("gos_rt_unicode_is_graphic", self.tcx.bool_ty()),
            "unicode::is_control" => ("gos_rt_unicode_is_control", self.tcx.bool_ty()),
            "unicode::is_assigned" => ("gos_rt_unicode_is_assigned", self.tcx.bool_ty()),
            "unicode::combining_class" => (
                "gos_rt_unicode_combining_class",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "unicode::to_upper" => ("gos_rt_unicode_to_upper", self.tcx.char_ty()),
            "unicode::to_lower" => ("gos_rt_unicode_to_lower", self.tcx.char_ty()),
            "unicode::to_title" => ("gos_rt_unicode_to_title", self.tcx.char_ty()),
            "unicode::simple_fold" => ("gos_rt_unicode_simple_fold", self.tcx.char_ty()),
            "unicode::to_upper_str" => ("gos_rt_unicode_to_upper_str", self.tcx.string_ty()),
            "unicode::to_lower_str" => ("gos_rt_unicode_to_lower_str", self.tcx.string_ty()),
            "unicode::fold_case" => ("gos_rt_unicode_fold_case", self.tcx.string_ty()),
            "unicode::nfc" => ("gos_rt_unicode_nfc", self.tcx.string_ty()),
            "unicode::nfd" => ("gos_rt_unicode_nfd", self.tcx.string_ty()),
            "unicode::nfkc" => ("gos_rt_unicode_nfkc", self.tcx.string_ty()),
            "unicode::nfkd" => ("gos_rt_unicode_nfkd", self.tcx.string_ty()),
            "unicode::is_nfc" => ("gos_rt_unicode_is_nfc", self.tcx.bool_ty()),
            "unicode::is_nfd" => ("gos_rt_unicode_is_nfd", self.tcx.bool_ty()),
            "unicode::is_nfkc" => ("gos_rt_unicode_is_nfkc", self.tcx.bool_ty()),
            "unicode::is_nfkd" => ("gos_rt_unicode_is_nfkd", self.tcx.bool_ty()),
            "unicode::graphemes" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_unicode_graphemes", v)
            }
            "unicode::grapheme_count" => (
                "gos_rt_unicode_grapheme_count",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "unicode::words" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_unicode_words", v)
            }
            "unicode::word_bounds" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_unicode_word_bounds", v)
            }
            "unicode::word_count" => (
                "gos_rt_unicode_word_count",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "unicode::sentences" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_unicode_sentences", v)
            }
            "unicode::sentence_count" => (
                "gos_rt_unicode_sentence_count",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            _ => return None,
        })
    }

    pub(super) fn lower_encoding_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // encoding::utf16::* (previously VM-only).
            "encoding::utf16::is_surrogate" | "utf16::is_surrogate" => {
                ("gos_rt_utf16_is_surrogate", self.tcx.bool_ty())
            }
            "encoding::utf16::rune_len" | "utf16::rune_len" => (
                "gos_rt_utf16_rune_len",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "encoding::utf16::decode_surrogate_pair" | "utf16::decode_surrogate_pair" => {
                let c = self.tcx.char_ty();
                let substs = gossamer_types::Substs::from_types([c]);
                let opt = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX - 1),
                    substs,
                });
                ("gos_rt_utf16_decode_surrogate_pair", opt)
            }
            "encoding::utf16::encode_string" | "utf16::encode_string" => {
                let u16_ty = self.tcx.int_ty(gossamer_types::IntTy::U16);
                (
                    "gos_rt_utf16_encode_string",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u16_ty)),
                )
            }
            "encoding::utf16::decode_to_string" | "utf16::decode_to_string" => {
                ("gos_rt_utf16_decode_to_string", self.tcx.string_ty())
            }
            "encoding::hex::encode" | "hex::encode" => {
                ("gos_rt_encoding_hex_encode", self.tcx.string_ty())
            }
            "encoding::hex::decode" | "hex::decode" => {
                ("gos_rt_encoding_hex_decode", self.result_vec_u8_error_ty())
            }
            "encoding::base64::encode" | "base64::encode" => {
                ("gos_rt_encoding_base64_encode", self.tcx.string_ty())
            }
            "encoding::base64::decode" | "base64::decode" => (
                "gos_rt_encoding_base64_decode",
                self.result_vec_u8_error_ty(),
            ),
            "encoding::base32::encode" | "base32::encode" => {
                ("gos_rt_encoding_base32_encode", self.tcx.string_ty())
            }
            "encoding::base32::encode_hex" | "base32::encode_hex" => {
                ("gos_rt_encoding_base32_encode_hex", self.tcx.string_ty())
            }
            "encoding::base32::decode" | "base32::decode" => (
                "gos_rt_encoding_base32_decode",
                self.result_vec_u8_error_ty(),
            ),
            "encoding::base32::decode_hex" | "base32::decode_hex" => (
                "gos_rt_encoding_base32_decode_hex",
                self.result_vec_u8_error_ty(),
            ),
            // encoding::binary - put_* return [u8]; get_* return
            // Result<i64>; uvarint/varint return Result<(i64,i64)>.
            "encoding::binary::put_u8"
            | "encoding::binary::put_u16_be"
            | "encoding::binary::put_u16_le"
            | "encoding::binary::put_u32_be"
            | "encoding::binary::put_u32_le"
            | "encoding::binary::put_u64_be"
            | "encoding::binary::put_u64_le"
            | "encoding::binary::put_uvarint"
            | "encoding::binary::put_varint" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                let sym = match joined {
                    "encoding::binary::put_u8" => "gos_rt_bin_put_u8",
                    "encoding::binary::put_u16_be" => "gos_rt_bin_put_u16_be",
                    "encoding::binary::put_u16_le" => "gos_rt_bin_put_u16_le",
                    "encoding::binary::put_u32_be" => "gos_rt_bin_put_u32_be",
                    "encoding::binary::put_u32_le" => "gos_rt_bin_put_u32_le",
                    "encoding::binary::put_u64_be" => "gos_rt_bin_put_u64_be",
                    "encoding::binary::put_u64_le" => "gos_rt_bin_put_u64_le",
                    "encoding::binary::put_uvarint" => "gos_rt_bin_put_uvarint",
                    _ => "gos_rt_bin_put_varint",
                };
                (sym, self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)))
            }
            _ => return None,
        })
    }

    pub(super) fn lower_encoding_2_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "encoding::binary::get_u8"
            | "encoding::binary::get_u16_be"
            | "encoding::binary::get_u16_le"
            | "encoding::binary::get_u32_be"
            | "encoding::binary::get_u32_le"
            | "encoding::binary::get_u64_be"
            | "encoding::binary::get_u64_le" => {
                let sym = match joined {
                    "encoding::binary::get_u8" => "gos_rt_bin_get_u8",
                    "encoding::binary::get_u16_be" => "gos_rt_bin_get_u16_be",
                    "encoding::binary::get_u16_le" => "gos_rt_bin_get_u16_le",
                    "encoding::binary::get_u32_be" => "gos_rt_bin_get_u32_be",
                    "encoding::binary::get_u32_le" => "gos_rt_bin_get_u32_le",
                    "encoding::binary::get_u64_be" => "gos_rt_bin_get_u64_be",
                    _ => "gos_rt_bin_get_u64_le",
                };
                (sym, self.result_i64_error_adt_ty())
            }
            // The `_at` family reads and writes through the caller's own
            // buffer, so it answers the natural unsigned width rather than
            // the lossy `i64` the append-and-return forms use.
            "encoding::binary::get_u16_be_at"
            | "encoding::binary::get_u16_le_at"
            | "encoding::binary::get_u32_be_at"
            | "encoding::binary::get_u32_le_at"
            | "encoding::binary::get_u64_be_at"
            | "encoding::binary::get_u64_le_at" => {
                let (sym, width) = match joined {
                    "encoding::binary::get_u16_be_at" => {
                        ("gos_rt_bin_get_u16_be_at", gossamer_types::IntTy::U16)
                    }
                    "encoding::binary::get_u16_le_at" => {
                        ("gos_rt_bin_get_u16_le_at", gossamer_types::IntTy::U16)
                    }
                    "encoding::binary::get_u32_be_at" => {
                        ("gos_rt_bin_get_u32_be_at", gossamer_types::IntTy::U32)
                    }
                    "encoding::binary::get_u32_le_at" => {
                        ("gos_rt_bin_get_u32_le_at", gossamer_types::IntTy::U32)
                    }
                    "encoding::binary::get_u64_be_at" => {
                        ("gos_rt_bin_get_u64_be_at", gossamer_types::IntTy::U64)
                    }
                    _ => ("gos_rt_bin_get_u64_le_at", gossamer_types::IntTy::U64),
                };
                let value = self.tcx.int_ty(width);
                (sym, self.result_of(value))
            }
            "encoding::binary::put_u16_be_at"
            | "encoding::binary::put_u16_le_at"
            | "encoding::binary::put_u32_be_at"
            | "encoding::binary::put_u32_le_at"
            | "encoding::binary::put_u64_be_at"
            | "encoding::binary::put_u64_le_at" => {
                let sym = match joined {
                    "encoding::binary::put_u16_be_at" => "gos_rt_bin_put_u16_be_at",
                    "encoding::binary::put_u16_le_at" => "gos_rt_bin_put_u16_le_at",
                    "encoding::binary::put_u32_be_at" => "gos_rt_bin_put_u32_be_at",
                    "encoding::binary::put_u32_le_at" => "gos_rt_bin_put_u32_le_at",
                    "encoding::binary::put_u64_be_at" => "gos_rt_bin_put_u64_be_at",
                    _ => "gos_rt_bin_put_u64_le_at",
                };
                let unit = self.tcx.unit();
                (sym, self.result_of(unit))
            }
            "encoding::binary::uvarint" => ("gos_rt_bin_uvarint", self.result_pair_i64_error_ty()),
            "encoding::binary::varint" => ("gos_rt_bin_varint", self.result_pair_i64_error_ty()),
            "encoding::csv::parse_line" | "csv::parse_line" => {
                let s = self.tcx.string_ty();
                (
                    "gos_rt_csv_parse_line",
                    self.tcx.intern(gossamer_types::TyKind::Vec(s)),
                )
            }
            "encoding::csv::read" | "csv::read" => {
                ("gos_rt_csv_read", self.result_vec_vec_string_error_ty())
            }
            "encoding::csv::write" | "csv::write" => ("gos_rt_csv_write", self.tcx.string_ty()),
            "encoding::ascii85::encode" | "ascii85::encode" => {
                ("gos_rt_encoding_ascii85_encode", self.tcx.string_ty())
            }
            "encoding::ascii85::decode" | "ascii85::decode" => (
                "gos_rt_encoding_ascii85_decode",
                self.result_vec_u8_error_ty(),
            ),
            "encoding::xml::escape" | "xml::escape" => {
                ("gos_rt_encoding_xml_escape", self.tcx.string_ty())
            }
            "encoding::xml::parse" | "xml::parse" => {
                ("gos_rt_xml_parse", self.result_json_value_error_adt_ty())
            }
            "encoding::xml::encode" | "xml::encode" => ("gos_rt_xml_encode", self.tcx.string_ty()),
            "encoding::base32::encode_string" | "base32::encode_string" => {
                ("gos_rt_encoding_base32_encode_string", self.tcx.string_ty())
            }
            "encoding::base32::decode_string" | "base32::decode_string" => (
                "gos_rt_encoding_base32_decode_string",
                self.result_string_error_adt_ty(),
            ),
            _ => return None,
        })
    }

    pub(super) fn lower_strings_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // 0.7.0 stdlib wiring - string-surface free fns that
            // the VM already exposes but that lacked a compiled-tier
            // runtime entry point. Each maps a fully-qualified
            // module path to the matching `gos_rt_*` helper.
            "strings::join" => ("gos_rt_strings_join", self.tcx.string_ty()),
            "strings::split_once" | "strings::rsplit_once" => {
                let s = self.tcx.string_ty();
                let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![s, s]));
                let substs = gossamer_types::Substs::from_types([tup]);
                let opt_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX - 1),
                    substs,
                });
                let sym = if joined == "strings::split_once" {
                    "gos_rt_str_split_once"
                } else {
                    "gos_rt_str_rsplit_once"
                };
                (sym, opt_ty)
            }
            "strings::count" => (
                "gos_rt_str_count",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // 0.10.0 - string-surface free fns. Each routes to the
            // matching `gos_rt_str_*` runtime helper (same shim that
            // already backs the method-call form). Without these,
            // MIR emits `@strings::trim` etc. as a literal symbol and
            // LLVM `opt` fails with `use of undefined value`.
            "strings::trim" => ("gos_rt_str_trim", self.tcx.string_ty()),
            "strings::trim_start" => ("gos_rt_str_trim_start", self.tcx.string_ty()),
            "strings::trim_end" => ("gos_rt_str_trim_end", self.tcx.string_ty()),
            "strings::to_uppercase" => ("gos_rt_str_to_upper", self.tcx.string_ty()),
            "strings::to_lowercase" => ("gos_rt_str_to_lower", self.tcx.string_ty()),
            "strings::contains" => ("gos_rt_str_contains", self.tcx.bool_ty()),
            "strings::replace" => ("gos_rt_str_replace", self.tcx.string_ty()),
            "strings::starts_with" => ("gos_rt_str_starts_with", self.tcx.bool_ty()),
            "strings::ends_with" => ("gos_rt_str_ends_with", self.tcx.bool_ty()),
            "strings::repeat" => ("gos_rt_str_repeat", self.tcx.string_ty()),
            "strings::byte_len" => (
                "gos_rt_str_byte_len",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "strings::byte_at" => (
                "gos_rt_str_byte_at",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "strings::substring" => ("gos_rt_str_substring", self.tcx.string_ty()),
            "strings::bytes" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty));
                ("gos_rt_str_as_bytes", v)
            }
            "strings::chars" => {
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(i64_ty));
                ("gos_rt_str_chars", v)
            }
            "strings::lines" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_str_lines", v)
            }
            "strings::split" => {
                let s = self.tcx.string_ty();
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
                ("gos_rt_str_split", v)
            }
            "strings::find" => {
                let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let substs = gossamer_types::Substs::from_types([i]);
                let opt_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX - 1),
                    substs,
                });
                ("gos_rt_str_find_opt", opt_ty)
            }
            "strings::trim_start_matches" => ("gos_rt_str_lstrip_chars", self.tcx.string_ty()),
            "strings::trim_end_matches" => ("gos_rt_str_rstrip_chars", self.tcx.string_ty()),
            "strings::center" => ("gos_rt_str_center", self.tcx.string_ty()),
            "strings::slice" => ("gos_rt_str_slice", self.result_string_error_adt_ty()),
            // 0.10.0 - remaining strings::* free fns previously
            // VM-only. Each routes to the matching gos_rt_str_*
            // runtime helper backed by gossamer_std::strings.
            "strings::splitn" => {
                let s = self.tcx.string_ty();
                (
                    "gos_rt_str_splitn",
                    self.tcx.intern(gossamer_types::TyKind::Vec(s)),
                )
            }
            "strings::split_whitespace" => {
                let s = self.tcx.string_ty();
                (
                    "gos_rt_str_split_whitespace",
                    self.tcx.intern(gossamer_types::TyKind::Vec(s)),
                )
            }
            "strings::replacen" => ("gos_rt_str_replacen", self.tcx.string_ty()),
            "strings::to_title" => ("gos_rt_str_to_title", self.tcx.string_ty()),
            "strings::trim_matches" => ("gos_rt_str_trim_matches", self.tcx.string_ty()),
            "strings::pad_left" => ("gos_rt_str_pad_left", self.tcx.string_ty()),
            "strings::pad_right" => ("gos_rt_str_pad_right", self.tcx.string_ty()),
            "strings::contains_any" => ("gos_rt_str_contains_any", self.tcx.bool_ty()),
            "strings::equal_fold" => ("gos_rt_str_equal_fold", self.tcx.bool_ty()),
            "strings::parse" => ("gos_rt_parse_i64_result", self.result_i64_error_adt_ty()),
            _ => return None,
        })
    }

    pub(super) fn lower_strings_2_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "strings::find_any" => ("gos_rt_str_index_any", self.option_i64_adt_ty()),
            "strings::rfind_any" => ("gos_rt_str_last_index_any", self.option_i64_adt_ty()),
            "strings::strip_prefix" => ("gos_rt_str_strip_prefix", self.option_string_adt_ty()),
            "strings::strip_suffix" => ("gos_rt_str_strip_suffix", self.option_string_adt_ty()),
            "strings::to_i64" => ("gos_rt_str_to_i64_opt", self.option_i64_adt_ty()),
            "strings::to_f64" => ("gos_rt_str_to_f64_opt", self.option_f64_adt_ty()),
            "strings::to_bool" => ("gos_rt_str_to_bool_opt", self.option_bool_adt_ty()),
            // String-as-receiver `rfind` returns Option<i64>; same
            // discriminant-packed shape as `find_opt`.
            "strings::rfind" => {
                let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let substs = gossamer_types::Substs::from_types([i]);
                let opt_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
                    def: gossamer_resolve::DefId::local(u32::MAX - 1),
                    substs,
                });
                ("gos_rt_str_rfind_opt", opt_ty)
            }
            _ => return None,
        })
    }

    pub(super) fn lower_strconv_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // 0.10.0 - strconv free fns. parse_* return
            // Result<T, errors::Error> packed as a *mut GosResult;
            // format_* return String.
            "strconv::parse_i64" => ("gos_rt_strconv_parse_i64", self.result_i64_error_adt_ty()),
            "strconv::parse_u64" => ("gos_rt_strconv_parse_u64", self.result_u64_error_adt_ty()),
            "strconv::parse_f64" => ("gos_rt_strconv_parse_f64", self.result_f64_error_adt_ty()),
            "strconv::parse_bool" => ("gos_rt_strconv_parse_bool", self.result_bool_error_adt_ty()),
            "strconv::parse_i64_radix" => (
                "gos_rt_strconv_parse_i64_radix",
                self.result_i64_error_adt_ty(),
            ),
            "strconv::format_i64_radix" => {
                ("gos_rt_strconv_format_i64_radix", self.tcx.string_ty())
            }
            "strconv::quote" => ("gos_rt_strconv_quote", self.tcx.string_ty()),
            "strconv::unquote" => ("gos_rt_strconv_unquote", self.result_string_error_adt_ty()),
            // Format-spec intrinsics from `{:spec}` expansion. `__fmt_radix`
            // and `__fmt_upper` reuse the strconv/strings shims; `__fmt_pad`
            // applies width/alignment/fill to an already-rendered string.
            "__fmt_radix" => ("gos_rt_fmt_radix_i64", self.tcx.string_ty()),
            "__fmt_upper" => ("gos_rt_str_to_upper", self.tcx.string_ty()),
            "__fmt_pad" => ("gos_rt_fmt_pad", self.tcx.string_ty()),
            "strconv::format_i64" => ("gos_rt_strconv_format_i64", self.tcx.string_ty()),
            "strconv::format_u64" => ("gos_rt_strconv_format_i64", self.tcx.string_ty()),
            "strconv::format_f64" => ("gos_rt_strconv_format_f64", self.tcx.string_ty()),
            "strconv::format_bool" => ("gos_rt_strconv_format_bool", self.tcx.string_ty()),
            _ => return None,
        })
    }

    pub(super) fn lower_compress_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "compress::gzip::encode" | "gzip::encode" => {
                ("gos_rt_compress_gzip_encode", self.result_vec_u8_error_ty())
            }
            "compress::gzip::decode" | "gzip::decode" => {
                ("gos_rt_compress_gzip_decode", self.result_vec_u8_error_ty())
            }
            "compress::flate::compress" | "flate::compress" => (
                "gos_rt_compress_flate_compress",
                self.result_vec_u8_error_ty(),
            ),
            "compress::flate::decompress" | "flate::decompress" => (
                "gos_rt_compress_flate_decompress",
                self.result_vec_u8_error_ty(),
            ),
            "compress::bzip2::compress" | "bzip2::compress" => (
                "gos_rt_compress_bzip2_compress",
                self.result_vec_u8_error_ty(),
            ),
            "compress::bzip2::decompress" | "bzip2::decompress" => (
                "gos_rt_compress_bzip2_decompress",
                self.result_vec_u8_error_ty(),
            ),
            "compress::zstd::encode" | "zstd::encode" => {
                ("gos_rt_compress_zstd_encode", self.result_vec_u8_error_ty())
            }
            "compress::zstd::encode_level" | "zstd::encode_level" => (
                "gos_rt_compress_zstd_encode_level",
                self.result_vec_u8_error_ty(),
            ),
            "compress::zstd::decode" | "zstd::decode" => {
                ("gos_rt_compress_zstd_decode", self.result_vec_u8_error_ty())
            }
            "compress::zlib::compress" | "zlib::compress" => (
                "gos_rt_compress_zlib_compress",
                self.result_vec_u8_error_ty(),
            ),
            "compress::zlib::decompress" | "zlib::decompress" => (
                "gos_rt_compress_zlib_decompress",
                self.result_vec_u8_error_ty(),
            ),
            _ => return None,
        })
    }

    pub(super) fn lower_codec_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // pem leaf intrinsics (called from injected Gossamer
            // wrappers; return tuples/bytes the wrappers fold into
            // real `Block` structs).
            "__gos_pem_decode_raw" => {
                let tup = self.tuple_str_bytes_ty();
                ("gos_rt_pem_decode_raw", self.result_of(tup))
            }
            "__gos_pem_decode_all_raw" => {
                let tup = self.tuple_str_bytes_ty();
                let vec = self.tcx.intern(gossamer_types::TyKind::Vec(tup));
                ("gos_rt_pem_decode_all_raw", self.result_of(vec))
            }
            "__gos_pem_encode_raw" => ("gos_rt_pem_encode_raw", self.tcx.string_ty()),
            "__gos_x509_parse_pem_raw" => {
                let tup = self.tuple_cert_info_ty();
                ("gos_rt_x509_parse_pem_raw", self.result_of(tup))
            }
            "__gos_fs_metadata_raw" => {
                let tup = self.tuple_fs_metadata_ty();
                ("gos_rt_fs_metadata_raw", self.result_of(tup))
            }
            "__gos_fs_read_dir_raw" => {
                let tup = self.tuple_dir_entry_ty();
                let vec = self.tcx.intern(gossamer_types::TyKind::Vec(tup));
                ("gos_rt_fs_read_dir_raw", self.result_of(vec))
            }
            "__gos_process_run_raw" => {
                let tup = self.tuple_process_output_ty();
                ("gos_rt_exec_run_raw", self.result_of(tup))
            }
            "__gos_process_run_in_raw" => {
                let tup = self.tuple_process_output_ty();
                ("gos_rt_exec_run_in_raw", self.result_of(tup))
            }
            "__gos_process_pipeline_run_raw" => {
                let tup = self.tuple_process_output_ty();
                ("gos_rt_exec_pipeline_run_raw", self.result_of(tup))
            }
            "__gos_tar_read_raw" | "__gos_zip_read_raw" => {
                let tup = self.tuple_entry_ty();
                let vec = self.tcx.intern(gossamer_types::TyKind::Vec(tup));
                let sym = if joined == "__gos_tar_read_raw" {
                    "gos_rt_tar_read_raw"
                } else {
                    "gos_rt_zip_read_raw"
                };
                (sym, self.result_of(vec))
            }
            // tar/zip write take `[(String,[u8])]` tuples and return
            // Result<[u8]> - no struct, so they lower directly.
            "archive::tar::write" | "tar::write" => {
                ("gos_rt_tar_write", self.result_vec_u8_error_ty())
            }
            "archive::zip::write" | "zip::write" => {
                ("gos_rt_zip_write", self.result_vec_u8_error_ty())
            }
            "html::escape" => ("gos_rt_html_escape", self.tcx.string_ty()),
            "html::unescape" => ("gos_rt_html_unescape", self.tcx.string_ty()),
            "html::template::render_json" => (
                "gos_rt_html_template_render_json",
                self.result_string_error_adt_ty(),
            ),
            _ => return None,
        })
    }
}
