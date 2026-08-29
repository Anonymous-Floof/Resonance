//! Export a bundle, import it into a fresh library, and check what came back.
//!
//! The property that matters is not "the zip parses" — it is that a person can
//! move to a new machine and find their settings, their playlists and their
//! listening history where they left them. These tests exercise that whole
//! path against real files on disk.

use std::path::{Path, PathBuf};

use mp_core::Config;
use mp_core::bundle::{self, ExportOptions, Mode};
use mp_core::library::{Library, Progress, ScanOptions};

/// A scratch directory that removes itself when the guard is dropped.
///
/// Returned rather than a bare path so that a failing assertion cleans up too:
/// the names carry a pid, so a directory left behind by one run is never
/// reused by the next, it just accumulates. Callers use `.path()`.
fn scratch(name: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("resonance-bundle-{name}-"))
        .tempdir()
        .unwrap()
}

/// Files recognised by extension. Nothing here decodes them.
fn music(root: &Path, names: &[&str]) {
    for name in names {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, b"not decodable, but recognisable by extension").unwrap();
    }
}

fn scanned(root: &Path) -> Library {
    let mut library = Library::in_memory().unwrap();
    library
        .scan_blocking(
            &ScanOptions {
                roots: vec![root.to_path_buf()],
                min_duration: std::time::Duration::ZERO,
                extract_art: false,
                ..ScanOptions::default()
            },
            &Progress::new(),
        )
        .unwrap();
    library
}

fn track_paths(library: &Library, playlist: mp_core::library::PlaylistId) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = library
        .playlist_tracks(playlist)
        .unwrap()
        .into_iter()
        .map(|track| track.path)
        .collect();
    paths.sort();
    paths
}

fn playlist_named(library: &Library, name: &str) -> Option<mp_core::library::Playlist> {
    library
        .playlists()
        .unwrap()
        .into_iter()
        .find(|playlist| playlist.name == name)
}

#[test]
fn settings_and_playlists_survive_a_round_trip() {
    let scratch_dir = scratch("roundtrip");
    let dir = scratch_dir.path();
    music(dir, &["a.mp3", "deep/b.flac", "c.wav"]);

    // -- the machine being left behind
    let mut library = scanned(dir);

    let ids: Vec<i64> = library
        .tracks(&Default::default(), mp_core::library::Order::Title, false)
        .unwrap()
        .iter()
        .map(|track| track.id)
        .collect();
    assert_eq!(ids.len(), 3);

    let mixtape = library.create_playlist("Mixtape").unwrap();
    library.add_to_playlist(mixtape, &ids).unwrap();
    let expected = track_paths(&library, mixtape);

    library
        .create_smart_playlist("Everything", &mp_core::library::SmartRules::default())
        .unwrap();

    let mut config = Config::default();
    config.appearance.accent = "#FF8800".into();
    config.playback.volume = 0.42;
    config.library.watched_folders = vec![dir.to_path_buf()];

    let bundle_path = dir.join("settings.mpbundle");
    let manifest = bundle::export(
        &bundle_path,
        &config,
        &library,
        ExportOptions {
            include_playlists: true,
            include_statistics: false,
        },
    )
    .unwrap();

    assert!(bundle_path.is_file());
    assert_eq!(manifest.playlists, 2);
    assert!(!manifest.has_statistics);

    // -- the new machine: same music, nothing configured
    let mut fresh_config = Config::default();
    let mut fresh = scanned(dir);

    let summary =
        bundle::import(&bundle_path, &mut fresh_config, &mut fresh, Mode::Replace).unwrap();

    assert!(summary.settings_applied);
    assert_eq!(summary.playlists_added, 2);
    assert!(
        summary.tracks_missing.is_empty(),
        "{:?}",
        summary.tracks_missing
    );

    assert_eq!(fresh_config.appearance.accent, "#FF8800");
    assert!((fresh_config.playback.volume - 0.42).abs() < 1e-6);

    let imported = playlist_named(&fresh, "Mixtape").expect("the playlist came across");
    assert_eq!(track_paths(&fresh, imported.id), expected);

    let smart = playlist_named(&fresh, "Everything").expect("the smart one too");
    assert!(smart.rules.is_some(), "its rules should have come with it");

    let _ = std::fs::remove_dir_all(dir);
}

/// Importing the same bundle twice must leave the library exactly as the first
/// import did — otherwise "did that work?" cannot be answered by retrying.
#[test]
fn importing_twice_changes_nothing_the_second_time() {
    let scratch_dir = scratch("idempotent");
    let dir = scratch_dir.path();
    music(dir, &["a.mp3", "b.mp3"]);

    let mut library = scanned(dir);
    let ids: Vec<i64> = library
        .tracks(&Default::default(), mp_core::library::Order::Title, false)
        .unwrap()
        .iter()
        .map(|track| track.id)
        .collect();

    library.record_play(ids[0]).unwrap();
    library.record_play(ids[0]).unwrap();
    library.record_play(ids[1]).unwrap();

    let list = library.create_playlist("Once").unwrap();
    library.add_to_playlist(list, &ids).unwrap();

    let bundle_path = dir.join("twice.mpbundle");
    bundle::export(
        &bundle_path,
        &Config::default(),
        &library,
        ExportOptions {
            include_playlists: true,
            include_statistics: true,
        },
    )
    .unwrap();

    let mut config = Config::default();
    let mut fresh = scanned(dir);

    let first = bundle::import(&bundle_path, &mut config, &mut fresh, Mode::Replace).unwrap();
    assert_eq!(first.playlists_added, 1);

    let after_first = fresh.play_statistics().unwrap();

    let second = bundle::import(&bundle_path, &mut config, &mut fresh, Mode::Replace).unwrap();
    assert_eq!(
        second.playlists_added, 0,
        "it should have replaced, not added"
    );
    assert_eq!(second.playlists_replaced, 1);

    assert_eq!(
        fresh.playlists().unwrap().len(),
        1,
        "a duplicate playlist was created"
    );

    let after_second = fresh.play_statistics().unwrap();
    assert_eq!(
        after_first.len(),
        after_second.len(),
        "the statistics changed on a repeated import"
    );
    for (a, b) in after_first.iter().zip(&after_second) {
        assert_eq!(a.path, b.path);
        assert_eq!(a.play_count, b.play_count);
        assert_eq!(
            a.plays.len(),
            b.plays.len(),
            "history entries were duplicated"
        );
    }

    let _ = std::fs::remove_dir_all(dir);
}

/// Merge is for filling gaps, not for overwriting what is already there.
#[test]
fn merging_leaves_existing_settings_and_playlists_alone() {
    let scratch_dir = scratch("merge");
    let dir = scratch_dir.path();
    music(dir, &["a.mp3", "b.mp3"]);

    let mut source = scanned(dir);
    let ids: Vec<i64> = source
        .tracks(&Default::default(), mp_core::library::Order::Title, false)
        .unwrap()
        .iter()
        .map(|track| track.id)
        .collect();

    let shared = source.create_playlist("Shared Name").unwrap();
    source.add_to_playlist(shared, &ids).unwrap();
    let only_here = source.create_playlist("Only In The Bundle").unwrap();
    source.add_to_playlist(only_here, &ids[..1]).unwrap();

    let mut exported_config = Config::default();
    exported_config.appearance.accent = "#123456".into();

    let bundle_path = dir.join("merge.mpbundle");
    bundle::export(
        &bundle_path,
        &exported_config,
        &source,
        ExportOptions::default(),
    )
    .unwrap();

    // The destination already has a playlist by that name, with one track.
    let mut destination = scanned(dir);
    let mine = destination.create_playlist("Shared Name").unwrap();
    destination.add_to_playlist(mine, &ids[..1]).unwrap();

    let mut config = Config::default();
    let before_accent = config.appearance.accent.clone();

    let summary = bundle::import(&bundle_path, &mut config, &mut destination, Mode::Merge).unwrap();

    assert!(!summary.settings_applied, "merge must not touch settings");
    assert_eq!(config.appearance.accent, before_accent);

    assert_eq!(summary.playlists_skipped, 1);
    assert_eq!(summary.playlists_added, 1);

    let kept = playlist_named(&destination, "Shared Name").unwrap();
    assert_eq!(
        track_paths(&destination, kept.id).len(),
        1,
        "the existing playlist was overwritten"
    );

    assert!(playlist_named(&destination, "Only In The Bundle").is_some());

    let _ = std::fs::remove_dir_all(dir);
}

/// A bundle from a machine with more music than this one has must import what
/// it can and say plainly what it could not.
#[test]
fn tracks_that_are_not_here_are_reported() {
    let scratch_dir = scratch("missing");
    let dir = scratch_dir.path();
    music(dir, &["here.mp3"]);

    let mut source = scanned(dir);
    let here = source.id_for_path(&dir.join("here.mp3")).unwrap().unwrap();

    // A playlist referring to a file this machine will not have.
    let list = source.create_playlist("Partial").unwrap();
    source.add_to_playlist(list, &[here]).unwrap();

    let bundle_path = dir.join("partial.mpbundle");
    bundle::export(
        &bundle_path,
        &Config::default(),
        &source,
        ExportOptions::default(),
    )
    .unwrap();

    // An empty library stands in for a machine that has the playlist's music
    // somewhere else, or not at all — which is what a bundle from a larger
    // collection produces.
    let mut config = Config::default();
    let mut elsewhere = Library::in_memory().unwrap();

    let summary = bundle::import(&bundle_path, &mut config, &mut elsewhere, Mode::Replace).unwrap();

    assert_eq!(
        summary.playlists_added, 1,
        "the playlist itself still arrives"
    );
    assert_eq!(summary.tracks_matched, 0);
    assert_eq!(summary.tracks_missing.len(), 1);
    assert!(
        summary.summary().contains("not in your library"),
        "{}",
        summary.summary()
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn statistics_are_only_included_when_asked_for() {
    let scratch_dir = scratch("stats-opt-in");
    let dir = scratch_dir.path();
    music(dir, &["a.mp3"]);

    let library = scanned(dir);
    let id = library.id_for_path(&dir.join("a.mp3")).unwrap().unwrap();
    library.record_play(id).unwrap();

    let without = dir.join("without.mpbundle");
    let manifest = bundle::export(
        &without,
        &Config::default(),
        &library,
        ExportOptions {
            include_playlists: true,
            include_statistics: false,
        },
    )
    .unwrap();
    assert!(!manifest.has_statistics);

    let with = dir.join("with.mpbundle");
    let manifest = bundle::export(
        &with,
        &Config::default(),
        &library,
        ExportOptions {
            include_playlists: true,
            include_statistics: true,
        },
    )
    .unwrap();
    assert!(manifest.has_statistics);

    // And the counts actually arrive.
    let mut fresh = scanned(dir);
    let mut config = Config::default();
    let summary = bundle::import(&with, &mut config, &mut fresh, Mode::Replace).unwrap();

    assert_eq!(summary.statistics_applied, 1);
    let stats = fresh.play_statistics().unwrap();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].play_count, 1);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn something_that_is_not_a_bundle_is_refused_before_anything_is_applied() {
    let scratch_dir = scratch("not-a-bundle");
    let dir = scratch_dir.path();

    let text = dir.join("notes.txt");
    std::fs::write(&text, b"just some text").unwrap();

    let mut config = Config::default();
    config.appearance.accent = "#ABCDEF".into();
    let before = config.appearance.accent.clone();
    let mut library = Library::in_memory().unwrap();

    assert!(bundle::inspect(&text).is_err());
    assert!(bundle::import(&text, &mut config, &mut library, Mode::Replace).is_err());
    assert_eq!(config.appearance.accent, before, "settings were touched");

    // A valid zip that is not one of ours must also be refused.
    let stranger = dir.join("stranger.mpbundle");
    {
        let file = std::fs::File::create(&stranger).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        zip.start_file("hello.txt", options).unwrap();
        std::io::Write::write_all(&mut zip, b"hi").unwrap();
        zip.finish().unwrap();
    }

    let refused = bundle::inspect(&stranger);
    assert!(refused.is_err());
    assert!(
        refused.unwrap_err().to_string().contains("manifest"),
        "the refusal should explain itself"
    );

    let _ = std::fs::remove_dir_all(dir);
}

/// A failed export must not leave a broken file where a good one was.
#[test]
fn an_exported_bundle_is_a_readable_zip_with_the_expected_members() {
    let scratch_dir = scratch("members");
    let dir = scratch_dir.path();
    music(dir, &["a.mp3"]);

    let library = scanned(dir);
    let path = dir.join("out.mpbundle");

    bundle::export(
        &path,
        &Config::default(),
        &library,
        ExportOptions {
            include_playlists: true,
            include_statistics: true,
        },
    )
    .unwrap();

    let file = std::fs::File::open(&path).unwrap();
    let mut zip = zip::ZipArchive::new(file).unwrap();

    let names: Vec<String> = (0..zip.len())
        .map(|index| zip.by_index(index).unwrap().name().to_owned())
        .collect();

    for expected in [
        "manifest.json",
        "config.toml",
        "playlists.json",
        "statistics.json",
    ] {
        assert!(
            names.contains(&expected.to_owned()),
            "missing {expected} in {names:?}"
        );
    }

    // And nothing is left behind from the atomic write.
    assert!(
        !dir.join("out.mpbundle.part").exists(),
        "a temporary file was left behind"
    );

    let _ = std::fs::remove_dir_all(dir);
}
