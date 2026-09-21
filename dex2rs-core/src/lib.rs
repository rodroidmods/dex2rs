//! `dex2rs-core` — the Dalvik bytecode to Rust/JNI AOT compiler.
//!
//! This is a port of the `dex2c` Python package from the `d2c` project. The
//! pipeline mirrors it one step at a time:
//!
//! ```text
//! smali::SmaliClass/SmaliMethod
//!   -> dexinput::MethodInput     (linear decoded instruction list)
//!   -> graph::construct          (basic blocks + CFG + landing pads)
//!   -> block::fill_node_from_block
//!   -> ssa::IrBuilder            (SSA + type inference)
//!   -> codegen::Writer           (Rust source for the JNI method)
//! ```
//!
//! `smali` replaces the vendored androguard subset: it both parses the dex and
//! round-trips through smali, and its `DexOp` enum is the typed equivalent of
//! androguard's raw instruction words.

pub mod block;
pub mod codegen;
pub mod dexedit;
pub mod dexinput;
pub mod graph;
pub mod ir;
pub mod opcodes;
pub mod ssa;
pub mod util;

pub use ir::{BlockId, InsId, Ir, ValId};
pub use ssa::{IrBuilder, IrMethod};
