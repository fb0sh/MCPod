//! Filesystem layer: workspace path containment, per-file mutation queue,
//! atomic writes, and text helpers (BOM / line endings).

pub mod atomic_write;
pub mod mutation_queue;
pub mod path;
pub mod text;
