#![allow(clippy::too_many_lines, clippy::wildcard_imports)]
use super::*;

impl<'tcx> FnBuilder<'tcx> {
    pub(crate) fn compile_path(
        &mut self,
        segments: &[Ident],
        def: Option<gossamer_resolve::DefId>,
    ) -> RuntimeResult<Reg> {
        let Some(first) = segments.first() else {
            return Err(RuntimeError::UnresolvedName(String::new()));
        };
        if segments.len() == 1 {
            if let Some(tr) = self.lookup_local(&first.name) {
                return Ok(self.as_value(tr));
            }
            // Const items inline through the constant
            // pool (single-index fetch) instead of `LoadGlobal`
            // (string-keyed HashMap lookup). This is keyed by
            // `DefId`, so block-scoped consts do not collapse into
            // same-named constants from an outer scope.
            if let Some(def) = def
                && let Some(value) = self.module_consts.get(def)
            {
                let key = const_key_for_value(value);
                let idx = self.const_idx(key, value.clone());
                let dst = self.alloc_reg();
                self.emit(Op::LoadConst { dst, idx });
                return Ok(dst);
            }
            if let Some(def) = def
                && let Some(global) = self.module_consts.deferred_global(def)
            {
                let idx = self.global_idx(global);
                let dst = self.alloc_reg();
                self.emit(Op::LoadGlobal { dst, idx });
                return Ok(dst);
            }
        }
        // For multi-segment paths (`fmt::println`,
        // `http::Response::text`, ...) the VM has two builtins to
        // pick between: one registered under the tail name
        // (`text`) and one under the fully-qualified path
        // (`http::Response::text`). Emit a LoadGlobal keyed on the
        // full join - the global table has entries for both, and
        // the qualified key is unambiguous.
        let name = if segments.len() > 1 {
            // Strip `super::` / `crate::` / `self::` so a path written
            // inside an inline `mod tests` resolves the flat global.
            let stripped = strip_module_relative(segments);
            if stripped.len() == 1 {
                stripped[0].name.clone()
            } else {
                stripped
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join("::")
            }
        } else {
            first.name.clone()
        };
        let idx = self.global_idx(&name);
        let dst = self.alloc_reg();
        self.emit(Op::LoadGlobal { dst, idx });
        Ok(dst)
    }
}
