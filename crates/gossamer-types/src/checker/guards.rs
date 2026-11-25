//! The guard a `let`-bound `m.lock()` answers.

use gossamer_ast::{Expr, ExprKind};

use super::TypeChecker;
use crate::{Ty, TyKind};

/// The wrapper type a guard `let` binds.
pub(crate) const GUARD_TYPE: &str = "__gos_sync_MutexGuard";

impl TypeChecker<'_> {
    /// The guard type `init` takes as a `let`'s value: a `lock()` of a
    /// `sync::Mutex` holds the lock to the end of the binding's block.
    pub(super) fn mutex_guard_ty(&mut self, init: &Expr) -> Option<Ty> {
        let ExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } = &init.kind
        else {
            return None;
        };
        if name.name != "lock" || !args.is_empty() {
            return None;
        }
        let receiver_ty = self.table.get(receiver.id)?;
        let receiver_ty = self.peel_refs(receiver_ty);
        let is_mutex = matches!(
            self.tcx.kind(receiver_ty),
            Some(TyKind::Adt { def, .. }) if self.tcx.def_name(*def) == Some("sync::Mutex")
        );
        if !is_mutex {
            return None;
        }
        let def = *self.user_type_defs.get(GUARD_TYPE)?;
        Some(self.tcx.intern(TyKind::Adt {
            def,
            substs: crate::Substs::new(),
        }))
    }
}
