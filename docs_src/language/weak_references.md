# `lang::weak_references`

`Weak<T>` downgrade/upgrade handles. `downgrade` pins its referent for the scope that took it and aggregate stores copy, so a weak never observes a member of a reference cycle and `upgrade` answers the same on every tier whenever cycles are collected.
