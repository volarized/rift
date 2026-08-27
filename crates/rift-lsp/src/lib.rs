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
pub mod position;
pub mod session;
pub mod uri;

pub use capabilities::{Capabilities, CapabilitiesError, CapabilitiesFault, PositionEncoding};
pub use correlation::{Correlation, CorrelationError, CorrelationFault, RequestId};
pub use framing::{Framing, FramingError, FramingFault};
pub use position::{LineIndex, PositionError, PositionFault};
pub use session::{EngineError, EngineFault, EngineLaunch, EngineSession};
pub use uri::{TreeRoot, UriError, UriFault};
