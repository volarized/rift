//! The subscriber the unscoped stream installs as the process's global default.
//!
//! A test that installs a recorder hands span ids of the recorder's registry to tasks on
//! other threads: `parent: &span` passes the id to whichever subscriber the opening thread
//! runs under. Under the global default, that registry never issued the id and panics:
//! "tried to clone {:?}, but no span exists with that ID" (`clone_span`), and "tried to drop
//! a ref to {:?}, but no such span exists!" (`try_close`), in `tracing-subscriber` 0.3.23's
//! `registry/sharded.rs`. Before the stream, those threads ran under no subscriber, which
//! enables nothing. [`StopAtRecorder`], the outermost layer of the stream, keeps that from
//! the first recorder's install on: as a layer with no filter of its own it filters the
//! whole subscriber, which enables nothing more, so no span opens under it to meet such
//! an id.

use std::sync::atomic::Ordering;

use tracing::subscriber::Interest;
use tracing::{Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

use super::RECORDER_INSTALLED;

/// Enables nothing once the process installed a recorder.
pub(super) struct StopAtRecorder;

impl<S: Subscriber> Layer<S> for StopAtRecorder {
    /// Asked again at each span and record: the answer changes at a recorder's install.
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }

    fn enabled(&self, _metadata: &Metadata<'_>, _context: Context<'_, S>) -> bool {
        !RECORDER_INSTALLED.load(Ordering::Relaxed)
    }
}
