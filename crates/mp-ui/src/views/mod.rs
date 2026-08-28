//! Top-level content views selected from the nav rail.
//!
//! In M0 the library does not exist yet, so every list view renders an empty
//! state. Settings is fully wired, because the config layer it edits is real.

pub mod browse;
pub mod equalizer;
pub mod now_playing;
pub mod playlists;
pub mod settings;
pub mod songs;
pub mod tag_editor;
pub mod visualizer;
pub mod welcome;

use crate::widgets::icons::Icon;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum View {
    Home,
    Songs,
    Artists,
    Albums,
    Genres,
    Folders,
    Playlists,
    Equalizer,
    Visualizer,
    Settings,
}

impl View {
    /// Views in nav-rail order.
    pub const LIBRARY: [Self; 5] = [
        Self::Songs,
        Self::Artists,
        Self::Albums,
        Self::Genres,
        Self::Folders,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Songs => "Songs",
            Self::Artists => "Artists",
            Self::Albums => "Albums",
            Self::Genres => "Genres",
            Self::Folders => "Folders",
            Self::Playlists => "Playlists",
            Self::Equalizer => "Equalizer",
            Self::Visualizer => "Visualizer",
            Self::Settings => "Settings",
        }
    }

    pub fn icon(self) -> Icon {
        match self {
            Self::Home => Icon::Home,
            Self::Songs => Icon::Songs,
            Self::Artists => Icon::Artists,
            Self::Albums => Icon::Albums,
            Self::Genres => Icon::Genres,
            Self::Folders => Icon::Folders,
            Self::Playlists => Icon::Playlists,
            Self::Equalizer => Icon::Equalizer,
            Self::Visualizer => Icon::Visualizer,
            Self::Settings => Icon::Settings,
        }
    }

    /// Headline shown when the view has nothing to display.
    pub fn empty_title(self) -> &'static str {
        match self {
            Self::Home => "Nothing playing yet",
            Self::Songs => "No songs yet",
            Self::Artists => "No artists yet",
            Self::Albums => "No albums yet",
            Self::Genres => "No genres yet",
            Self::Folders => "No folders yet",
            Self::Playlists => "No playlists yet",
            Self::Equalizer => "Equalizer",
            Self::Visualizer => "Visualizer",
            Self::Settings => "Settings",
        }
    }

    /// Supporting line for the empty state - always tells the user what to do
    /// next rather than just stating the absence.
    pub fn empty_body(self) -> &'static str {
        match self {
            Self::Home => "Add a music folder in Settings to get started.",
            Self::Playlists => "Create one, or build a smart playlist from rules.",
            Self::Equalizer => "",
            Self::Visualizer => "",
            Self::Settings => "",
            _ => "Add a music folder in Settings, then scan your library.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_view_has_a_label_icon_and_empty_state() {
        let all = [
            View::Home,
            View::Songs,
            View::Artists,
            View::Albums,
            View::Genres,
            View::Folders,
            View::Playlists,
            View::Equalizer,
            View::Visualizer,
            View::Settings,
        ];

        for view in all {
            assert!(!view.label().is_empty(), "{view:?} needs a label");
            assert!(!view.empty_title().is_empty(), "{view:?} needs a title");
            // Icons are distinct per view so the rail is scannable.
            assert!(!view.icon().label().is_empty());
        }
    }

    #[test]
    fn library_views_point_the_user_at_the_next_step() {
        for view in View::LIBRARY {
            let body = view.empty_body();
            assert!(
                body.contains("Settings"),
                "{view:?} empty state should say where to go, got {body:?}"
            );
        }
    }

    #[test]
    fn library_views_have_distinct_icons() {
        let mut seen = std::collections::HashSet::new();
        for view in View::LIBRARY {
            assert!(seen.insert(view.icon()), "{view:?} reuses an icon");
        }
    }
}
