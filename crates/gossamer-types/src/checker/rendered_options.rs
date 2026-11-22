//! `Option` values rendered as text through `{}`.

use gossamer_ast::NodeId;
use gossamer_lex::Span;

use super::{OPTION_DEF_LOCAL, TypeChecker};
use crate::{TyKind, TypeError};

impl TypeChecker<'_> {
    /// Notes a value a `{}` placeholder renders, so an `Option` among them is
    /// reported once inference settles its type. Code the compiler writes
    /// renders an `Option` on purpose and is left out.
    pub(super) fn note_rendered_option(&mut self, value: NodeId, span: Span) {
        if self
            .current_fn_name
            .as_deref()
            .is_some_and(|name| name.starts_with("__"))
        {
            return;
        }
        self.deferred_rendered_options.push((value, span));
    }

    /// GL0067 for a `{}` placeholder whose value is an `Option` rendered among
    /// other text: a `None` there reads as the word `None` in the sentence.
    pub(super) fn check_deferred_rendered_options(&mut self) {
        let mut reported = std::collections::HashSet::new();
        for (value, span) in std::mem::take(&mut self.deferred_rendered_options) {
            let Some(ty) = self.table.get(value) else {
                continue;
            };
            let resolved = self.infer.resolve(self.tcx, ty);
            let Some(TyKind::Adt { def, substs }) = self.tcx.kind(resolved).cloned() else {
                continue;
            };
            if def.local != OPTION_DEF_LOCAL || !reported.insert((span.start, span.end)) {
                continue;
            }
            let inner = substs
                .types()
                .first()
                .map_or_else(|| "_".to_string(), |t| self.render_public_ty(*t));
            self.emit(TypeError::OptionRendered { inner }, span);
        }
    }
}
