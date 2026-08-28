//! The Windows System Media Transport Controls.
//!
//! This is what puts Resonance in the Win11 media flyout above the volume
//! overlay, and — the part that matters most day to day — what makes the
//! play/pause/next keys on a keyboard or headset work while the window is not
//! focused. Windows routes those keys to whichever app owns the current media
//! session; an app with no session simply never receives them.
//!
//! The session is created for the app's own window through the interop
//! interface, which is the supported route for a Win32 app that is not using
//! WinRT's `MediaPlayer` to do its own playback.
//!
//! Buttons arrive on a system thread. Rather than reach into the player from
//! there, presses are pushed onto a small queue that the UI drains once a
//! frame. That keeps every mutation of player state on the one thread that
//! owns it, and means a stuck UI can never deadlock the system's event
//! dispatch.

use std::future::IntoFuture;
use std::path::Path;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Foundation::TypedEventHandler;
use windows::Media::{
    MediaPlaybackStatus, MediaPlaybackType, SystemMediaTransportControls,
    SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
};
use windows::Storage::StorageFile;
use windows::Storage::Streams::RandomAccessStreamReference;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::WinRT::ISystemMediaTransportControlsInterop;
use windows::core::{HSTRING, Result as WinResult};

use super::{MediaCommand, NowPlayingInfo, PlaybackState};

/// Presses waiting to be drained by the UI thread.
type Queue = Arc<Mutex<Vec<MediaCommand>>>;

pub struct Session {
    controls: SystemMediaTransportControls,
    queue: Queue,

    /// What the system has already been told.
    ///
    /// Updating the display is not free — it marshals strings and re-reads the
    /// thumbnail file — and the caller has no cheap way to know whether
    /// anything changed, so the comparison lives here.
    shown: Option<NowPlayingInfo>,
    state: Option<PlaybackState>,
}

impl Session {
    pub fn new(handle: &impl HasWindowHandle) -> WinResult<Self> {
        let hwnd = hwnd_of(handle).ok_or_else(|| {
            windows::core::Error::new(
                windows::Win32::Foundation::E_HANDLE,
                "no Win32 window handle",
            )
        })?;

        // The session belongs to a window, which is how the shell attributes
        // it to this app in the flyout.
        let interop: ISystemMediaTransportControlsInterop = windows::core::factory::<
            SystemMediaTransportControls,
            ISystemMediaTransportControlsInterop,
        >()?;

        let controls: SystemMediaTransportControls = unsafe { interop.GetForWindow(hwnd)? };

        controls.SetIsEnabled(true)?;
        controls.SetIsPlayEnabled(true)?;
        controls.SetIsPauseEnabled(true)?;
        controls.SetIsNextEnabled(true)?;
        controls.SetIsPreviousEnabled(true)?;
        // Deliberately off: there is no stop in this player's transport, and a
        // button that does nothing is worse than an absent one.
        controls.SetIsStopEnabled(false)?;

        let queue: Queue = Arc::default();

        let sink = queue.clone();
        controls.ButtonPressed(&TypedEventHandler::<
            SystemMediaTransportControls,
            SystemMediaTransportControlsButtonPressedEventArgs,
        >::new(move |_, args| {
            let Some(args) = args.as_ref() else {
                return Ok(());
            };

            if let Some(command) = translate(args.Button()?)
                && let Ok(mut queue) = sink.lock()
            {
                // A key held down can repeat faster than the UI redraws.
                // Bounded so a burst cannot grow without limit while the
                // window is minimised and repainting slowly.
                if queue.len() < 32 {
                    queue.push(command);
                }
            }

            Ok(())
        }))?;

        // Music, not video: it is what makes the flyout show the album layout.
        controls
            .DisplayUpdater()?
            .SetType(MediaPlaybackType::Music)?;

        Ok(Self {
            controls,
            queue,
            shown: None,
            state: None,
        })
    }

    pub fn set_now_playing(&mut self, now: Option<&NowPlayingInfo>) -> WinResult<()> {
        if self.shown.as_ref() == now {
            return Ok(());
        }
        self.shown = now.cloned();

        let updater = self.controls.DisplayUpdater()?;

        let Some(now) = now else {
            updater.ClearAll()?;
            updater.Update()?;
            return Ok(());
        };

        updater.SetType(MediaPlaybackType::Music)?;

        let music = updater.MusicProperties()?;
        music.SetTitle(&HSTRING::from(now.title.as_str()))?;
        music.SetArtist(&HSTRING::from(now.artist.as_str()))?;
        music.SetAlbumTitle(&HSTRING::from(now.album.as_deref().unwrap_or_default()))?;

        // A failed thumbnail must not cost the title and artist, so it is
        // tried separately and its error dropped.
        match now.artwork.as_deref().and_then(thumbnail) {
            Some(stream) => updater.SetThumbnail(&stream)?,
            None => updater.SetThumbnail(None)?,
        }

        updater.Update()
    }

    pub fn set_state(&mut self, state: PlaybackState) -> WinResult<()> {
        if self.state == Some(state) {
            return Ok(());
        }
        self.state = Some(state);

        self.controls.SetPlaybackStatus(match state {
            PlaybackState::Playing => MediaPlaybackStatus::Playing,
            PlaybackState::Paused => MediaPlaybackStatus::Paused,
            PlaybackState::Stopped => MediaPlaybackStatus::Stopped,
        })
    }

    pub fn take_commands(&mut self) -> Vec<MediaCommand> {
        self.queue
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Leaving an enabled session behind means the shell keeps offering a
        // flyout for an app that has gone.
        let _ = self.controls.SetIsEnabled(false);
    }
}

/// How long to wait for the shell to hand back a `StorageFile`.
///
/// Generous for opening a local file that our own cache wrote moments ago, and
/// short enough that a hung shell costs one stutter rather than the app.
const THUMBNAIL_TIMEOUT: Duration = Duration::from_millis(1500);

/// Load a cached cover as a stream the shell can read.
///
/// Opening a `StorageFile` is only offered asynchronously, and the result is
/// needed before the display update is sent, so this waits for it. Blocking is
/// acceptable precisely because of when it runs: once per track change, on a
/// small local JPEG, never per frame. The deadline is the important part —
/// without one, a shell that never completes the operation would hang the UI
/// thread outright.
fn thumbnail(path: &Path) -> Option<RandomAccessStreamReference> {
    let path = HSTRING::from(path.to_string_lossy().as_ref());
    let operation = StorageFile::GetFileFromPathAsync(&path).ok()?;

    let file: StorageFile = block_on(operation, THUMBNAIL_TIMEOUT)?.ok()?;
    RandomAccessStreamReference::CreateFromFile(&file).ok()
}

/// Wakes the thread that is waiting on a future.
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Drive a future to completion on this thread, giving up after `timeout`.
///
/// Hand-rolled rather than pulling in an async runtime for one call. WinRT
/// async is "hot start" — the operation is already running when we get it — so
/// there is nothing to schedule: this only has to wait for the completion
/// handler to wake it, and stop waiting if it never does.
fn block_on<F>(future: F, timeout: Duration) -> Option<F::Output>
where
    F: IntoFuture,
{
    let mut future = pin!(future.into_future());
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut context = Context::from_waker(&waker);

    let deadline = Instant::now() + timeout;

    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return Some(value);
        }

        let now = Instant::now();
        if now >= deadline {
            return None;
        }

        // A spurious wake just polls again, which is exactly right.
        thread::park_timeout(deadline - now);
    }
}

fn translate(button: SystemMediaTransportControlsButton) -> Option<MediaCommand> {
    match button {
        SystemMediaTransportControlsButton::Play => Some(MediaCommand::Play),
        SystemMediaTransportControlsButton::Pause => Some(MediaCommand::Pause),
        SystemMediaTransportControlsButton::Stop => Some(MediaCommand::Stop),
        SystemMediaTransportControlsButton::Next => Some(MediaCommand::Next),
        SystemMediaTransportControlsButton::Previous => Some(MediaCommand::Previous),
        // Record, fast-forward, rewind, channel up/down. Nothing this player
        // does, and guessing at a mapping would be worse than ignoring them.
        _ => None,
    }
}

fn hwnd_of(handle: &impl HasWindowHandle) -> Option<HWND> {
    match handle.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(win32) => Some(HWND(win32.hwnd.get() as *mut core::ffi::c_void)),
        _ => None,
    }
}
