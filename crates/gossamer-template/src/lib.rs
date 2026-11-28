//! Text and context-aware HTML template engines for Gossamer.
//!
//! Relocated below `gossamer-runtime` so the runtime's template registry
//! (`gossamer_runtime::templates`), which every tier renders through, can
//! use it without a dependency cycle through `gossamer-std`.

pub mod html;
pub mod text;
