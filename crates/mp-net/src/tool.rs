//! Running an external program, and the seam that keeps it testable.
//!
//! Some things this build needs cannot reasonably be done with a GET. YouTube
//! does not publish a stable way to turn a link into a playable audio stream;
//! working it out is a moving target that `yt-dlp` tracks full time and a
//! music player would track badly. So Resonance asks `yt-dlp` instead of
//! reimplementing it.
//!
//! ## What that costs, and why it is declared rather than hidden
//!
//! A child process makes its own requests. They do not go through
//! [`Transport`](crate::http::Transport), they do not appear under `ureq` in
//! `cargo tree`, and nothing in this crate can intercept them. That is a real
//! hole in the claim this crate exists to make, and the answer is not to
//! pretend otherwise: the source registry names the program that makes them,
//! the settings screen prints it before the feature is switched on, and every
//! invocation is written to the activity log like any other request.
//!
//! The program is never bundled and never downloaded. The user installs it,
//! and [`YtDlp::locate`] finds it or reports that it did not — a music player
//! that fetched and ran an executable on its own behalf would be a much larger
//! thing to trust than one that makes HTTP requests.
//!
//! ## The seam
//!
//! [`Runner`] is to processes what [`Transport`](crate::http::Transport) is to
//! sockets. Everything above it is tested against a scripted fake, so no test
//! spawns a process any more than it opens a socket.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::NetError;

/// How long to wait for the program to be asked its version.
///
/// Short: this runs while the settings screen is being drawn, and a hung
/// version check should read as "not working" quickly rather than hang the
/// pass that called it.
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a waiting run checks whether the child has finished.
///
/// Short enough that a fast command is not padded noticeably, long enough that
/// waiting for a ten-minute download is not a spin.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The most of the program's error output that is kept.
///
/// Only ever used for a log line and a message on screen. A program in a retry
/// loop can produce a great deal of it, and none of it past the first problem
/// tells the user anything new.
const MAX_STDERR_BYTES: usize = 8 * 1024;

/// What a finished program left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// The exit status, or `None` if a signal ended it.
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    /// Error output, truncated to [`MAX_STDERR_BYTES`].
    pub stderr: String,
}

impl Output {
    pub fn succeeded(&self) -> bool {
        self.status == Some(0)
    }

    /// The last non-empty line of error output.
    ///
    /// `yt-dlp` prints its actual complaint last, after any number of retry
    /// notices, so the tail is the part worth showing.
    pub fn complaint(&self) -> Option<&str> {
        self.stderr
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
    }
}

/// Somewhere an external program can be run.
///
/// The seam that keeps callers testable without spawning anything, for exactly
/// the reason [`Transport`](crate::http::Transport) exists.
pub trait Runner: Send + Sync {
    /// Run the program with these arguments and wait for it to finish.
    ///
    /// `Err` means the program could not be run or did not finish in time. A
    /// program that ran and failed is `Ok` with a non-zero
    /// [`status`](Output::status) — deciding what a given failure *means* is
    /// the caller's business, because exit codes are the program's convention
    /// and not a standard.
    fn run(&self, args: &[&str], timeout: Duration) -> Result<Output, NetError>;
}

/// The real thing: `yt-dlp`, wherever it was found.
#[derive(Debug, Clone)]
pub struct YtDlp {
    program: PathBuf,
}

/// What the executable is called, most specific first.
#[cfg(windows)]
const PROGRAM_NAMES: &[&str] = &["yt-dlp.exe", "yt-dlp"];
#[cfg(not(windows))]
const PROGRAM_NAMES: &[&str] = &["yt-dlp"];

impl YtDlp {
    /// Find the program, preferring a path the user gave.
    ///
    /// Searching `PATH` here rather than letting the shell do it at spawn time
    /// is deliberate: it gives the settings screen an absolute path to show, so
    /// "which yt-dlp is this actually using" has an answer, and it means a
    /// missing program is discovered before anything has been started.
    pub fn locate(configured: Option<&Path>) -> Option<Self> {
        if let Some(given) = configured {
            // A path the user typed is used as given or not at all. Falling
            // back to `PATH` would quietly run a different program than the one
            // named on screen.
            return given.is_file().then(|| Self {
                program: given.to_owned(),
            });
        }

        let path = std::env::var_os("PATH")?;
        for directory in std::env::split_paths(&path) {
            for name in PROGRAM_NAMES {
                let candidate = directory.join(name);
                if candidate.is_file() {
                    return Some(Self { program: candidate });
                }
            }
        }

        None
    }

    /// Where it was found.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// What version it reports, for the settings screen.
    ///
    /// Worth showing because the failure it predicts is invisible otherwise:
    /// extraction breaks when the service changes, and an old copy fails on
    /// links a current one handles.
    pub fn version(&self) -> Option<String> {
        let output = self.run(&["--version"], VERSION_TIMEOUT).ok()?;
        if !output.succeeded() {
            return None;
        }

        let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (!version.is_empty()).then_some(version)
    }
}

impl Runner for YtDlp {
    fn run(&self, args: &[&str], timeout: Duration) -> Result<Output, NetError> {
        let mut child = Command::new(&self.program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| {
                NetError::Transport(format!("could not run {}: {err}", self.program.display()))
            })?;

        // Drained on their own threads. Polling for exit while the pipes fill
        // is how this deadlocks: the child blocks writing, the parent blocks
        // waiting, and neither one ever moves again.
        let stdout = child.stdout.take().map(drain);
        let stderr = child.stderr.take().map(drain);

        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(err) => {
                    return Err(NetError::Transport(format!(
                        "lost track of the program: {err}"
                    )));
                }
            }

            if started.elapsed() >= timeout {
                // Best effort. A child that cannot be killed is already a
                // stranger problem than this feature can solve.
                let _ = child.kill();
                let _ = child.wait();
                return Err(NetError::Transport(format!(
                    "{} did not finish within {} seconds",
                    self.program.display(),
                    timeout.as_secs()
                )));
            }

            std::thread::sleep(POLL_INTERVAL);
        };

        let stdout = stdout.map(join).unwrap_or_default();
        let stderr = stderr.map(join).unwrap_or_default();

        Ok(Output {
            status: status.code(),
            stdout,
            stderr: truncate(String::from_utf8_lossy(&stderr).into_owned()),
        })
    }
}

/// Read a pipe to the end on its own thread.
fn drain<R: Read + Send + 'static>(mut pipe: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        // A read that fails leaves whatever arrived before it, which is more
        // useful than nothing and never worse.
        let _ = pipe.read_to_end(&mut buffer);
        buffer
    })
}

fn join(handle: std::thread::JoinHandle<Vec<u8>>) -> Vec<u8> {
    handle.join().unwrap_or_default()
}

fn truncate(mut text: String) -> String {
    if text.len() <= MAX_STDERR_BYTES {
        return text;
    }

    // Truncating mid-character would panic, so step back to a boundary.
    let mut end = MAX_STDERR_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }

    text.truncate(end);
    text.push('\u{2026}');
    text
}

/// The arguments a run was given, for a log line.
///
/// Joined rather than debug-printed so the log shows something that could be
/// pasted into a terminal and tried by hand.
pub fn describe(program: &Path, args: &[&str]) -> String {
    let name = program
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.display().to_string());

    let mut described = name;
    for arg in args {
        described.push(' ');
        described.push_str(arg);
    }

    described
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A runner that answers from a script.
    ///
    /// The whole point of the trait: everything above it is tested without a
    /// process, exactly as the fetchers are tested without a socket.
    #[derive(Debug, Default)]
    struct Fake {
        scripted: Mutex<Vec<Result<Output, NetError>>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl Fake {
        fn with(answers: Vec<Result<Output, NetError>>) -> Self {
            Self {
                scripted: Mutex::new(answers),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl Runner for Fake {
        fn run(&self, args: &[&str], _timeout: Duration) -> Result<Output, NetError> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| (*arg).to_owned()).collect());

            let mut scripted = self.scripted.lock().unwrap();
            assert!(
                !scripted.is_empty(),
                "a run was made with nothing scripted for it: {args:?}"
            );

            scripted.remove(0)
        }
    }

    fn ok(stdout: &str) -> Output {
        Output {
            status: Some(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: String::new(),
        }
    }

    #[test]
    fn a_zero_exit_is_success_and_anything_else_is_not() {
        assert!(ok("{}").succeeded());

        let failed = Output {
            status: Some(1),
            stdout: Vec::new(),
            stderr: "ERROR: Video unavailable".into(),
        };
        assert!(!failed.succeeded());

        let signalled = Output {
            status: None,
            stdout: Vec::new(),
            stderr: String::new(),
        };
        assert!(!signalled.succeeded());
    }

    /// The tail of the error output is the part that says what went wrong;
    /// everything before it is usually retry noise.
    #[test]
    fn the_complaint_is_the_last_thing_said() {
        let output = Output {
            status: Some(1),
            stdout: Vec::new(),
            stderr: "WARNING: retrying (1/3)\nWARNING: retrying (2/3)\nERROR: Private video\n"
                .into(),
        };

        assert_eq!(output.complaint(), Some("ERROR: Private video"));
    }

    #[test]
    fn output_with_nothing_to_complain_about_complains_about_nothing() {
        assert_eq!(ok("{}").complaint(), None);

        let blank = Output {
            status: Some(1),
            stdout: Vec::new(),
            stderr: "\n  \n".into(),
        };
        assert_eq!(blank.complaint(), None);
    }

    /// A program in a retry loop can produce a great deal of this, and none of
    /// it past the first problem is new information.
    #[test]
    fn error_output_is_capped() {
        let huge = "e".repeat(MAX_STDERR_BYTES * 3);
        let kept = truncate(huge);

        assert!(kept.len() <= MAX_STDERR_BYTES + 4, "{} bytes", kept.len());
        assert!(kept.ends_with('\u{2026}'));
    }

    /// Truncating in the middle of a multi-byte character would panic, and the
    /// error output of a program handling somebody's music is full of them.
    #[test]
    fn capping_does_not_split_a_character() {
        let text = "\u{2192}".repeat(MAX_STDERR_BYTES);
        let kept = truncate(text);

        // Reaching here at all is most of the assertion: `String::truncate`
        // panics on a boundary it does not like.
        assert!(kept.ends_with('\u{2026}'));
    }

    #[test]
    fn short_output_is_left_alone() {
        assert_eq!(truncate("ERROR: nope".into()), "ERROR: nope");
        assert_eq!(truncate(String::new()), "");
    }

    /// A path the user typed is used as given or not at all: silently falling
    /// back to `PATH` would run a different program than the one on screen.
    #[test]
    fn a_configured_path_that_is_not_there_finds_nothing() {
        let missing = Path::new("D:/nothing/of/this/name/yt-dlp.exe");
        assert!(YtDlp::locate(Some(missing)).is_none());
    }

    #[test]
    fn a_configured_path_is_used_as_given() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let program = dir.path().join("yt-dlp.exe");
        std::fs::write(&program, b"not really a program").expect("writing the stand-in");

        let found = YtDlp::locate(Some(&program)).expect("the file is there");
        assert_eq!(found.program(), program);
    }

    /// A directory of that name is not a program, and treating one as such
    /// would turn a typo into a spawn failure at the worst moment.
    #[test]
    fn a_directory_is_not_a_program() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let program = dir.path().join("yt-dlp.exe");
        std::fs::create_dir(&program).expect("creating the stand-in");

        assert!(YtDlp::locate(Some(&program)).is_none());
    }

    #[test]
    fn the_description_reads_like_something_you_could_type() {
        let described = describe(
            Path::new("C:/tools/yt-dlp.exe"),
            &["--dump-single-json", "https://example.org/watch"],
        );

        assert_eq!(
            described,
            "yt-dlp --dump-single-json https://example.org/watch"
        );
    }

    #[test]
    fn the_fake_records_what_it_was_asked() {
        let fake = Fake::with(vec![Ok(ok("1.2.3"))]);

        let output = fake.run(&["--version"], VERSION_TIMEOUT).expect("scripted");

        assert_eq!(String::from_utf8_lossy(&output.stdout), "1.2.3");
        assert_eq!(fake.calls.lock().unwrap()[0], vec!["--version"]);
    }

    #[test]
    fn the_fake_can_refuse() {
        let fake = Fake::with(vec![Err(NetError::Transport("not installed".into()))]);

        let result = fake.run(&["--version"], VERSION_TIMEOUT);

        assert!(matches!(result, Err(NetError::Transport(_))));
    }
}
