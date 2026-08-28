//! System font loading.
//!
//! egui's built-in font is fine for tool UIs but it is the single strongest
//! "this is a debug window" signal, so the shell loads a real UI font instead.
//! Rather than bundling one (and its licence), we look for the platform's own
//! fonts by family name and fall back to the built-in if nothing matches.
//!
//! Two families are registered:
//!   * [`FontFamily::Proportional`] - regular weight, used for all body text.
//!   * [`STRONG_FAMILY`] - semibold, used for headings, track titles and the
//!     nav rail, so emphasis comes from weight rather than only size.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

/// Family name for the semibold face. Use via `FontFamily::Name(STRONG_FAMILY.into())`.
pub const STRONG_FAMILY: &str = "strong";

/// What the loader actually managed to find, for display in Settings.
#[derive(Debug, Clone, Default)]
pub struct FontReport {
    pub regular: Option<String>,
    pub strong: Option<String>,
}

impl FontReport {
    /// True when we fell back to egui's built-in font for body text.
    pub fn using_builtin(&self) -> bool {
        self.regular.is_none()
    }

    pub fn summary(&self) -> String {
        match (&self.regular, &self.strong) {
            (Some(r), Some(s)) if r == s => r.clone(),
            (Some(r), Some(s)) => format!("{r} + {s}"),
            (Some(r), None) => r.clone(),
            (None, _) => "egui built-in".to_owned(),
        }
    }
}

/// Install fonts into `ctx`, preferring `candidates` in order.
///
/// Never fails: an unreadable or missing font simply leaves the built-in in
/// place, because a player that refuses to start over a font is worse than one
/// that looks slightly plainer.
pub fn install(ctx: &egui::Context, candidates: &[String]) -> FontReport {
    let installed = scan_font_dirs();
    let mut definitions = FontDefinitions::default();
    let mut report = FontReport::default();

    for family in candidates {
        let Some(faces) = resolve_family(family, &installed) else {
            continue;
        };

        if let Some((name, bytes)) = load(&faces.regular) {
            definitions
                .font_data
                .insert(name.clone(), Arc::new(FontData::from_owned(bytes)));

            // Put ours first and keep egui's default behind it as a glyph
            // fallback, so CJK and symbol coverage does not regress.
            definitions
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .insert(0, name.clone());

            report.regular = Some(family.clone());

            // Semibold is optional; without it the strong family reuses regular
            // and headings simply lean on size alone.
            let strong_name = faces
                .strong
                .as_ref()
                .and_then(|p| load(p))
                .map(|(strong_name, bytes)| {
                    definitions
                        .font_data
                        .insert(strong_name.clone(), Arc::new(FontData::from_owned(bytes)));
                    report.strong = Some(format!("{family} Semibold"));
                    strong_name
                })
                .unwrap_or_else(|| {
                    report.strong = report.regular.clone();
                    name.clone()
                });

            let mut strong_stack = vec![strong_name];
            strong_stack.extend(
                definitions
                    .families
                    .get(&FontFamily::Proportional)
                    .cloned()
                    .unwrap_or_default(),
            );
            definitions
                .families
                .insert(FontFamily::Name(STRONG_FAMILY.into()), strong_stack);

            break;
        }
    }

    if report.using_builtin() {
        // Still register the strong family so call sites do not have to branch.
        let proportional = definitions
            .families
            .get(&FontFamily::Proportional)
            .cloned()
            .unwrap_or_default();
        definitions
            .families
            .insert(FontFamily::Name(STRONG_FAMILY.into()), proportional);

        tracing::info!("no preferred UI font found; using the egui built-in");
    } else {
        tracing::info!("UI font: {}", report.summary());
    }

    ctx.set_fonts(definitions);
    report
}

/// Regular and (optionally) semibold files for one family.
struct Faces {
    regular: PathBuf,
    strong: Option<PathBuf>,
}

fn load(path: &Path) -> Option<(String, Vec<u8>)> {
    // Font collections need an index to select a face; ab_glyph takes a single
    // face, so skip them rather than render garbage.
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("ttc"))
    {
        return None;
    }

    match std::fs::read(path) {
        Ok(bytes) => {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("ui-font")
                .to_owned();
            Some((name, bytes))
        }
        Err(err) => {
            tracing::debug!("could not read font {}: {err}", path.display());
            None
        }
    }
}

/// Map a family name onto concrete files present on this machine.
fn resolve_family(family: &str, installed: &BTreeMap<String, PathBuf>) -> Option<Faces> {
    let (regular_names, strong_names) = known_filenames(family);

    let find = |names: &[&str]| -> Option<PathBuf> {
        names
            .iter()
            .find_map(|n| installed.get(&n.to_ascii_lowercase()).cloned())
    };

    let regular = find(&regular_names)?;
    let strong = find(&strong_names);
    Some(Faces { regular, strong })
}

/// Candidate filenames for a family, most specific first.
///
/// Matching by filename rather than parsing every font's internal name table
/// keeps startup fast; the trade-off is this table, which only needs to cover
/// fonts we would actually pick.
fn known_filenames(family: &str) -> (Vec<&'static str>, Vec<&'static str>) {
    let normalised = family.to_ascii_lowercase();

    match normalised.as_str() {
        // Windows 11. Absent on Windows 10, which falls through to Segoe UI.
        "segoe ui variable" | "segoe ui variable text" | "segoe ui variable display" => (
            vec!["SegUIVar.ttf", "SegoeUIVariableStatic-Regular.ttf"],
            vec!["SegoeUIVariableStatic-Semibold.ttf"],
        ),
        "segoe ui" => (
            vec!["segoeui.ttf"],
            vec!["seguisb.ttf", "segoeuisb.ttf", "segoeuib.ttf"],
        ),
        "inter" => (
            vec!["Inter-Regular.ttf", "Inter_28pt-Regular.ttf", "inter.ttf"],
            vec!["Inter-SemiBold.ttf", "Inter_28pt-SemiBold.ttf"],
        ),
        "manrope" => (
            vec!["Manrope-Regular.ttf", "manrope.ttf"],
            vec!["Manrope-SemiBold.ttf", "Manrope-Bold.ttf"],
        ),
        "roboto" => (
            vec!["Roboto-Regular.ttf"],
            vec!["Roboto-Medium.ttf", "Roboto-Bold.ttf"],
        ),
        "arial" => (vec!["arial.ttf"], vec!["arialbd.ttf"]),
        // Unknown family: try the obvious permutations before giving up.
        other => {
            let _ = other;
            (Vec::new(), Vec::new())
        }
    }
}

/// Index every font file available to this user, keyed by lowercase filename.
fn scan_font_dirs() -> BTreeMap<String, PathBuf> {
    let mut found = BTreeMap::new();

    for dir in font_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let is_font = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| ["ttf", "otf", "ttc"].contains(&e.to_ascii_lowercase().as_str()));

            if !is_font {
                continue;
            }

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                // Per-user fonts are scanned last and must not shadow system
                // ones, so only insert when absent.
                found.entry(name.to_ascii_lowercase()).or_insert(path);
            }
        }
    }

    found
}

fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    #[cfg(target_os = "windows")]
    {
        if let Ok(windir) = std::env::var("SystemRoot") {
            dirs.push(PathBuf::from(windir).join("Fonts"));
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join("Microsoft/Windows/Fonts"));
        }
    }

    #[cfg(target_os = "linux")]
    {
        dirs.push(PathBuf::from("/usr/share/fonts"));
        dirs.push(PathBuf::from("/usr/local/share/fonts"));
        if let Ok(home) = std::env::var("HOME") {
            dirs.push(PathBuf::from(home).join(".local/share/fonts"));
        }
    }

    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/System/Library/Fonts"));
        dirs.push(PathBuf::from("/Library/Fonts"));
        if let Ok(home) = std::env::var("HOME") {
            dirs.push(PathBuf::from(home).join("Library/Fonts"));
        }
    }

    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_collections_are_skipped() {
        // .ttc needs a face index we cannot supply, so loading must decline.
        assert!(load(Path::new("C:/Windows/Fonts/cambria.ttc")).is_none());
    }

    #[test]
    fn unknown_families_resolve_to_nothing() {
        let installed = BTreeMap::new();
        assert!(resolve_family("Definitely Not A Font", &installed).is_none());
    }

    #[test]
    fn family_lookup_is_case_insensitive() {
        let mut installed = BTreeMap::new();
        installed.insert(
            "segoeui.ttf".to_owned(),
            PathBuf::from("/fonts/segoeui.ttf"),
        );

        let faces =
            resolve_family("SEGOE UI", &installed).expect("should match regardless of case");
        assert_eq!(faces.regular, PathBuf::from("/fonts/segoeui.ttf"));
        assert!(faces.strong.is_none(), "no semibold file was present");
    }

    #[test]
    fn semibold_is_picked_up_when_present() {
        let mut installed = BTreeMap::new();
        installed.insert(
            "segoeui.ttf".to_owned(),
            PathBuf::from("/fonts/segoeui.ttf"),
        );
        installed.insert(
            "seguisb.ttf".to_owned(),
            PathBuf::from("/fonts/seguisb.ttf"),
        );

        let faces = resolve_family("Segoe UI", &installed).expect("match");
        assert_eq!(faces.strong, Some(PathBuf::from("/fonts/seguisb.ttf")));
    }

    #[test]
    fn report_describes_the_fallback_case() {
        let report = FontReport::default();
        assert!(report.using_builtin());
        assert_eq!(report.summary(), "egui built-in");
    }
}
