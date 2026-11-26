//! Generic LSP stdio client for Rift language engines.
//!
//! A language engine is an external LSP server a workspace configures;
//! this crate speaks the protocol's client side to one engine child. The
//! sans-I/O core - framing, correlation, capabilities, positions, document
//! URIs - is deterministic and fully unit-tested; [`EngineSession`] is the
//! thin Tokio shell that owns the child, its bounded drains, and every
//! timeout.

pub mod capabilities;
pub mod contribution;
pub mod correlation;
pub mod framing;
mod jvm;
pub mod position;
mod rust_declarations;
pub mod session;
pub mod uri;

pub use capabilities::{Capabilities, PositionEncoding};
pub use correlation::{Correlation, RequestId};
pub use framing::Framing;
pub use jvm::JvmLanguageServer;
pub use position::LineIndex;
pub use rust_declarations::{RustDeclarationKind, RustDeclarationToken, RustDeclarationTokens};
pub use session::{EngineLaunch, EngineSession, RustMacroExpansion};
pub use uri::{EngineAddress, EngineRoots, PackageFile, PackageRoot, TreeRoot};
