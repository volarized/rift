# Upstream source

This directory contains `tracing-opentelemetry` 0.34.0 from
`https://github.com/tokio-rs/tracing-opentelemetry` at commit
`8c1a02d68034de317607ada0b1dd0d98abefd6cd`.

The crates.io archive SHA-256 is
`0a904802a1b902f43638b677ff2a650847e3b4404101b6c586d648e8c1e3e8fe`. The crate uses
the MIT license in `LICENSE`.

Rift changes one lookup in `src/layer.rs`. `OpenTelemetryLayer::on_new_span` returns when
`Context::span` has no span in this layer's saved filter map. Every other upstream callback
remains unchanged.

Three inherited trailing-whitespace instances were removed from `CHANGELOG.md` and
`src/span_ext.rs` so repository diff checks pass. These edits do not change behavior.
