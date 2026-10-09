//! Values of the `rift mcp` command line.

use rift_mcp::OutputPolicy;

/// Which representations of tool answers `rift mcp` forwards.
///
/// The variants carry no documentation of their own: clap renders a value's
/// doc comment as per-value help, which turns the whole command's help into
/// its long form.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub(super) enum OutputMode {
    All,
    #[default]
    Text,
}

impl From<OutputMode> for OutputPolicy {
    fn from(mode: OutputMode) -> Self {
        match mode {
            OutputMode::All => Self::All,
            OutputMode::Text => Self::Text,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::ValueEnum as _;
    use rift_mcp::OutputPolicy;

    use super::OutputMode;

    #[test]
    fn the_default_mode_is_text() {
        assert_eq!(OutputMode::default(), OutputMode::Text);
        assert_eq!(
            OutputPolicy::from(OutputMode::default()),
            OutputPolicy::Text
        );
    }

    #[test]
    fn each_mode_converts_to_its_policy() {
        assert_eq!(OutputPolicy::from(OutputMode::All), OutputPolicy::All);
        assert_eq!(OutputPolicy::from(OutputMode::Text), OutputPolicy::Text);
    }

    #[test]
    fn modes_are_spelled_all_and_text() {
        let names: Vec<_> = OutputMode::value_variants()
            .iter()
            .filter_map(OutputMode::to_possible_value)
            .map(|value| value.get_name().to_owned())
            .collect();
        assert_eq!(names, ["all", "text"]);
    }
}
