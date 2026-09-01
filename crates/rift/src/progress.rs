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
/// Message drawn while the latest release is being looked up.
///
/// Named so the terminal-width regression test in this module's `tests`
/// draws the exact production text, which carries a double-width emoji.
///
/// Every stage glyph is a single-codepoint emoji: indicatif measures a drawn
/// line one `char` at a time, so an emoji built with a variation selector
/// (U+FE0F) measures one column short of what a terminal renders, and the
/// line indicatif pads to the terminal width then wraps onto a second row,
/// leaving one stale line behind per redraw.
const CHECKING_RELEASE_MESSAGE: &str = "🔍 Checking the latest rift release...";
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
            UpdateStage::CheckingRelease => line.start(CHECKING_RELEASE_MESSAGE),
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
                line.finish(format!("📥 Downloaded {downloaded}"));
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
            UpdateStage::Installed => line.finish("🚀 Installed"),
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
        Some(version) => format!("📥 Downloading rift v{version}"),
        None => "📥 Downloading rift".to_owned(),
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
    use unicode_width::UnicodeWidthStr as _;

    use super::{
        CHECKING_RELEASE_MESSAGE, TerminalProgress, download_bar, rendered_bytes, stage_style,
    };
    use crate::update::{UpdateProgress, UpdateStage};

    /// A terminal keeping every frame a progress bar drew on it.
    ///
    /// A frame is everything written between two `flush` calls: indicatif
    /// flushes once per redraw, so each frame is exactly what one
    /// `draw_to_term` call sent, and its total cell width is what a real
    /// terminal would show before wrapping.
    #[derive(Debug, Clone)]
    struct RecordingTerminal {
        width: u16,
        frames: Arc<Mutex<Vec<String>>>,
        current_frame: Arc<Mutex<String>>,
    }

    impl RecordingTerminal {
        /// A terminal `width` columns wide, with no frames drawn yet.
        fn new(width: u16) -> Self {
            Self {
                width,
                frames: Arc::new(Mutex::new(Vec::new())),
                current_frame: Arc::new(Mutex::new(String::new())),
            }
        }

        /// Every completed frame, oldest first.
        fn frames(&self) -> Vec<String> {
            self.frames
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        /// Every byte drawn across every frame, concatenated.
        fn drawn(&self) -> String {
            self.frames().concat()
        }
    }

    impl Default for RecordingTerminal {
        fn default() -> Self {
            Self::new(120)
        }
    }

    impl TermLike for RecordingTerminal {
        fn width(&self) -> u16 {
            self.width
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
            self.current_frame
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push_str(text);
            Ok(())
        }

        fn clear_line(&self) -> io::Result<()> {
            Ok(())
        }

        fn flush(&self) -> io::Result<()> {
            let mut current_frame = self
                .current_frame
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            self.frames
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(std::mem::take(&mut *current_frame));
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
        assert!(drawn.contains("📥 Downloading rift v0.0.26"), "{drawn}");
        assert!(drawn.contains("12.3 MB / 45.1 MB"), "{drawn}");
    }

    #[test]
    fn an_unsized_download_draws_a_byte_counter() {
        let bar = download_bar(None, None);
        let drawn = drawn_line(&bar, 640_000);
        assert!(drawn.contains("📥 Downloading rift"), "{drawn}");
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
            UpdateStage::Installed,
        ] {
            progress.report(stage);
        }
    }

    /// A terminal column budget under the production message's real width, so
    /// the wide-char undercount defect this test guards against pads the
    /// drawn line past what a real terminal can show without wrapping.
    const NARROW_TERMINAL_WIDTH: u16 = 40;

    /// A terminal wide enough that a download frame fits on one row, so any
    /// frame drawn past this width is over-padding, never content wrapping.
    const WIDE_TERMINAL_WIDTH: u16 = 120;

    #[test]
    fn a_download_frame_never_pads_past_the_terminal_width() {
        let bar = download_bar(Some(&Version::new(0, 0, 30)), Some(14_100_000));
        let terminal = RecordingTerminal::new(WIDE_TERMINAL_WIDTH);
        bar.set_draw_target(ProgressDrawTarget::term_like(Box::new(terminal.clone())));
        bar.set_position(1_365);
        bar.force_draw();

        // indicatif pads every drawn line to the terminal width and relies on
        // the terminal wrapping at that width instead of writing a newline. A
        // frame measuring wider than the terminal wraps onto a second row the
        // next redraw never clears, so the bar leaves one stale line behind
        // per redraw.
        let frames = terminal.frames();
        assert!(!frames.is_empty(), "the bar must draw at least one frame");
        for frame in &frames {
            let drawn_width = frame.width();
            assert!(
                drawn_width <= usize::from(WIDE_TERMINAL_WIDTH),
                "a drawn frame must fit the terminal: width={WIDE_TERMINAL_WIDTH}, \
                 drawn_width={drawn_width}, frame={frame:?}"
            );
        }
    }

    #[test]
    fn a_double_width_stage_message_never_overflows_a_narrow_terminal() {
        let spinner = ProgressBar::new_spinner().with_message(CHECKING_RELEASE_MESSAGE);
        let terminal = RecordingTerminal::new(NARROW_TERMINAL_WIDTH);
        spinner.set_draw_target(ProgressDrawTarget::term_like(Box::new(terminal.clone())));
        spinner.force_draw();

        // `unicode_width` measures independently of whatever indicatif chose
        // for its own padding: indicatif derives the padding from the same
        // function it would be compared against, so a comparison against
        // that same function fits by construction regardless of correctness.
        let frames = terminal.frames();
        assert!(
            !frames.is_empty(),
            "the spinner must draw at least one frame"
        );
        for frame in &frames {
            let drawn_width = frame.width();
            assert!(
                drawn_width <= usize::from(NARROW_TERMINAL_WIDTH),
                "a drawn frame must fit the terminal: width={NARROW_TERMINAL_WIDTH}, \
                 drawn_width={drawn_width}, frame={frame:?}"
            );
        }
    }
}
