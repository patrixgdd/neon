//! Internals needed by macros. These have to be exported for the macros to work
/// but are subject to change and should never be explicitly used.

// Used by the class macro.
pub use js::class::internal::{AllocateCallback, ConstructCallback, ConstructorCallCallback, MethodCallback};
pub use vm::internal::initialize_module;

// An alias for neon_runtime so macros can refer to it.
pub mod runtime {
    pub use neon_runtime::*;
}
