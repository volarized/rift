//! Build identity and executable selected by the serving process.

use std::path::PathBuf;

use crate::{BuildCheckout, RiftMcp};

/// Build identity and optional compatible executable for bounded query processes.
/// A library entry point keeps its existing build selection until an executable is supplied.
#[derive(Clone, Debug)]
pub struct ServeOptions {
    pub(crate) checkout: BuildCheckout,
    pub(crate) query_program: Option<PathBuf>,
}

impl From<BuildCheckout> for ServeOptions {
    fn from(checkout: BuildCheckout) -> Self {
        Self {
            checkout,
            query_program: None,
        }
    }
}

impl ServeOptions {
    /// Selects the compatible Rift executable used for bounded query child processes.
    #[must_use]
    pub fn with_query_program(mut self, program: PathBuf) -> Self {
        self.query_program = Some(program);
        self
    }

    pub(crate) fn apply(self, server: RiftMcp) -> RiftMcp {
        match self.query_program {
            Some(program) => server.with_query_program(program),
            None => server,
        }
    }
}
