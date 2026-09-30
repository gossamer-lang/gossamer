//! Algorithms the bytecode VM's standard library and the native runtime both
//! need, written once so the tiers cannot disagree about them.

#![forbid(unsafe_code)]

pub mod json;
