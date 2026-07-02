use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::fmt::Display;

// Two-bar layout (current file above overall totals) shared by copy_all and verify_all
// so both phases render the same way. Callers just push byte positions and route
// persistent lines through println/suspend so bars never tear them.
pub struct Progress {
    mp: MultiProgress,
    file: ProgressBar,
    overall: ProgressBar,
}

impl Progress {
    pub fn new(total_bytes: u64) -> Self {
        let mp = MultiProgress::new();

        let file = mp.add(ProgressBar::new(0));
        file.set_style(
            ProgressStyle::with_template(
                "[{prefix}] {wide_msg} {bar:30} {bytes}/{total_bytes} ({bytes_per_sec})",
            )
            .unwrap()
            .progress_chars("=> "),
        );

        let overall = mp.add(ProgressBar::new(total_bytes));
        overall.set_style(
            ProgressStyle::with_template(
                "Overall {bar:30} {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
            )
            .unwrap()
            .progress_chars("=> "),
        );

        Self { mp, file, overall }
    }

    // Resets the file bar for a new file; prefix is "i/n".
    pub fn file_start(&self, prefix: &str, name: &str, size: u64) {
        self.file.set_length(size);
        self.file.set_position(0);
        self.file.set_prefix(prefix.to_string());
        self.file.set_message(name.to_string());
    }

    pub fn update(&self, file_bytes: u64, overall_bytes: u64) {
        self.file.set_position(file_bytes);
        self.overall.set_position(overall_bytes);
    }

    // Skipped files never touch the file bar; only overall advances.
    pub fn advance_overall(&self, bytes: u64) {
        self.overall.inc(bytes);
    }

    // Persists a completed/skipped/failed line above the bars, in order. Goes through
    // suspend + a real println! rather than MultiProgress::println: the latter routes
    // through the draw target and is silently dropped once that target is hidden
    // (e.g. output piped to a file), which would lose these lines entirely.
    pub fn println(&self, msg: impl Display) {
        let msg = msg.to_string();
        self.mp.suspend(|| println!("{msg}"));
    }

    // Runs f with the bars temporarily hidden, so plain eprintln/stdin prompts don't
    // get torn by a redraw.
    pub fn suspend<F: FnOnce() -> R, R>(&self, f: F) -> R {
        self.mp.suspend(f)
    }

    pub fn finish(&self) {
        self.file.finish_and_clear();
        self.overall.finish_and_clear();
    }
}
