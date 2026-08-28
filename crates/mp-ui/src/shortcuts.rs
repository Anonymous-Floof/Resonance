//! Keyboard control for the whole app.
//!
//! The binding table is a pure function from a keypress to an [`Action`], with
//! no egui state involved, so every binding can be tested without a window.
//! What the app then *does* with an action lives in the shell, which is the
//! only place that knows whether there is a queue or a search box in play.
//!
//! ## Single letters, and why they are safe
//!
//! Media players conventionally bind bare letters — `m` for mute, `s` for
//! shuffle — and doing so is far more discoverable than burying everything
//! behind a modifier. That is only safe because the caller refuses to dispatch
//! anything while a text field has focus; see [`should_dispatch`], which is
//! where both focus rules live. Get that check wrong and typing "smart" into
//! the search box would shuffle the queue, mute the audio and open the queue
//! panel.

use egui::{Key, Modifiers};

/// How far the seek keys move, in seconds.
pub const SEEK_STEP: f64 = 5.0;

/// How much the volume keys move, as a fraction of full scale.
pub const VOLUME_STEP: f32 = 0.05;

/// Something the user asked for with the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    PlayPause,
    Next,
    Previous,
    SeekForward,
    SeekBack,
    VolumeUp,
    VolumeDown,
    ToggleMute,
    ToggleShuffle,
    CycleRepeat,
    ToggleQueue,
    ToggleFullScreen,
    FocusSearch,
    /// Back out of whatever is open: the search, a drill-down, full screen.
    Escape,
}

impl Action {
    /// Whether this action may only fire when no text field has focus.
    ///
    /// Escape and the modifier-carrying bindings are exempt: they are either
    /// meaningless as typed characters or already impossible to type by
    /// accident, and Escape in particular has to work *because* a field has
    /// focus — clearing the search is its whole job.
    pub fn needs_idle_keyboard(self) -> bool {
        !matches!(self, Self::Escape | Self::FocusSearch)
    }

    /// A human-readable name, for a shortcut list.
    pub fn label(self) -> &'static str {
        match self {
            Self::PlayPause => "Play or pause",
            Self::Next => "Next track",
            Self::Previous => "Previous track",
            Self::SeekForward => "Skip forward 5 seconds",
            Self::SeekBack => "Skip back 5 seconds",
            Self::VolumeUp => "Volume up",
            Self::VolumeDown => "Volume down",
            Self::ToggleMute => "Mute",
            Self::ToggleShuffle => "Shuffle",
            Self::CycleRepeat => "Repeat",
            Self::ToggleQueue => "Queue panel",
            Self::ToggleFullScreen => "Full screen",
            Self::FocusSearch => "Search",
            Self::Escape => "Back out / clear search",
        }
    }
}

/// Every binding, in the order a help list should show them.
///
/// The table is the single source of truth: [`action_for`] reads it, and so
/// does the shortcut list in Settings, so a binding cannot exist without being
/// documented or be documented without existing.
pub const BINDINGS: &[(Modifiers, Key, Action)] = &[
    (Modifiers::NONE, Key::Space, Action::PlayPause),
    (Modifiers::CTRL, Key::ArrowRight, Action::Next),
    (Modifiers::CTRL, Key::ArrowLeft, Action::Previous),
    (Modifiers::NONE, Key::ArrowRight, Action::SeekForward),
    (Modifiers::NONE, Key::ArrowLeft, Action::SeekBack),
    (Modifiers::NONE, Key::ArrowUp, Action::VolumeUp),
    (Modifiers::NONE, Key::ArrowDown, Action::VolumeDown),
    (Modifiers::NONE, Key::M, Action::ToggleMute),
    (Modifiers::NONE, Key::S, Action::ToggleShuffle),
    (Modifiers::NONE, Key::R, Action::CycleRepeat),
    (Modifiers::NONE, Key::Q, Action::ToggleQueue),
    (Modifiers::NONE, Key::F11, Action::ToggleFullScreen),
    (Modifiers::CTRL, Key::F, Action::FocusSearch),
    (Modifiers::NONE, Key::Escape, Action::Escape),
];

/// Keys egui itself uses to activate whatever currently has focus.
///
/// A binding on one of these fires twice while anything is focused: once from
/// the focused widget, and once from here. Pressing Space with the play button
/// focused would start and immediately stop the music.
const ACTIVATION_KEYS: &[Key] = &[Key::Space, Key::Enter];

/// Whether a keypress should be acted on, given what has focus.
///
/// Two different gates, because "a text box has focus" and "something has
/// focus" are different problems:
///
/// - With a **text box** focused, every bare-letter binding has to stand down
///   or typing into the search box shuffles the queue.
/// - With **any widget** focused, only the activation keys have to stand down,
///   because egui is already going to act on them.
pub fn should_dispatch(key: Key, action: Action, text_focused: bool, widget_focused: bool) -> bool {
    if text_focused && action.needs_idle_keyboard() {
        return false;
    }

    if widget_focused && ACTIVATION_KEYS.contains(&key) {
        return false;
    }

    true
}

/// The action a keypress means, if any.
pub fn action_for(key: Key, modifiers: Modifiers) -> Option<Action> {
    BINDINGS
        .iter()
        .find(|(wanted, bound, _)| *bound == key && matches(*wanted, modifiers))
        .map(|(_, _, action)| *action)
}

/// Whether the modifiers held match the ones a binding wants.
///
/// Compared exactly on Ctrl, Alt and Shift rather than loosely, so `Ctrl+Right`
/// cannot also trigger the bare `Right` binding and skip the track *and* seek
/// within it. Command is folded into Ctrl by egui on the platforms that have
/// one, so it needs no separate arm.
fn matches(wanted: Modifiers, held: Modifiers) -> bool {
    wanted.ctrl == held.ctrl && wanted.alt == held.alt && wanted.shift == held.shift
}

/// How a binding should be written in a help list, e.g. `Ctrl + →`.
pub fn describe(modifiers: Modifiers, key: Key) -> String {
    let name = match key {
        Key::Space => "Space",
        Key::ArrowLeft => "←",
        Key::ArrowRight => "→",
        Key::ArrowUp => "↑",
        Key::ArrowDown => "↓",
        Key::Escape => "Esc",
        other => other.name(),
    };

    let mut out = String::new();
    if modifiers.ctrl {
        out.push_str("Ctrl + ");
    }
    if modifiers.alt {
        out.push_str("Alt + ");
    }
    if modifiers.shift {
        out.push_str("Shift + ");
    }
    out.push_str(name);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_common_transport_keys_are_bound() {
        assert_eq!(
            action_for(Key::Space, Modifiers::NONE),
            Some(Action::PlayPause)
        );
        assert_eq!(
            action_for(Key::ArrowRight, Modifiers::NONE),
            Some(Action::SeekForward)
        );
        assert_eq!(
            action_for(Key::ArrowRight, Modifiers::CTRL),
            Some(Action::Next)
        );
    }

    /// The bug this guards: a loose modifier comparison would make `Ctrl+→`
    /// match the bare `→` binding as well, skipping the track and then seeking
    /// five seconds into the next one.
    #[test]
    fn a_modifier_binding_does_not_also_fire_the_bare_one() {
        assert_eq!(
            action_for(Key::ArrowRight, Modifiers::CTRL),
            Some(Action::Next)
        );
        assert_eq!(
            action_for(Key::ArrowRight, Modifiers::NONE),
            Some(Action::SeekForward)
        );

        // And a modifier nobody asked for matches nothing at all.
        assert_eq!(action_for(Key::ArrowRight, Modifiers::ALT), None);
        assert_eq!(action_for(Key::Space, Modifiers::CTRL), None);
    }

    #[test]
    fn an_unbound_key_is_ignored() {
        assert_eq!(action_for(Key::Z, Modifiers::NONE), None);
        assert_eq!(action_for(Key::F7, Modifiers::NONE), None);
    }

    /// The safety property the bare-letter bindings rest on.
    #[test]
    fn the_typing_hazards_are_all_gated_on_an_idle_keyboard() {
        for (modifiers, key, action) in BINDINGS {
            // Anything reachable by typing an ordinary character must be
            // gated, or it fires while the user is filling in a text field.
            let is_plain_letter = *modifiers == Modifiers::NONE && key.name().len() == 1;

            if is_plain_letter {
                assert!(
                    action.needs_idle_keyboard(),
                    "{action:?} is bound to a bare letter and would fire while typing"
                );
            }
        }
    }

    /// Escape has to work *while* a field has focus — clearing the search is
    /// the whole point of it.
    #[test]
    fn escape_and_search_still_work_with_a_field_focused() {
        assert!(!Action::Escape.needs_idle_keyboard());
        assert!(!Action::FocusSearch.needs_idle_keyboard());
    }

    /// Typing in the search box must not operate the player.
    #[test]
    fn a_focused_text_box_silences_the_bare_bindings() {
        for (key, action) in [
            (Key::M, Action::ToggleMute),
            (Key::S, Action::ToggleShuffle),
            (Key::Space, Action::PlayPause),
            (Key::ArrowLeft, Action::SeekBack),
        ] {
            assert!(
                !should_dispatch(key, action, true, true),
                "{action:?} fired while the user was typing"
            );
        }
    }

    /// But backing out and reaching the search box still have to work, since
    /// that is what someone with a focused text box actually wants.
    #[test]
    fn escape_and_search_survive_a_focused_text_box() {
        assert!(should_dispatch(Key::Escape, Action::Escape, true, true));
        assert!(should_dispatch(Key::F, Action::FocusSearch, true, true));
    }

    /// egui activates the focused widget on Space, so a Space binding would
    /// fire twice — starting and immediately stopping playback.
    #[test]
    fn space_stands_down_when_a_widget_has_focus() {
        assert!(
            !should_dispatch(Key::Space, Action::PlayPause, false, true),
            "Space would fire here and in the focused widget"
        );

        // With nothing focused it is ours.
        assert!(should_dispatch(Key::Space, Action::PlayPause, false, false));
    }

    /// A focused *button* is not a focused text box: the letter bindings are
    /// still fine, because a button will not swallow an `m`.
    #[test]
    fn a_focused_button_leaves_the_letter_bindings_alone() {
        assert!(should_dispatch(Key::M, Action::ToggleMute, false, true));
        assert!(should_dispatch(Key::S, Action::ToggleShuffle, false, true));
        assert!(should_dispatch(
            Key::ArrowRight,
            Action::SeekForward,
            false,
            true
        ));
    }

    #[test]
    fn with_nothing_focused_everything_dispatches() {
        for (modifiers, key, action) in BINDINGS {
            let _ = modifiers;
            assert!(
                should_dispatch(*key, *action, false, false),
                "{action:?} does not work even with nothing focused"
            );
        }
    }

    #[test]
    fn every_binding_is_unique() {
        let mut seen = std::collections::HashSet::new();

        for (modifiers, key, _) in BINDINGS {
            let signature = (modifiers.ctrl, modifiers.alt, modifiers.shift, *key);
            assert!(
                seen.insert(signature),
                "{} is bound twice",
                describe(*modifiers, *key)
            );
        }
    }

    /// A help list is only useful if every entry reads as a key.
    #[test]
    fn every_binding_describes_itself() {
        for (modifiers, key, action) in BINDINGS {
            let text = describe(*modifiers, *key);

            assert!(!text.is_empty(), "{action:?} has no printable binding");
            assert!(!action.label().is_empty(), "{action:?} has no label");
            assert!(
                !text.ends_with(' ') && !text.ends_with('+'),
                "{text:?} looks unfinished"
            );
        }
    }

    #[test]
    fn modifiers_are_spelled_out_in_the_description() {
        assert_eq!(describe(Modifiers::NONE, Key::Space), "Space");
        assert_eq!(describe(Modifiers::CTRL, Key::ArrowRight), "Ctrl + →");
        assert_eq!(describe(Modifiers::NONE, Key::Escape), "Esc");
        assert_eq!(describe(Modifiers::CTRL, Key::F), "Ctrl + F");
    }

    /// Every action the app can be asked to perform should be reachable.
    #[test]
    fn every_action_has_a_binding() {
        let all = [
            Action::PlayPause,
            Action::Next,
            Action::Previous,
            Action::SeekForward,
            Action::SeekBack,
            Action::VolumeUp,
            Action::VolumeDown,
            Action::ToggleMute,
            Action::ToggleShuffle,
            Action::CycleRepeat,
            Action::ToggleQueue,
            Action::ToggleFullScreen,
            Action::FocusSearch,
            Action::Escape,
        ];

        for action in all {
            assert!(
                BINDINGS.iter().any(|(_, _, bound)| *bound == action),
                "{action:?} exists but no key reaches it"
            );
        }
    }
}
