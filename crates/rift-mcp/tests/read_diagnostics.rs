//! Request diagnostics observed on a current-thread Tokio test runtime.

use std::error::Error;

use rift_tracing::{LogDrain, LogFilterError, ScopedRecorder};
use serde_json::Value;

/// Captures the structured warning records the MCP server logs during reads.
pub(crate) struct ReadDiagnostics {
    _recorder: ScopedRecorder,
    drain: LogDrain,
}

impl ReadDiagnostics {
    #[allow(
        dead_code,
        reason = "shared harness also supports reads in a polling loop"
    )]
    pub(crate) async fn observe(
        read: impl Future<Output = Result<Value, Box<dyn Error>>>,
    ) -> Result<(Value, Vec<Value>), Box<dyn Error>> {
        let mut diagnostics = Self::capture()?;
        let answer = read.await?;
        assert!(answer.get("warnings").is_none(), "{answer:#}");
        Ok((answer, diagnostics.take()?))
    }

    pub(crate) fn capture() -> Result<Self, LogFilterError> {
        let (recorder, drain) = ScopedRecorder::builder()
            .capture("rift_mcp::output::warnings=warn")
            .install()?;
        Ok(Self {
            _recorder: recorder,
            drain,
        })
    }

    pub(crate) fn take(&mut self) -> Result<Vec<Value>, Box<dyn Error>> {
        self.drain
            .queued_records()
            .into_iter()
            .filter(|record| record.operation() == "read.warning")
            .map(|record| {
                let fields: Value = serde_json::from_str(record.fields())?;
                let warning: Value = serde_json::from_str(
                    fields["evidence"]
                        .as_str()
                        .ok_or("warning lacks evidence")?,
                )?;
                assert_eq!(warning["code"], fields["warning_code"], "{fields}");
                Ok(warning)
            })
            .collect()
    }
}
