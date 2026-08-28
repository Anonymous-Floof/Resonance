//! The Adaptive theme: the shell takes its colour from the current cover.
//!
//! Two things make this harder than "read the accent and use it".
//!
//! The first is cost. Reading a palette touches the disk, and the answer must
//! be ready on the frame a track changes — so results are cached per cover id
//! and a cover is only ever read once per session.
//!
//! The second is that a hard cut is unpleasant. Album art changes every three
//! minutes; if the entire interface snapped to a new colour each time, it
//! would read as a glitch. The accent is crossfaded over a beat instead, which
//! turns a jarring event into something you notice only if you look.

use std::collections::HashMap;

use mp_core::color::Rgb;
use mp_core::library::CoverPalette;

use crate::library::LibraryState;

/// How long the accent takes to travel from one cover to the next.
///
/// Long enough to read as a transition rather than a repaint, short enough
/// that the interface is settled well before anyone has finished reading the
/// new track's title.
const FADE_SECONDS: f32 = 0.75;

/// Tracks the colour of the cover on screen, and the fade between covers.
#[derive(Debug, Default)]
pub struct Adaptive {
    /// Palettes already read, keyed by cover id.
    ///
    /// `None` is cached as deliberately as `Some`: a monochrome cover has no
    /// accent, and that is an answer worth remembering rather than a miss to
    /// retry on every frame.
    cache: HashMap<String, Option<CoverPalette>>,

    /// The cover the current target came from, so an unchanged track costs a
    /// string comparison and nothing else.
    showing: Option<String>,

    from: Option<Rgb>,
    to: Option<Rgb>,
    /// 0.0 at the start of a fade, 1.0 when it has arrived.
    progress: f32,

    /// The palette of the cover being faded *to*, for the ambient background
    /// on the full-screen view.
    palette: Option<CoverPalette>,
}

impl Adaptive {
    pub fn new() -> Self {
        Self {
            progress: 1.0,
            ..Self::default()
        }
    }

    /// Point the theme at a cover. Returns whether a fade started.
    ///
    /// Safe to call every frame: the work happens only when the cover id
    /// actually changes.
    pub fn observe(&mut self, library: &LibraryState, art_id: Option<&str>) -> bool {
        if self.showing.as_deref() == art_id {
            return false;
        }

        let palette = match art_id {
            Some(id) => self
                .cache
                .entry(id.to_owned())
                .or_insert_with(|| library.library().art_palette(id))
                .clone(),
            None => None,
        };

        // Start from wherever the fade had got to, not from the previous
        // target. Changing tracks twice in a second otherwise makes the colour
        // jump backwards before setting off again.
        self.from = self.current();
        self.to = palette.as_ref().and_then(|palette| palette.accent);
        self.progress = 0.0;

        self.showing = art_id.map(str::to_owned);
        self.palette = palette;

        true
    }

    /// Advance the fade. Returns whether the colour moved this frame.
    pub fn advance(&mut self, dt: f32) -> bool {
        if self.progress >= 1.0 {
            return false;
        }

        self.progress = (self.progress + dt / FADE_SECONDS).min(1.0);
        true
    }

    /// The accent as it stands mid-fade, or `None` where the cover offers no
    /// colour at either end.
    fn current(&self) -> Option<Rgb> {
        match (self.from, self.to) {
            (Some(from), Some(to)) => Some(from.mix(to, self.progress)),
            (Some(from), None) => Some(from),
            (None, Some(to)) => Some(to),
            (None, None) => None,
        }
    }

    /// The accent to theme with, given the colour configured in settings.
    ///
    /// The fallback is resolved *before* mixing rather than after, so moving
    /// from a monochrome cover to a coloured one fades from the configured
    /// accent instead of cutting to the new colour at full strength.
    pub fn accent(&self, configured: Rgb) -> Rgb {
        let from = self.from.unwrap_or(configured);
        let to = self.to.unwrap_or(configured);
        from.mix(to, self.progress)
    }

    /// Whether a fade is still running, so the caller knows to keep repainting.
    pub fn is_animating(&self) -> bool {
        self.progress < 1.0
    }

    /// The full palette of the cover on screen, for ambient backgrounds.
    pub fn palette(&self) -> Option<&CoverPalette> {
        self.palette.as_ref()
    }

    /// Drop everything remembered. Used when the library is rescanned, since
    /// cover ids can be reassigned under us.
    pub fn clear(&mut self) {
        *self = Self::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIGURED: Rgb = Rgb::new(0x7C, 0x5C, 0xFF);
    const RED: Rgb = Rgb::new(0xD0, 0x40, 0x40);
    const BLUE: Rgb = Rgb::new(0x40, 0x60, 0xD0);

    /// Drives the state directly, since building a `LibraryState` with real
    /// covers on disk would be testing the wrong thing.
    fn fade_to(state: &mut Adaptive, accent: Option<Rgb>) {
        state.from = state.current();
        state.to = accent;
        state.progress = 0.0;
    }

    #[test]
    fn with_no_cover_the_configured_accent_is_used() {
        let state = Adaptive::new();
        assert_eq!(state.accent(CONFIGURED), CONFIGURED);
        assert!(!state.is_animating());
    }

    #[test]
    fn the_accent_arrives_at_the_covers_colour() {
        let mut state = Adaptive::new();
        fade_to(&mut state, Some(RED));

        assert_eq!(
            state.accent(CONFIGURED),
            CONFIGURED,
            "it starts where it was"
        );

        // Well past the fade, so this pins the endpoint and not the curve.
        state.advance(FADE_SECONDS * 2.0);

        assert_eq!(state.accent(CONFIGURED), RED);
        assert!(!state.is_animating());
    }

    #[test]
    fn the_accent_is_somewhere_in_between_partway_through() {
        let mut state = Adaptive::new();
        fade_to(&mut state, Some(RED));
        state.advance(FADE_SECONDS / 2.0);

        let midpoint = state.accent(CONFIGURED);
        assert_ne!(midpoint, CONFIGURED);
        assert_ne!(midpoint, RED);
        assert!(state.is_animating());
    }

    /// A monochrome cover has to fade *back* to the configured accent, not sit
    /// on the previous track's colour forever.
    #[test]
    fn a_cover_with_no_colour_returns_to_the_configured_accent() {
        let mut state = Adaptive::new();

        fade_to(&mut state, Some(RED));
        state.advance(FADE_SECONDS * 2.0);
        assert_eq!(state.accent(CONFIGURED), RED);

        fade_to(&mut state, None);
        state.advance(FADE_SECONDS * 2.0);

        assert_eq!(state.accent(CONFIGURED), CONFIGURED);
    }

    /// Skipping through tracks must not make the colour lurch backwards to a
    /// target it never reached.
    #[test]
    fn a_fade_interrupted_midway_continues_from_where_it_was() {
        let mut state = Adaptive::new();

        fade_to(&mut state, Some(RED));
        state.advance(FADE_SECONDS * 2.0);

        fade_to(&mut state, Some(BLUE));
        state.advance(FADE_SECONDS * 0.4);
        let partway = state.accent(CONFIGURED);

        // Interrupted before it arrived.
        fade_to(&mut state, Some(RED));

        assert_eq!(
            state.accent(CONFIGURED),
            partway,
            "the new fade should begin from the colour on screen"
        );
    }

    #[test]
    fn advancing_a_settled_theme_does_nothing() {
        let mut state = Adaptive::new();
        fade_to(&mut state, Some(RED));

        assert!(state.advance(FADE_SECONDS * 2.0));
        assert!(!state.advance(1.0), "a finished fade is not still moving");
    }

    #[test]
    fn clearing_forgets_the_current_cover() {
        let mut state = Adaptive::new();
        fade_to(&mut state, Some(RED));
        state.advance(FADE_SECONDS * 2.0);
        state.showing = Some("abc".into());

        state.clear();

        assert_eq!(state.accent(CONFIGURED), CONFIGURED);
        assert!(state.showing.is_none());
        assert!(state.palette().is_none());
    }
}
