//! The query child limits live Rust allocation requests before it reads its input.

#[cfg(target_os = "linux")]
type Allocator = mimalloc::MiMalloc;
#[cfg(not(target_os = "linux"))]
type Allocator = std::alloc::System;

#[cfg(target_os = "linux")]
const INNER: Allocator = mimalloc::MiMalloc;
#[cfg(not(target_os = "linux"))]
const INNER: Allocator = std::alloc::System;

/// Ordinary commands retain an unlimited allocator. Only the synchronous query child
/// sets a finite ceiling, before runtime, tracing or request decoding can start.
#[global_allocator]
static ALLOCATOR: cap::Cap<Allocator> = cap::Cap::new(INNER, usize::MAX);

/// Sets the query child's live Rust allocation ceiling.
///
/// This process must remain single-threaded until it exits. `cap` accounts for requested
/// layout bytes, including reallocations; allocator metadata and C heaps are outside
/// this quantity. The selected parser and evaluator use Rust allocation requests.
pub(super) fn limit(bytes_max: usize) -> Result<(), ()> {
    ALLOCATOR.set_limit(bytes_max)
}
