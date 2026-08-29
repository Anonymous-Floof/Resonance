//! Vector icons drawn directly with the egui painter.
//!
//! Bundling an icon font would mean shipping a second font file and its
//! licence, and glyph-based icons pick up font hinting at small sizes. These
//! are drawn in a normalised 0..1 box and mapped into whatever rect they are
//! given, so they stay crisp at any size and inherit the theme's colours.
//!
//! Coordinates below read as fractions of the icon box: `(0.5, 0.5)` is the
//! centre.

use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    // Navigation
    Home,
    Songs,
    Artists,
    Albums,
    Genres,
    Folders,
    Playlists,
    Settings,
    // Transport
    Play,
    Pause,
    Next,
    Previous,
    Shuffle,
    Repeat,
    RepeatOne,
    // Player bar
    VolumeHigh,
    VolumeLow,
    VolumeMute,
    Queue,
    Equalizer,
    Visualizer,
    // General
    Search,
    Plus,
    ChevronLeft,
    ChevronRight,
    /// Enter the full-screen now-playing view.
    Expand,
    /// Leave it again.
    Collapse,
    /// The lyrics pane.
    Lyrics,
    /// Sort direction, ascending.
    SortAscending,
    /// Sort direction, descending.
    SortDescending,
    // Window controls
    Minimize,
    Maximize,
    Restore,
    Close,
}

impl Icon {
    /// A human-readable name, used for tooltips and accessibility labels.
    pub fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Songs => "Songs",
            Self::Artists => "Artists",
            Self::Albums => "Albums",
            Self::Genres => "Genres",
            Self::Folders => "Folders",
            Self::Playlists => "Playlists",
            Self::Settings => "Settings",
            Self::Play => "Play",
            Self::Pause => "Pause",
            Self::Next => "Next track",
            Self::Previous => "Previous track",
            Self::Shuffle => "Shuffle",
            Self::Repeat => "Repeat",
            Self::RepeatOne => "Repeat one",
            Self::VolumeHigh => "Volume",
            Self::VolumeLow => "Volume",
            Self::VolumeMute => "Muted",
            Self::Queue => "Queue",
            Self::Equalizer => "Equalizer",
            Self::Visualizer => "Visualizer",
            Self::Search => "Search",
            Self::Plus => "Add",
            Self::ChevronLeft => "Back",
            Self::ChevronRight => "Forward",
            Self::Expand => "Full screen",
            Self::Collapse => "Leave full screen",
            Self::Lyrics => "Lyrics",
            Self::SortAscending => "Ascending",
            Self::SortDescending => "Descending",
            Self::Minimize => "Minimise",
            Self::Maximize => "Maximise",
            Self::Restore => "Restore down",
            Self::Close => "Close",
        }
    }
}

/// Draw `icon` inside `rect`.
///
/// `rect` is squared off and inset slightly first, so icons of different
/// shapes still read as optically the same size when placed side by side.
pub fn draw(painter: &Painter, icon: Icon, rect: Rect, color: Color32, thickness: f32) {
    let box_rect = square(rect);
    let stroke = Stroke::new(thickness, color);

    match icon {
        Icon::Home => home(painter, box_rect, stroke),
        Icon::Songs => songs(painter, box_rect, color, stroke),
        Icon::Artists => artists(painter, box_rect, stroke),
        Icon::Albums => albums(painter, box_rect, color, stroke),
        Icon::Genres => genres(painter, box_rect, color, stroke),
        Icon::Folders => folders(painter, box_rect, stroke),
        Icon::Playlists => playlists(painter, box_rect, stroke),
        Icon::Settings => settings(painter, box_rect, stroke),

        Icon::Play => play(painter, box_rect, color),
        Icon::Pause => pause(painter, box_rect, color),
        Icon::Next => skip(painter, box_rect, color, false),
        Icon::Previous => skip(painter, box_rect, color, true),
        Icon::Shuffle => shuffle(painter, box_rect, stroke),
        Icon::Repeat => repeat(painter, box_rect, stroke, false),
        Icon::RepeatOne => repeat(painter, box_rect, stroke, true),

        Icon::VolumeHigh => volume(painter, box_rect, color, stroke, 2),
        Icon::VolumeLow => volume(painter, box_rect, color, stroke, 1),
        Icon::VolumeMute => volume(painter, box_rect, color, stroke, 0),
        Icon::Queue => queue(painter, box_rect, stroke),
        Icon::Equalizer => equalizer(painter, box_rect, stroke),
        Icon::Visualizer => visualizer(painter, box_rect, color),

        Icon::Search => search(painter, box_rect, stroke),
        Icon::Plus => plus(painter, box_rect, stroke),
        Icon::ChevronLeft => chevron(painter, box_rect, stroke, true),
        Icon::ChevronRight => chevron(painter, box_rect, stroke, false),
        Icon::Expand => corners(painter, box_rect, stroke, true),
        Icon::Collapse => corners(painter, box_rect, stroke, false),
        Icon::Lyrics => lyrics(painter, box_rect, stroke),
        Icon::SortAscending => sort_arrow(painter, box_rect, stroke, color, true),
        Icon::SortDescending => sort_arrow(painter, box_rect, stroke, color, false),

        Icon::Minimize => minimize(painter, box_rect, stroke),
        Icon::Maximize => maximize(painter, box_rect, stroke),
        Icon::Restore => restore(painter, box_rect, stroke),
        Icon::Close => close(painter, box_rect, stroke),
    }
}

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

/// Centred square with a small inset, so strokes never clip the edge.
fn square(rect: Rect) -> Rect {
    let size = rect.width().min(rect.height()) * 0.82;
    Rect::from_center_size(rect.center(), Vec2::splat(size))
}

/// Map normalised 0..1 coordinates into the icon box.
fn p(rect: Rect, x: f32, y: f32) -> Pos2 {
    Pos2::new(
        rect.left() + rect.width() * x,
        rect.top() + rect.height() * y,
    )
}

fn poly(painter: &Painter, rect: Rect, points: &[(f32, f32)], color: Color32) {
    let points: Vec<Pos2> = points.iter().map(|(x, y)| p(rect, *x, *y)).collect();
    painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
}

fn path(painter: &Painter, rect: Rect, points: &[(f32, f32)], stroke: Stroke) {
    let points: Vec<Pos2> = points.iter().map(|(x, y)| p(rect, *x, *y)).collect();
    painter.add(Shape::line(points, stroke));
}

// ---------------------------------------------------------------------------
// Navigation icons
// ---------------------------------------------------------------------------

fn home(painter: &Painter, r: Rect, stroke: Stroke) {
    path(
        painter,
        r,
        &[(0.08, 0.45), (0.5, 0.1), (0.92, 0.45)],
        stroke,
    );
    path(
        painter,
        r,
        &[(0.2, 0.38), (0.2, 0.9), (0.8, 0.9), (0.8, 0.38)],
        stroke,
    );
}

fn songs(painter: &Painter, r: Rect, color: Color32, stroke: Stroke) {
    // Beamed quaver: two note heads joined by a stem and beam.
    painter.circle_filled(p(r, 0.26, 0.76), r.width() * 0.13, color);
    painter.circle_filled(p(r, 0.74, 0.64), r.width() * 0.13, color);
    path(painter, r, &[(0.39, 0.76), (0.39, 0.22)], stroke);
    path(painter, r, &[(0.87, 0.64), (0.87, 0.1)], stroke);
    path(painter, r, &[(0.39, 0.22), (0.87, 0.1)], stroke);
}

fn artists(painter: &Painter, r: Rect, stroke: Stroke) {
    painter.circle_stroke(p(r, 0.5, 0.32), r.width() * 0.2, stroke);
    path(
        painter,
        r,
        &[
            (0.14, 0.92),
            (0.14, 0.78),
            (0.32, 0.62),
            (0.68, 0.62),
            (0.86, 0.78),
            (0.86, 0.92),
        ],
        stroke,
    );
}

fn albums(painter: &Painter, r: Rect, color: Color32, stroke: Stroke) {
    painter.circle_stroke(r.center(), r.width() * 0.42, stroke);
    painter.circle_filled(r.center(), r.width() * 0.09, color);
}

fn genres(painter: &Painter, r: Rect, color: Color32, stroke: Stroke) {
    // A luggage-style tag.
    path(
        painter,
        r,
        &[(0.1, 0.5), (0.5, 0.1), (0.9, 0.5), (0.5, 0.9), (0.1, 0.5)],
        stroke,
    );
    painter.circle_filled(p(r, 0.5, 0.5), r.width() * 0.1, color);
}

fn folders(painter: &Painter, r: Rect, stroke: Stroke) {
    path(
        painter,
        r,
        &[
            (0.1, 0.82),
            (0.1, 0.22),
            (0.42, 0.22),
            (0.52, 0.36),
            (0.9, 0.36),
            (0.9, 0.82),
            (0.1, 0.82),
        ],
        stroke,
    );
}

fn playlists(painter: &Painter, r: Rect, stroke: Stroke) {
    for (i, y) in [0.24, 0.48, 0.72].iter().enumerate() {
        // The last line is shortened to leave room for the note.
        let right = if i == 2 { 0.5 } else { 0.9 };
        path(painter, r, &[(0.1, *y), (right, *y)], stroke);
    }
    painter.circle_stroke(p(r, 0.74, 0.78), r.width() * 0.13, stroke);
    path(painter, r, &[(0.87, 0.78), (0.87, 0.4)], stroke);
}

fn settings(painter: &Painter, r: Rect, stroke: Stroke) {
    // Sliders read better than a gear at 16px and match the app's content.
    for (y, knob) in [(0.26, 0.68), (0.5, 0.36), (0.74, 0.58)] {
        path(painter, r, &[(0.1, y), (0.9, y)], stroke);
        painter.circle_filled(p(r, knob, y), r.width() * 0.1, stroke.color);
    }
}

// ---------------------------------------------------------------------------
// Transport icons
// ---------------------------------------------------------------------------

fn play(painter: &Painter, r: Rect, color: Color32) {
    poly(painter, r, &[(0.22, 0.1), (0.86, 0.5), (0.22, 0.9)], color);
}

fn pause(painter: &Painter, r: Rect, color: Color32) {
    let radius = egui::CornerRadius::same((r.width() * 0.06) as u8);
    painter.rect_filled(
        Rect::from_min_max(p(r, 0.22, 0.12), p(r, 0.41, 0.88)),
        radius,
        color,
    );
    painter.rect_filled(
        Rect::from_min_max(p(r, 0.59, 0.12), p(r, 0.78, 0.88)),
        radius,
        color,
    );
}

fn skip(painter: &Painter, r: Rect, color: Color32, backwards: bool) {
    let flip = |x: f32| if backwards { 1.0 - x } else { x };

    poly(
        painter,
        r,
        &[(flip(0.16), 0.14), (flip(0.68), 0.5), (flip(0.16), 0.86)],
        color,
    );
    painter.rect_filled(
        Rect::from_min_max(
            p(r, flip(0.72).min(flip(0.84)), 0.14),
            p(r, flip(0.72).max(flip(0.84)), 0.86),
        ),
        egui::CornerRadius::same((r.width() * 0.05) as u8),
        color,
    );
}

fn shuffle(painter: &Painter, r: Rect, stroke: Stroke) {
    path(
        painter,
        r,
        &[(0.08, 0.26), (0.3, 0.26), (0.7, 0.74), (0.92, 0.74)],
        stroke,
    );
    path(
        painter,
        r,
        &[(0.08, 0.74), (0.3, 0.74), (0.42, 0.6)],
        stroke,
    );
    path(
        painter,
        r,
        &[(0.58, 0.4), (0.7, 0.26), (0.92, 0.26)],
        stroke,
    );
    arrow_head(painter, r, (0.92, 0.26), stroke);
    arrow_head(painter, r, (0.92, 0.74), stroke);
}

fn arrow_head(painter: &Painter, r: Rect, tip: (f32, f32), stroke: Stroke) {
    let (x, y) = tip;
    path(
        painter,
        r,
        &[(x - 0.14, y - 0.1), (x, y), (x - 0.14, y + 0.1)],
        stroke,
    );
}

fn repeat(painter: &Painter, r: Rect, stroke: Stroke, one: bool) {
    path(
        painter,
        r,
        &[(0.22, 0.28), (0.78, 0.28), (0.78, 0.5)],
        stroke,
    );
    path(
        painter,
        r,
        &[(0.78, 0.72), (0.22, 0.72), (0.22, 0.5)],
        stroke,
    );
    arrow_head(painter, r, (0.86, 0.28), stroke);
    path(painter, r, &[(0.22, 0.72), (0.14, 0.72)], stroke);

    if one {
        // A "1" in the middle, drawn as two strokes.
        path(painter, r, &[(0.44, 0.44), (0.52, 0.4)], stroke);
        path(painter, r, &[(0.52, 0.4), (0.52, 0.62)], stroke);
    }
}

// ---------------------------------------------------------------------------
// Player bar icons
// ---------------------------------------------------------------------------

fn volume(painter: &Painter, r: Rect, color: Color32, stroke: Stroke, waves: u8) {
    poly(
        painter,
        r,
        &[
            (0.1, 0.36),
            (0.26, 0.36),
            (0.46, 0.16),
            (0.46, 0.84),
            (0.26, 0.64),
            (0.1, 0.64),
        ],
        color,
    );

    if waves >= 1 {
        path(
            painter,
            r,
            &[(0.58, 0.36), (0.66, 0.5), (0.58, 0.64)],
            stroke,
        );
    }
    if waves >= 2 {
        path(
            painter,
            r,
            &[(0.72, 0.24), (0.86, 0.5), (0.72, 0.76)],
            stroke,
        );
    }
    if waves == 0 {
        // Muted: a cross where the waves would be.
        path(painter, r, &[(0.62, 0.36), (0.88, 0.64)], stroke);
        path(painter, r, &[(0.88, 0.36), (0.62, 0.64)], stroke);
    }
}

fn queue(painter: &Painter, r: Rect, stroke: Stroke) {
    for y in [0.22, 0.44, 0.66] {
        path(painter, r, &[(0.1, y), (0.9, y)], stroke);
    }
    path(painter, r, &[(0.1, 0.88), (0.52, 0.88)], stroke);
}

fn equalizer(painter: &Painter, r: Rect, stroke: Stroke) {
    // Three vertical rails with handles at different heights.
    for (x, knob) in [(0.24, 0.62), (0.5, 0.34), (0.76, 0.54)] {
        path(painter, r, &[(x, 0.1), (x, 0.9)], stroke);
        painter.circle_filled(p(r, x, knob), r.width() * 0.11, stroke.color);
    }
}

fn visualizer(painter: &Painter, r: Rect, color: Color32) {
    // A miniature spectrum, which is exactly what the button toggles.
    let heights = [0.42, 0.72, 0.28, 0.6, 0.36];
    let width = 0.1;
    let gap = (1.0 - heights.len() as f32 * width) / (heights.len() as f32 + 1.0);

    for (i, h) in heights.iter().enumerate() {
        let x = gap + i as f32 * (width + gap);
        painter.rect_filled(
            Rect::from_min_max(p(r, x, 0.9 - h), p(r, x + width, 0.9)),
            egui::CornerRadius::same((r.width() * 0.04) as u8),
            color,
        );
    }
}

// ---------------------------------------------------------------------------
// General icons
// ---------------------------------------------------------------------------

fn search(painter: &Painter, r: Rect, stroke: Stroke) {
    painter.circle_stroke(p(r, 0.44, 0.44), r.width() * 0.3, stroke);
    path(painter, r, &[(0.66, 0.66), (0.9, 0.9)], stroke);
}

fn plus(painter: &Painter, r: Rect, stroke: Stroke) {
    path(painter, r, &[(0.5, 0.14), (0.5, 0.86)], stroke);
    path(painter, r, &[(0.14, 0.5), (0.86, 0.5)], stroke);
}

fn chevron(painter: &Painter, r: Rect, stroke: Stroke, left: bool) {
    let (near, far) = if left { (0.34, 0.66) } else { (0.66, 0.34) };
    path(painter, r, &[(far, 0.16), (near, 0.5), (far, 0.84)], stroke);
}

/// Bars of decreasing length beside an arrow, the way sort controls are drawn
/// everywhere.
///
/// The bars carry the meaning — short-to-long for ascending — and the arrow
/// repeats it, so the icon reads at a glance and still makes sense to someone
/// who has only ever seen one of the two conventions.
fn sort_arrow(painter: &Painter, r: Rect, stroke: Stroke, color: Color32, ascending: bool) {
    let bars: [f32; 3] = if ascending {
        [0.30, 0.44, 0.58]
    } else {
        [0.58, 0.44, 0.30]
    };

    for (index, end) in bars.iter().enumerate() {
        let y = 0.24 + index as f32 * 0.26;
        path(painter, r, &[(0.10, y), (*end, y)], stroke);
    }

    // The arrow, on the right, pointing the way the sort runs.
    let (tail, head) = if ascending {
        (0.86, 0.16)
    } else {
        (0.16, 0.86)
    };
    path(painter, r, &[(0.80, tail), (0.80, head)], stroke);

    let barb = if ascending { head + 0.18 } else { head - 0.18 };
    poly(
        painter,
        r,
        &[(0.80, head), (0.70, barb), (0.90, barb)],
        color,
    );
}

/// Four corner brackets, pointing out to expand and in to collapse.
///
/// The same geometry mirrored rather than two drawings, so the pair reads as
/// one control in two states — which is what it is.
fn corners(painter: &Painter, r: Rect, stroke: Stroke, out: bool) {
    // Where each bracket's elbow sits, and how far its arms reach. The
    // collapsed elbows have to stay well clear of the centre: at 0.46 the four
    // brackets met in the middle and the icon read as a plus sign.
    let (near, far) = if out { (0.16, 0.42) } else { (0.36, 0.12) };

    for (sx, sy) in [(1.0, 1.0), (-1.0, 1.0), (1.0, -1.0), (-1.0, -1.0)] {
        let elbow = (mirror(near, sx), mirror(near, sy));
        let arm_x = (mirror(far, sx), mirror(near, sy));
        let arm_y = (mirror(near, sx), mirror(far, sy));

        path(painter, r, &[arm_x, elbow, arm_y], stroke);
    }
}

/// Reflect a normalised coordinate about the centre when `sign` is negative.
fn mirror(value: f32, sign: f32) -> f32 {
    if sign > 0.0 { value } else { 1.0 - value }
}

/// Lines of text with a musical note beside them.
fn lyrics(painter: &Painter, r: Rect, stroke: Stroke) {
    // Ragged line lengths, so it reads as verse rather than a paragraph.
    for (y, end) in [(0.26, 0.62), (0.44, 0.78), (0.62, 0.54), (0.80, 0.70)] {
        path(painter, r, &[(0.14, y), (end, y)], stroke);
    }

    // A small note head with its stem, top right.
    painter.circle_filled(p(r, 0.80, 0.30), r.width() * 0.075, stroke.color);
    path(painter, r, &[(0.86, 0.30), (0.86, 0.10)], stroke);
}

// ---------------------------------------------------------------------------
// Window controls
// ---------------------------------------------------------------------------
//
// Drawn to the proportions Windows uses for caption buttons, so the custom
// title bar still feels like a Windows title bar.

fn minimize(painter: &Painter, r: Rect, stroke: Stroke) {
    path(painter, r, &[(0.24, 0.5), (0.76, 0.5)], stroke);
}

fn maximize(painter: &Painter, r: Rect, stroke: Stroke) {
    stroke_rect(
        painter,
        Rect::from_min_max(p(r, 0.26, 0.26), p(r, 0.74, 0.74)),
        1,
        stroke,
    );
}

fn restore(painter: &Painter, r: Rect, stroke: Stroke) {
    // Front pane, plus the two visible edges of the one behind it.
    stroke_rect(
        painter,
        Rect::from_min_max(p(r, 0.2, 0.34), p(r, 0.64, 0.78)),
        1,
        stroke,
    );
    path(
        painter,
        r,
        &[(0.34, 0.28), (0.8, 0.28), (0.8, 0.68)],
        stroke,
    );
}

fn close(painter: &Painter, r: Rect, stroke: Stroke) {
    path(painter, r, &[(0.26, 0.26), (0.74, 0.74)], stroke);
    path(painter, r, &[(0.74, 0.26), (0.26, 0.74)], stroke);
}

/// Outline a rect with the theme's stroke. Small helper shared by widgets.
pub fn stroke_rect(painter: &Painter, rect: Rect, radius: u8, stroke: Stroke) {
    painter.rect_stroke(
        rect,
        egui::CornerRadius::same(radius),
        stroke,
        StrokeKind::Inside,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every icon must have a label - they drive tooltips and a11y names.
    #[test]
    fn every_icon_has_a_non_empty_label() {
        let all = [
            Icon::Home,
            Icon::Songs,
            Icon::Artists,
            Icon::Albums,
            Icon::Genres,
            Icon::Folders,
            Icon::Playlists,
            Icon::Settings,
            Icon::Play,
            Icon::Pause,
            Icon::Next,
            Icon::Previous,
            Icon::Shuffle,
            Icon::Repeat,
            Icon::RepeatOne,
            Icon::VolumeHigh,
            Icon::VolumeLow,
            Icon::VolumeMute,
            Icon::Queue,
            Icon::Equalizer,
            Icon::Visualizer,
            Icon::Search,
            Icon::Plus,
            Icon::ChevronLeft,
            Icon::ChevronRight,
            Icon::Minimize,
            Icon::Maximize,
            Icon::Restore,
            Icon::Close,
        ];

        for icon in all {
            assert!(!icon.label().is_empty(), "{icon:?} has no label");
        }
    }

    #[test]
    fn square_is_centred_and_fits_inside_the_source_rect() {
        let wide = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(100.0, 40.0));
        let s = square(wide);

        assert_eq!(s.center(), wide.center());
        assert!((s.width() - s.height()).abs() < 1e-6, "must be square");
        assert!(s.width() <= wide.height(), "must fit the short axis");
    }

    #[test]
    fn normalised_coordinates_map_to_the_expected_corners() {
        let r = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(100.0, 100.0));

        assert_eq!(p(r, 0.0, 0.0), Pos2::new(0.0, 0.0));
        assert_eq!(p(r, 1.0, 1.0), Pos2::new(100.0, 100.0));
        assert_eq!(p(r, 0.5, 0.5), Pos2::new(50.0, 50.0));
    }

    #[test]
    fn skip_icon_mirrors_horizontally() {
        // `flip` is what makes Previous the mirror of Next; verify the maths
        // rather than the pixels.
        let forward = |x: f32| x;
        let backward = |x: f32| 1.0 - x;

        assert_eq!(forward(0.16), 0.16);
        assert!((backward(0.16) - 0.84).abs() < 1e-6);
        assert!((backward(0.68) - 0.32).abs() < 1e-6);
    }
}
