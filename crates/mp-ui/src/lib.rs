//! The Resonance interface: theme, shell, views and visualizers.

pub mod adaptive;
pub mod analysis_job;
pub mod app;
pub mod artwork;
pub mod fonts;
pub mod immersive;
pub mod library;
pub mod platform;
pub mod player;
pub mod playlists;
pub mod shortcuts;
pub mod surface;
pub mod tag_editor;
pub mod theme;
pub mod views;
pub mod visualizer;
pub mod widgets;
pub mod window_frame;

pub use app::{MIN_WINDOW_SIZE, ResonanceApp};
pub use player::Player;
pub use theme::{Metrics, Palette, Theme};
pub use views::View;
