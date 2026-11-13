//! Operator diagnostics removed from served tool answers.

use rift_protocol::documentation::DocumentationContext;
use rift_protocol::read::ReadWarning;

pub(crate) fn log_warnings(warnings: &[ReadWarning]) {
    for warning in warnings {
        log_warning(warning);
    }
}

fn log_warning(warning: &ReadWarning) {
    let warning = serde_json::to_value(warning)
        .unwrap_or_else(|error| unreachable!("read warnings serialize: {error}"));
    let code = warning["code"]
        .as_str()
        .unwrap_or_else(|| unreachable!("read warning has a code: {warning}"));
    rift_tracing::warn!(
        component = "mcp",
        operation = "read.warning",
        warning_code = code,
        evidence = %warning,
        "read warning"
    );
}

pub(super) fn record_warnings(warnings: &mut Vec<ReadWarning>) {
    log_warnings(warnings);
    warnings.clear();
}

pub(super) fn record_documentation(context: &mut Option<DocumentationContext>) {
    if let Some(context) = context {
        for warning in context.warnings.drain(..) {
            log_warning(&ReadWarning::Documentation { warning });
        }
    }
}
