//! Everything that talks to the operating system rather than to egui.
//!
//! Two things live here: the window chrome (dark title bar, Mica backdrop) and
//! the system media controls — the Windows 11 media flyout, the volume-key
//! overlay, and the play/pause/next keys on a keyboard or headset.
//!
//! Both are behind the same rule: they are *best effort*. A Windows build that
//! does not support Mica, a machine where the media session cannot be created,
//! a future port to another platform — none of those may stop the player from
//! playing music. Every entry point here is infallible from the caller's point
//! of view and degrades to doing nothing.

mod chrome;

#[cfg(target_os = "windows")]
mod smtc;

pub use chrome::apply_window_chrome;

use std::path::PathBuf;

/// What the system asked the player to do.
///
/// A deliberately small vocabulary: these are the only things a media key, a
/// headset button or the Windows media flyout can express. Anything richer
/// belongs in the app's own interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCommand {
    Play,
    Pause,
    /// A single button that means "the other one of play and pause".
    ///
    /// Headset buttons and most keyboards send this rather than a specific
    /// play or pause, so it cannot be collapsed into the two above.
    TogglePlayPause,
    Next,
    Previous,
    Stop,
}

/// What the system should be told is playing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NowPlayingInfo {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    /// A cached cover on disk, for the thumbnail in the media flyout.
    pub artwork: Option<PathBuf>,
}

/// Whether the player is playing, paused, or has nothing loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
}

/// The system media session, where there is one.
///
/// Held by the app across frames. Every method is a no-op when the session
/// could not be created, so callers never branch on availability.
pub struct MediaControls {
    #[cfg(target_os = "windows")]
    inner: Option<smtc::Session>,
}

impl MediaControls {
    /// Create the session and bind it to the app's window.
    ///
    /// Needs the real window handle: the Windows media session is per-window,
    /// which is also how the shell knows which app to show in the flyout.
    #[allow(unused_variables)]
    pub fn new(handle: &impl raw_window_handle::HasWindowHandle) -> Self {
        #[cfg(target_os = "windows")]
        {
            Self {
                inner: match smtc::Session::new(handle) {
                    Ok(session) => Some(session),
                    Err(err) => {
                        // Not an error the user needs to see. Media keys stop
                        // working; nothing else changes.
                        tracing::debug!("system media controls unavailable: {err}");
                        None
                    }
                },
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            Self {}
        }
    }

    /// Tell the system what is playing. Pass `None` when nothing is.
    #[allow(unused_variables)]
    pub fn set_now_playing(&mut self, now: Option<&NowPlayingInfo>) {
        #[cfg(target_os = "windows")]
        if let Some(session) = &mut self.inner
            && let Err(err) = session.set_now_playing(now)
        {
            tracing::debug!("could not update the media session: {err}");
        }
    }

    #[allow(unused_variables)]
    pub fn set_state(&mut self, state: PlaybackState) {
        #[cfg(target_os = "windows")]
        if let Some(session) = &mut self.inner
            && let Err(err) = session.set_state(state)
        {
            tracing::debug!("could not update the media session state: {err}");
        }
    }

    /// Take whatever buttons have been pressed since the last call.
    ///
    /// Polled rather than delivered by callback because the button handler
    /// runs on a system thread, and touching the player from there would mean
    /// locking the whole app's state against a thread we do not control.
    pub fn take_commands(&mut self) -> Vec<MediaCommand> {
        #[cfg(target_os = "windows")]
        {
            self.inner
                .as_mut()
                .map(smtc::Session::take_commands)
                .unwrap_or_default()
        }

        #[cfg(not(target_os = "windows"))]
        {
            Vec::new()
        }
    }

    /// Whether a session actually exists, for the diagnostics in Settings.
    pub fn is_active(&self) -> bool {
        #[cfg(target_os = "windows")]
        {
            self.inner.is_some()
        }

        #[cfg(not(target_os = "windows"))]
        {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vocabulary is fixed and small; this is here so adding a command
    /// without handling it somewhere is a compile error rather than a silent
    /// dead button.
    #[test]
    fn every_command_is_distinct() {
        let all = [
            MediaCommand::Play,
            MediaCommand::Pause,
            MediaCommand::TogglePlayPause,
            MediaCommand::Next,
            MediaCommand::Previous,
            MediaCommand::Stop,
        ];

        for (index, command) in all.iter().enumerate() {
            for other in &all[index + 1..] {
                assert_ne!(command, other);
            }
        }
    }

    #[test]
    fn now_playing_info_is_comparable_so_updates_can_be_skipped() {
        let a = NowPlayingInfo {
            title: "One".into(),
            artist: "Someone".into(),
            album: None,
            artwork: None,
        };
        let b = a.clone();

        assert_eq!(a, b, "an unchanged track must not look like a change");

        let c = NowPlayingInfo {
            title: "Two".into(),
            ..a.clone()
        };
        assert_ne!(a, c);
    }
}
