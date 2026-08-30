//! Terminal drawing for the stages of `rift update`.

use std::borrow::Cow;
use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressState, ProgressStyle};
use semver::Version;

use crate::update::{UpdateProgress, UpdateStage};

/// Redraw interval of the spinner standing for a running stage.
const SPINNER_TICK: Duration = Duration::from_millis(80);
/// Download line drawn when the response declared the archive's size.
const SIZED_DOWNLOAD_TEMPLATE: &str = "{msg}  [{bar}]  {bytes} / {total_bytes}";
/// Download line drawn when the response declared no size.
const UNSIZED_DOWNLOAD_TEMPLATE: &str = "{msg}  {bytes}";
/// Decimal byte units, smallest first.
const BYTE_UNITS: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
/// Step between two adjacent entries of `BYTE_UNITS`.
const BYTE_UNIT_STEP: u64 = 1000;

/// Renders a byte count the way GitHub sizes a release asset.
///
/// Decimal, not binary: the release page and the release API report asset
/// sizes in powers of ten, so a rendered size matches what the operator
/// reads there. One fraction digit is kept and a whole value drops it, so
/// the forms are `12.3 MB`, `640 KB`, and `12 B`.
pub(crate) fn rendered_bytes(bytes: u64) -> String {
    let mut value = bytes;
    let mut remainder = 0;
    let mut unit = 0;
    while value >= BYTE_UNIT_STEP && unit + 1 < BYTE_UNITS.len() {
        remainder = value % BYTE_UNIT_STEP;
        value /= BYTE_UNIT_STEP;
        unit += 1;
    }
    let tenths = remainder / 100;
    let name = BYTE_UNITS[unit];
    if tenths == 0 {
        format!("{value} {name}")
    } else {
        format!("{value}.{tenths} {name}")
    }
}

/// Draws one update stage at a time on stderr.
///
/// Every line goes to `ProgressDrawTarget::stderr`, which hides itself when
/// stderr is not a terminal, so piped output and test transcripts stay clean.
pub(crate) struct TerminalProgress {
    line: Mutex<StageLine>,
}

impl TerminalProgress {
    /// Starts with no line drawn.
    pub(crate) fn new() -> Self {
        Self {
            line: Mutex::new(StageLine {
                drawn: ProgressBar::hidden(),
                latest: None,
                received_bytes: None,
            }),
        }
    }
}

impl Default for TerminalProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl UpdateProgress for TerminalProgress {
    fn report(&self, stage: UpdateStage) {
        let mut line = self.line.lock().unwrap_or_else(PoisonError::into_inner);
        match stage {
            UpdateStage::CheckingRelease => line.start("🔍 Checking the latest rift release..."),
            UpdateStage::ReleaseFound { latest } => {
                line.finish(format!("🔍 Latest release: v{latest}"));
                line.latest = Some(latest);
            }
            UpdateStage::Downloading {
                received_bytes,
                total_bytes,
            } => line.download(received_bytes, total_bytes),
            UpdateStage::Verifying => {
                let downloaded = rendered_bytes(line.received_bytes.unwrap_or(0));
                line.finish(format!("⬇️  Downloaded {downloaded}"));
                line.start("🔐 Verifying the checksum...");
            }
            UpdateStage::Extracting => {
                line.finish("🔐 Checksum verified");
                line.start("📦 Extracting the binary...");
            }
            UpdateStage::Installing => {
                line.finish("📦 Binary extracted");
                line.start("🚀 Installing...");
            }
        }
    }
}

/// The stderr line an update draws on, and what labels it.
struct StageLine {
    drawn: ProgressBar,
    latest: Option<Version>,
    received_bytes: Option<u64>,
}

impl StageLine {
    /// Replaces the drawn line with a spinner carrying `message`.
    fn start(&mut self, message: impl Into<Cow<'static, str>>) {
        let spinner = ProgressBar::new_spinner().with_message(message);
        spinner.enable_steady_tick(SPINNER_TICK);
        self.drawn = spinner;
    }

    /// Closes the drawn line with the text the finished stage leaves behind.
    fn finish(&self, message: impl Into<Cow<'static, str>>) {
        self.drawn.finish_with_message(message);
    }

    /// Opens the byte bar on the first report, then advances it.
    fn download(&mut self, received_bytes: u64, total_bytes: Option<u64>) {
        if self.received_bytes.is_none() {
            self.drawn = download_bar(self.latest.as_ref(), total_bytes);
        }
        self.received_bytes = Some(received_bytes);
        self.drawn.set_position(received_bytes);
    }
}

/// One byte bar for the archive download, sized when the total is known.
fn download_bar(latest: Option<&Version>, total_bytes: Option<u64>) -> ProgressBar {
    let bar = match total_bytes {
        Some(total) => ProgressBar::new(total).with_style(stage_style(SIZED_DOWNLOAD_TEMPLATE)),
        None => ProgressBar::no_length().with_style(stage_style(UNSIZED_DOWNLOAD_TEMPLATE)),
    };
    bar.set_message(match latest {
        Some(version) => format!("⬇️  Downloading rift v{version}"),
        None => "⬇️  Downloading rift".to_owned(),
    });
    bar
}

/// Compiles one bar template, rendering byte counts through `rendered_bytes`.
///
/// The `bytes` and `total_bytes` keys are bound here so the bar and the
/// finished line report one set of units. A template the parser refuses
/// falls back to the built-in bar.
fn stage_style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .with_key(
            "bytes",
            |state: &ProgressState, writer: &mut dyn fmt::Write| {
                let _ = writer.write_str(&rendered_bytes(state.pos()));
            },
        )
        .with_key(
            "total_bytes",
            |state: &ProgressState, writer: &mut dyn fmt::Write| {
                let _ = writer.write_str(&rendered_bytes(state.len().unwrap_or_default()));
            },
        )
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex, PoisonError};

    use indicatif::{ProgressBar, ProgressDrawTarget, TermLike};
    use semver::Version;

    use super::{TerminalProgress, download_bar, rendered_bytes, stage_style};
    use crate::update::{UpdateProgress, UpdateStage};

    /// A terminal keeping every byte a progress bar drew on it.
    #[derive(Debug, Clone, Default)]
    struct RecordingTerminal {
        drawn: Arc<Mutex<String>>,
    }

    impl RecordingTerminal {
        fn drawn(&self) -> String {
            self.drawn
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl TermLike for RecordingTerminal {
        fn width(&self) -> u16 {
            120
        }

        fn move_cursor_up(&self, _lines: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_down(&self, _lines: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_right(&self, _columns: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_left(&self, _columns: usize) -> io::Result<()> {
            Ok(())
        }

        fn write_line(&self, line: &str) -> io::Result<()> {
            self.write_str(line)
        }

        fn write_str(&self, text: &str) -> io::Result<()> {
            self.drawn
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push_str(text);
            Ok(())
        }

        fn clear_line(&self) -> io::Result<()> {
            Ok(())
        }

        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Draws `bar` onto a recording terminal and returns what it wrote.
    fn drawn_line(bar: &ProgressBar, position: u64) -> String {
        let terminal = RecordingTerminal::default();
        bar.set_draw_target(ProgressDrawTarget::term_like(Box::new(terminal.clone())));
        bar.set_position(position);
        bar.force_draw();
        terminal.drawn()
    }

    #[test]
    fn byte_counts_render_in_decimal_units() {
        assert_eq!(rendered_bytes(0), "0 B");
        assert_eq!(rendered_bytes(12), "12 B");
        assert_eq!(rendered_bytes(999), "999 B");
        assert_eq!(rendered_bytes(1_000), "1 KB");
        assert_eq!(rendered_bytes(640_000), "640 KB");
        assert_eq!(rendered_bytes(12_300_000), "12.3 MB");
        assert_eq!(rendered_bytes(u64::MAX), "18.4 EB");
    }

    #[test]
    fn a_sized_download_draws_received_and_total_bytes() {
        let bar = download_bar(Some(&Version::new(0, 0, 26)), Some(45_100_000));
        let drawn = drawn_line(&bar, 12_300_000);
        assert!(drawn.contains("⬇️  Downloading rift v0.0.26"), "{drawn}");
        assert!(drawn.contains("12.3 MB / 45.1 MB"), "{drawn}");
    }

    #[test]
    fn an_unsized_download_draws_a_byte_counter() {
        let bar = download_bar(None, None);
        let drawn = drawn_line(&bar, 640_000);
        assert!(drawn.contains("⬇️  Downloading rift"), "{drawn}");
        assert!(drawn.contains("640 KB"), "{drawn}");
    }

    #[test]
    fn a_refused_template_falls_back_to_the_built_in_bar() {
        let bar = ProgressBar::new(10).with_style(stage_style("{bytes:x}"));
        let drawn = drawn_line(&bar, 5);
        assert!(drawn.contains("5/10"), "{drawn}");
    }

    #[test]
    fn every_stage_draws_off_a_terminal_without_output() {
        let progress = TerminalProgress::default();
        for stage in [
            UpdateStage::CheckingRelease,
            UpdateStage::ReleaseFound {
                latest: Version::new(0, 0, 26),
            },
            UpdateStage::Downloading {
                received_bytes: 1,
                total_bytes: Some(2),
            },
            UpdateStage::Downloading {
                received_bytes: 2,
                total_bytes: Some(2),
            },
            UpdateStage::Verifying,
            UpdateStage::Extracting,
            UpdateStage::Installing,
        ] {
            progress.report(stage);
        }
    }
}
