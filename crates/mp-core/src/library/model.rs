//! The shapes the library hands back to the UI.
//!
//! These are display-oriented: they carry the strings a row needs already
//! resolved, so drawing a list never has to touch the database. The database
//! remains the source of truth; these are snapshots of a query.

use std::path::PathBuf;
use std::time::Duration;

/// Row id of a track. Stable for as long as the file stays in the library.
pub type TrackId = i64;
pub type ArtistId = i64;
pub type AlbumId = i64;
pub type GenreId = i64;

/// Shown wherever a track has no usable artist tag and none could be parsed
/// out of its filename.
pub const UNKNOWN_ARTIST: &str = "Unknown Artist";

/// Shown for loose files that belong to no album.
pub const UNKNOWN_ALBUM: &str = "Singles & Loose Tracks";

/// One track, as a list row needs it.
#[derive(Debug, Clone)]
pub struct Track {
    pub id: TrackId,
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_id: Option<AlbumId>,
    pub artist_id: Option<ArtistId>,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub year: Option<i32>,
    pub duration: Option<Duration>,
    /// Content hash of this track's cover, if one was found.
    pub art_id: Option<String>,
    /// False when the title was recovered from the filename rather than a tag.
    pub tagged: bool,
    pub play_count: u32,
}

impl Track {
    /// Second line of a track row: artist, plus album when it adds something.
    pub fn subtitle(&self) -> String {
        if self.album == UNKNOWN_ALBUM || self.album.is_empty() {
            self.artist.clone()
        } else {
            format!("{} — {}", self.artist, self.album)
        }
    }
}

/// An artist with enough counts to render a card without a second query.
#[derive(Debug, Clone)]
pub struct Artist {
    pub id: ArtistId,
    pub name: String,
    pub track_count: u32,
    pub album_count: u32,
    /// Cover of one of this artist's albums, used as the card image.
    pub art_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Album {
    pub id: AlbumId,
    pub title: String,
    pub artist: String,
    pub artist_id: Option<ArtistId>,
    pub year: Option<i32>,
    pub track_count: u32,
    pub total_duration: Duration,
    pub art_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Genre {
    pub id: GenreId,
    pub name: String,
    pub track_count: u32,
    pub art_id: Option<String>,
}

/// A directory containing tracks, for the Folders view.
#[derive(Debug, Clone)]
pub struct Folder {
    pub path: PathBuf,
    /// Just the folder's own name, for display.
    pub name: String,
    /// Tracks directly inside this folder (not counting subfolders).
    pub track_count: u32,
    pub total_duration: Duration,
}

/// Totals for the header of the library views.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub tracks: u32,
    pub artists: u32,
    pub albums: u32,
    pub genres: u32,
    pub folders: u32,
    /// Files found but not decodable by this build.
    pub unplayable: u32,
    pub total_duration: Duration,
    /// Tracks whose metadata came from the filename rather than real tags.
    pub untagged: u32,
}

/// Which list a query should produce and in what order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    Title,
    Artist,
    Album,
    Year,
    Duration,
    DateAdded,
    PlayCount,
    LastPlayed,
    /// Album order: disc, then track number. Only meaningful within an album.
    TrackNumber,
}

impl Order {
    /// Every ordering, in the order a picker should offer them.
    pub const ALL: [Self; 9] = [
        Self::Title,
        Self::Artist,
        Self::Album,
        Self::Year,
        Self::Duration,
        Self::DateAdded,
        Self::PlayCount,
        Self::LastPlayed,
        Self::TrackNumber,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Title => "Title",
            Self::Artist => "Artist",
            Self::Album => "Album",
            Self::Year => "Year",
            Self::Duration => "Duration",
            Self::DateAdded => "Date added",
            Self::PlayCount => "Play count",
            Self::LastPlayed => "Last played",
            Self::TrackNumber => "Track number",
        }
    }

    /// A short name, for a picker that has to fit in a toolbar.
    pub fn short_label(self) -> &'static str {
        match self {
            Self::Title => "Title",
            Self::Artist => "Artist",
            Self::Album => "Album",
            Self::Year => "Year",
            Self::Duration => "Length",
            Self::DateAdded => "Added",
            Self::PlayCount => "Plays",
            Self::LastPlayed => "Last played",
            Self::TrackNumber => "Track",
        }
    }

    /// What this direction actually means for this key.
    ///
    /// "Ascending" is only obvious for text. Ascending by play count is
    /// *fewest* plays first, and ascending by last played is *longest ago*
    /// first — both of which people reliably guess backwards. Naming the
    /// effect rather than the direction removes the guess.
    pub fn direction_label(self, descending: bool) -> &'static str {
        match (self, descending) {
            (Self::Title | Self::Artist | Self::Album, false) => "A to Z",
            (Self::Title | Self::Artist | Self::Album, true) => "Z to A",

            (Self::Year | Self::DateAdded, false) => "Oldest first",
            (Self::Year | Self::DateAdded, true) => "Newest first",

            (Self::Duration, false) => "Shortest first",
            (Self::Duration, true) => "Longest first",

            (Self::PlayCount, false) => "Least played first",
            (Self::PlayCount, true) => "Most played first",

            (Self::LastPlayed, false) => "Longest ago first",
            (Self::LastPlayed, true) => "Most recent first",

            (Self::TrackNumber, false) => "First to last",
            (Self::TrackNumber, true) => "Last to first",
        }
    }

    /// The `ORDER BY` fragment, always ending in a total order so paging is
    /// stable and rows never swap places between two identical keys.
    pub(crate) fn sql(self, descending: bool) -> String {
        let direction = if descending { "DESC" } else { "ASC" };
        let primary = match self {
            Self::Title => "t.sort_title",
            Self::Artist => "ar.sort_name, al.year, al.sort_title, t.disc_no, t.track_no",
            Self::Album => "al.sort_title, t.disc_no, t.track_no",
            Self::Year => "t.year",
            Self::Duration => "t.duration_ms",
            Self::DateAdded => "t.added_at",
            Self::PlayCount => "t.play_count",
            Self::LastPlayed => "t.last_played_at",
            Self::TrackNumber => "t.disc_no, t.track_no",
        };
        // NULLs last in either direction: a track with no year should not head
        // the list just because its tag is missing.
        format!("{primary} {direction} NULLS LAST, t.sort_title ASC, t.id ASC")
    }
}

impl From<crate::config::SortKey> for Order {
    fn from(key: crate::config::SortKey) -> Self {
        use crate::config::SortKey;
        match key {
            SortKey::Title => Self::Title,
            SortKey::Artist => Self::Artist,
            SortKey::Album => Self::Album,
            SortKey::Year => Self::Year,
            SortKey::Duration => Self::Duration,
            SortKey::DateAdded => Self::DateAdded,
            SortKey::PlayCount => Self::PlayCount,
            SortKey::LastPlayed => Self::LastPlayed,
        }
    }
}

impl Order {
    /// The settings value this ordering corresponds to.
    ///
    /// `None` for [`Order::TrackNumber`], which is chosen by the album view
    /// itself and is not something the user can pick as a default — a list of
    /// every song sorted by track number would be nonsense.
    pub fn as_sort_key(self) -> Option<crate::config::SortKey> {
        use crate::config::SortKey;
        Some(match self {
            Self::Title => SortKey::Title,
            Self::Artist => SortKey::Artist,
            Self::Album => SortKey::Album,
            Self::Year => SortKey::Year,
            Self::Duration => SortKey::Duration,
            Self::DateAdded => SortKey::DateAdded,
            Self::PlayCount => SortKey::PlayCount,
            Self::LastPlayed => SortKey::LastPlayed,
            Self::TrackNumber => return None,
        })
    }
}

/// Narrows a track query to one group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    Artist(ArtistId),
    Album(AlbumId),
    Genre(GenreId),
    /// Tracks directly inside this folder.
    Folder(PathBuf),
    /// Free-text search across title, artist and album.
    Search(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two conversions have to agree, or a chosen sort is remembered as a
    /// different one and comes back changed after a restart.
    #[test]
    fn sort_keys_round_trip_through_order() {
        for order in Order::ALL {
            let Some(key) = order.as_sort_key() else {
                assert_eq!(order, Order::TrackNumber, "only track order has no key");
                continue;
            };

            assert_eq!(Order::from(key), order, "{order:?} did not round-trip");
        }
    }

    /// Every ordering needs both names, or the picker shows a blank entry.
    #[test]
    fn every_order_has_a_short_label_and_both_directions() {
        let mut seen = std::collections::HashSet::new();

        for order in Order::ALL {
            assert!(
                !order.short_label().is_empty(),
                "{order:?} has no short label"
            );
            assert!(
                seen.insert(order.short_label()),
                "{order:?} reuses a short label"
            );

            let up = order.direction_label(false);
            let down = order.direction_label(true);

            assert!(
                !up.is_empty() && !down.is_empty(),
                "{order:?} is missing a direction"
            );
            assert_ne!(up, down, "{order:?} describes both directions the same way");
        }
    }

    /// The wording has to match what the sort actually does, or it is worse
    /// than no wording at all.
    #[test]
    fn direction_wording_matches_the_sort() {
        // Ascending play count is fewest first, which people guess backwards.
        assert_eq!(
            Order::PlayCount.direction_label(false),
            "Least played first"
        );
        assert_eq!(Order::PlayCount.direction_label(true), "Most played first");

        // And ascending "last played" is the least recent.
        assert_eq!(
            Order::LastPlayed.direction_label(false),
            "Longest ago first"
        );
        assert_eq!(Order::LastPlayed.direction_label(true), "Most recent first");

        assert_eq!(Order::Duration.direction_label(false), "Shortest first");
    }

    #[test]
    fn a_track_with_no_album_shows_only_its_artist() {
        let track = Track {
            id: 1,
            path: PathBuf::from("a.mp3"),
            title: "Title".into(),
            artist: "Artist".into(),
            album: UNKNOWN_ALBUM.into(),
            album_id: None,
            artist_id: None,
            track_no: None,
            disc_no: None,
            year: None,
            duration: None,
            art_id: None,
            tagged: false,
            play_count: 0,
        };
        assert_eq!(track.subtitle(), "Artist");
    }

    /// Paging is only stable if every ordering ends in a unique column.
    #[test]
    fn every_order_ends_with_a_tiebreaker() {
        let orders = [
            Order::Title,
            Order::Artist,
            Order::Album,
            Order::Year,
            Order::Duration,
            Order::DateAdded,
            Order::PlayCount,
            Order::LastPlayed,
            Order::TrackNumber,
        ];
        for order in orders {
            for descending in [false, true] {
                assert!(
                    order.sql(descending).ends_with("t.id ASC"),
                    "{order:?} must break ties on the primary key"
                );
            }
        }
    }
}
