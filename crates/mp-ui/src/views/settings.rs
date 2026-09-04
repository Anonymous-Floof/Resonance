//! The settings view.
//!
//! Edits the live [`Config`] in place. The caller is responsible for noticing
//! that something changed and persisting it - this returns [`Changed`] rather
//! than writing to disk itself, so a slider drag does not cause a file write
//! per frame.

use egui::{RichText, TextStyle, Ui};
use mp_core::config::{
    Config, CrossfadeCurve, Density, Grouping, ReplayGainMode, ShuffleMode, SortKey, SurfaceStyle,
    ThemeMode, VisualizerKind, VizColorMode,
};

use crate::theme::{Theme, col};
use crate::visualizer;
use crate::widgets::{self, icons::Icon};

/// Whether anything in the config was edited this frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Changed(pub bool);

impl Changed {
    fn or(self, other: bool) -> Self {
        Self(self.0 || other)
    }
}

/// Width reserved for a row's control, in spacing units.
///
/// Wide enough for the longest combo box label and for a slider with its
/// number beside it. Capped at half the row at runtime, so a very narrow
/// window splits the difference rather than starving the label entirely.
const CONTROL_WIDTH_UNITS: f32 = 24.0;

/// Live state the settings screen reports but does not own.
///
/// Grouped rather than passed loose because these are all transient facts
/// about a running session rather than settings, and they read identically at
/// the call site.
#[derive(Debug, Clone, Copy)]
pub struct Live<'a> {
    pub sleep: Option<crate::player::Sleep>,
    /// Crossfades begun this session.
    pub fades: u64,
    /// What has been looked up online, and where the record of it is.
    pub network: Network<'a>,
}

/// What the Online section reports back to the user.
///
/// Counts come from the activity log's in-memory tail, so drawing this costs
/// nothing and touches no disk.
#[derive(Debug, Clone, Copy)]
pub struct Network<'a> {
    /// The service lyrics come from, so the description on screen and the code
    /// that makes the request cannot disagree.
    pub source: &'static mp_net::Source,
    /// Entries in the log, including the ones that never left the machine.
    pub entries: usize,
    /// How many of those were actual requests.
    pub requests: usize,
    /// Where the log is, when it is being written to disk.
    pub log_path: Option<&'a std::path::Path>,
}

impl Network<'_> {
    /// One line saying what has actually happened.
    ///
    /// Distinguishes requests from lookups on purpose. "43 lookups, 2 of which
    /// were requests" is the true and reassuring shape of the numbers, and
    /// reporting only the larger one would misrepresent the traffic.
    fn summary(&self) -> String {
        if self.entries == 0 {
            return "Nothing has been looked up yet, and nothing has left this machine.".to_owned();
        }

        let lookups = match self.entries {
            1 => "1 lookup".to_owned(),
            n => format!("{n} lookups"),
        };

        match self.requests {
            0 => format!("{lookups} this session, none of which left this machine."),
            1 => format!("{lookups} this session, 1 of which was a request."),
            n => format!("{lookups} this session, {n} of which were requests."),
        }
    }
}

/// Actions the settings view cannot perform itself.
#[derive(Debug, Default)]
pub struct SettingsOutcome {
    pub changed: Changed,
    /// User asked to pick a folder to add to the library.
    pub add_folder_requested: bool,
    /// Index into `library.watched_folders` to remove.
    pub remove_folder: Option<usize>,
    /// Theme-affecting settings changed and the style needs rebuilding.
    pub restyle: bool,
    /// Undo this journalled tag edit.
    pub undo_tag_edit: Option<i64>,

    /// Save everything to an `.mpbundle` file.
    pub export_bundle: bool,
    /// Load an `.mpbundle`, replacing settings.
    pub import_bundle_replace: bool,
    /// Load an `.mpbundle`, keeping settings and adding only what is missing.
    pub import_bundle_merge: bool,

    /// The sleep timer was changed. `Some(None)` cancels it.
    ///
    /// Nested because "the user did not touch it" and "the user turned it off"
    /// are different, and only the second should disturb a running timer.
    pub set_sleep: Option<Option<crate::player::Sleep>>,

    /// Open the network activity log in the system file manager.
    pub show_activity_log: bool,
    /// Forget every lyric fetched so far.
    pub clear_lyrics_cache: bool,

    /// The output device or buffer size changed; the stream has to be
    /// reopened for it to take effect.
    ///
    /// Separate from `changed`, which only means "save the config". Reopening
    /// the device interrupts playback, so it happens when the user actually
    /// picks something rather than on every keystroke elsewhere in Settings.
    pub reopen_device: bool,
}

pub fn show(
    ui: &mut Ui,
    theme: &Theme,
    config: &mut Config,
    font_summary: &str,
    analysis: Option<crate::analysis_job::Status>,
    tag_history: &[mp_core::library::TagEdit],
    live: Live<'_>,
) -> SettingsOutcome {
    let mut out = SettingsOutcome::default();
    let m = &theme.metrics;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(m.space(1.0));
            ui.label(
                RichText::new("Settings")
                    .text_style(TextStyle::Heading)
                    .color(col(theme.palette.text_primary)),
            );
            ui.add_space(m.space(2.0));

            let before_style = (
                config.appearance.theme,
                config.appearance.density,
                config.appearance.accent.clone(),
                config.appearance.ui_scale,
            );

            appearance_section(ui, theme, config, font_summary, &mut out);
            playback_section(ui, theme, config, live, &mut out);
            output_section(ui, theme, config, &mut out);
            library_section(ui, theme, config, analysis, &mut out);
            tag_history_section(ui, theme, config, tag_history, &mut out);
            visualizer_section(ui, theme, config, &mut out);
            backup_section(ui, theme, config, &mut out);
            shortcuts_section(ui, theme);
            online_section(ui, theme, config, live.network, &mut out);
            privacy_section(ui, theme, config, &mut out);

            let after_style = (
                config.appearance.theme,
                config.appearance.density,
                config.appearance.accent.clone(),
                config.appearance.ui_scale,
            );
            out.restyle = before_style != after_style;

            ui.add_space(m.space(4.0));
        });

    out
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

fn appearance_section(
    ui: &mut Ui,
    theme: &Theme,
    config: &mut Config,
    font_summary: &str,
    out: &mut SettingsOutcome,
) {
    section(ui, theme, "Appearance", |ui| {
        let a = &mut config.appearance;

        row(ui, theme, "Theme", "How the app is coloured", |ui| {
            let mut changed = false;
            egui::ComboBox::from_id_salt("theme_mode")
                .selected_text(theme_label(a.theme))
                .show_ui(ui, |ui| {
                    for mode in [ThemeMode::Dark, ThemeMode::Light, ThemeMode::Adaptive] {
                        changed |= ui
                            .selectable_value(&mut a.theme, mode, theme_label(mode))
                            .changed();
                    }
                });
            changed
        })
        .apply(out);

        row(
            ui,
            theme,
            "Density",
            "Row height and spacing throughout",
            |ui| {
                let mut changed = false;
                egui::ComboBox::from_id_salt("density")
                    .selected_text(match a.density {
                        Density::Comfortable => "Comfortable",
                        Density::Compact => "Compact",
                    })
                    .show_ui(ui, |ui| {
                        changed |= ui
                            .selectable_value(&mut a.density, Density::Comfortable, "Comfortable")
                            .changed();
                        changed |= ui
                            .selectable_value(&mut a.density, Density::Compact, "Compact")
                            .changed();
                    });
                changed
            },
        )
        .apply(out);

        row(ui, theme, "Accent colour", "Hex, e.g. #7C5CFF", |ui| {
            ui.add(egui::TextEdit::singleline(&mut a.accent).desired_width(110.0))
                .changed()
        })
        .apply(out);

        row(
            ui,
            theme,
            "Interface scale",
            "On top of your display DPI",
            |ui| {
                ui.add(
                    egui::Slider::new(&mut a.ui_scale, 0.75..=1.75)
                        .step_by(0.05)
                        .show_value(true),
                )
                .changed()
            },
        )
        .apply(out);

        row(
            ui,
            theme,
            "Window backdrop",
            "Mica effect on Windows 11",
            |ui| ui.checkbox(&mut a.mica_backdrop, "").changed(),
        )
        .apply(out);

        row(
            ui,
            theme,
            "Content background",
            "Behind the lists and panels",
            |ui| surface_picker(ui, "content_bg", &mut a.content_background),
        )
        .apply(out);

        row(
            ui,
            theme,
            "Player bar background",
            "Behind the transport along the bottom",
            |ui| surface_picker(ui, "player_bg", &mut a.player_background),
        )
        .apply(out);

        if a.content_background != SurfaceStyle::Solid || a.player_background != SurfaceStyle::Solid
        {
            row(
                ui,
                theme,
                "Background strength",
                "How much of it shows through",
                |ui| {
                    ui.add(
                        egui::Slider::new(&mut a.background_intensity, 0.0..=1.0).show_value(false),
                    )
                    .changed()
                },
            )
            .apply(out);

            note(
                ui,
                theme,
                "Backgrounds stay well behind the text at every setting, and fall back to the plain colour when nothing is playing.",
            );
        }

        note(ui, theme, &format!("Interface font: {font_summary}"));
    });
}

/// Sleep timer lengths offered, in minutes.
const SLEEP_CHOICES: &[u32] = &[15, 30, 45, 60, 90, 120];

fn playback_section(
    ui: &mut Ui,
    theme: &Theme,
    config: &mut Config,
    live: Live<'_>,
    out: &mut SettingsOutcome,
) {
    section(ui, theme, "Playback", |ui| {
        sleep_row(ui, theme, live.sleep, out);
        let p = &mut config.playback;

        row(ui, theme, "Shuffle", "How the next track is chosen", |ui| {
            let mut changed = false;
            egui::ComboBox::from_id_salt("shuffle")
                .selected_text(match p.shuffle {
                    ShuffleMode::Off => "Off",
                    ShuffleMode::Random => "Random",
                    ShuffleMode::Smart => "Smart",
                })
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(&mut p.shuffle, ShuffleMode::Off, "Off")
                        .changed();
                    changed |= ui
                        .selectable_value(&mut p.shuffle, ShuffleMode::Random, "Random")
                        .changed();
                    changed |= ui
                        .selectable_value(&mut p.shuffle, ShuffleMode::Smart, "Smart")
                        .on_hover_text("Spaces out the same artist and avoids recent repeats")
                        .changed();
                });
            changed
        })
        .apply(out);

        row(
            ui,
            theme,
            "Gapless playback",
            "No silence between tracks",
            |ui| ui.checkbox(&mut p.gapless, "").changed(),
        )
        .apply(out);

        row(
            ui,
            theme,
            "Crossfade",
            "Seconds of overlap; 0 disables",
            |ui| {
                ui.add(
                    egui::Slider::new(&mut p.crossfade_seconds, 0.0..=12.0)
                        .step_by(0.5)
                        .suffix(" s"),
                )
                .changed()
            },
        )
        .apply(out);

        if config.playback.crossfade_seconds > 0.0 {
            note(
                ui,
                theme,
                &format!(
                    "{} crossfade{} so far this session, at track ends rather than skips.",
                    live.fades,
                    if live.fades == 1 { "" } else { "s" }
                ),
            );

            let p = &mut config.playback;
            row(
                ui,
                theme,
                "Crossfade curve",
                "Equal power keeps loudness steady",
                |ui| {
                    let mut changed = false;
                    egui::ComboBox::from_id_salt("crossfade_curve")
                        .selected_text(match p.crossfade_curve {
                            CrossfadeCurve::Linear => "Linear",
                            CrossfadeCurve::EqualPower => "Equal power",
                        })
                        .show_ui(ui, |ui| {
                            changed |= ui
                                .selectable_value(
                                    &mut p.crossfade_curve,
                                    CrossfadeCurve::Linear,
                                    "Linear",
                                )
                                .changed();
                            changed |= ui
                                .selectable_value(
                                    &mut p.crossfade_curve,
                                    CrossfadeCurve::EqualPower,
                                    "Equal power",
                                )
                                .changed();
                        });
                    changed
                },
            )
            .apply(out);
        }

        let p = &mut config.playback;

        row(
            ui,
            theme,
            "Volume levelling",
            "Uses ReplayGain tags",
            |ui| {
                let mut changed = false;
                egui::ComboBox::from_id_salt("replaygain")
                    .selected_text(match p.replay_gain {
                        ReplayGainMode::Off => "Off",
                        ReplayGainMode::Track => "Per track",
                        ReplayGainMode::Album => "Per album",
                    })
                    .show_ui(ui, |ui| {
                        changed |= ui
                            .selectable_value(&mut p.replay_gain, ReplayGainMode::Off, "Off")
                            .changed();
                        changed |= ui
                            .selectable_value(
                                &mut p.replay_gain,
                                ReplayGainMode::Track,
                                "Per track",
                            )
                            .changed();
                        changed |= ui
                            .selectable_value(
                                &mut p.replay_gain,
                                ReplayGainMode::Album,
                                "Per album",
                            )
                            .changed();
                    });
                changed
            },
        )
        .apply(out);

        row(
            ui,
            theme,
            "Trim silence",
            "Skip dead air at track edges",
            |ui| ui.checkbox(&mut p.trim_silence, "").changed(),
        )
        .apply(out);

        row(
            ui,
            theme,
            "Start a track on",
            "How many clicks it takes to play a song from a list",
            |ui| {
                let mut single = p.play_on_single_click;
                let changed = egui::ComboBox::from_id_salt("click_to_play")
                    .selected_text(if single { "One click" } else { "Two clicks" })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut single, false, "Two clicks")
                            .changed()
                            | ui.selectable_value(&mut single, true, "One click")
                                .changed()
                    })
                    .inner
                    .unwrap_or(false);

                if changed {
                    p.play_on_single_click = single;
                }
                changed
            },
        )
        .apply(out);

        row(
            ui,
            theme,
            "Keep playing",
            "Continue with similar tracks when the queue empties",
            |ui| ui.checkbox(&mut p.auto_radio, "").changed(),
        )
        .apply(out);

        if p.auto_radio {
            row(
                ui,
                theme,
                "Radio batch",
                "How many tracks to add each time",
                |ui| {
                    ui.add(egui::Slider::new(&mut p.radio_batch, 1..=50))
                        .changed()
                },
            )
            .apply(out);
        }
    });
}

/// The sleep timer: transient, so it is state rather than a saved setting.
fn sleep_row(
    ui: &mut Ui,
    theme: &Theme,
    sleep: Option<crate::player::Sleep>,
    out: &mut SettingsOutcome,
) {
    use crate::player::Sleep;

    let label = match sleep {
        None => "Off".to_owned(),
        Some(Sleep::EndOfTrack) => "End of track".to_owned(),
        Some(Sleep::In(secs)) => {
            // Rounded up, so a timer with forty seconds left reads "1 min"
            // rather than counting down through a zero that has not arrived.
            let minutes = (secs / 60.0).ceil() as u32;
            format!("{minutes} min left")
        }
    };

    row(
        ui,
        theme,
        "Sleep timer",
        "Stop playing after a while",
        |ui| {
            let mut picked = None;

            egui::ComboBox::from_id_salt("sleep_timer")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    if ui.selectable_label(sleep.is_none(), "Off").clicked() {
                        picked = Some(None);
                    }
                    if ui
                        .selectable_label(sleep == Some(Sleep::EndOfTrack), "End of track")
                        .clicked()
                    {
                        picked = Some(Some(Sleep::EndOfTrack));
                    }
                    for &minutes in SLEEP_CHOICES {
                        if ui
                            .selectable_label(false, format!("{minutes} minutes"))
                            .clicked()
                        {
                            picked = Some(Some(Sleep::In(f64::from(minutes) * 60.0)));
                        }
                    }
                });

            if let Some(choice) = picked {
                out.set_sleep = Some(choice);
                return true;
            }
            false
        },
    );
}

/// Buffer sizes offered, in frames.
///
/// A short buffer responds faster and costs more CPU; a long one is the
/// opposite and is what fixes crackling on a busy machine. Automatic lets the
/// backend choose, which is right until it is not.
const BUFFER_CHOICES: &[u32] = &[128, 256, 512, 1024, 2048, 4096];

fn output_section(ui: &mut Ui, theme: &Theme, config: &mut Config, out: &mut SettingsOutcome) {
    section(ui, theme, "Output", |ui| {
        let devices = mp_audio::device::list_outputs();

        let picked = row(ui, theme, "Device", "Where audio is sent", |ui| {
            let mut changed = false;
            let p = &mut config.playback;

            let selected = p
                .output_device
                .clone()
                .unwrap_or_else(|| "System default".to_owned());

            egui::ComboBox::from_id_salt("output_device")
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(&mut p.output_device, None, "System default")
                        .changed();

                    for device in &devices {
                        let label = if device.is_default {
                            format!("{} (default)", device.name)
                        } else {
                            device.name.clone()
                        };
                        changed |= ui
                            .selectable_value(
                                &mut p.output_device,
                                Some(device.name.clone()),
                                label,
                            )
                            .changed();
                    }
                });
            changed
        });
        out.reopen_device |= picked.0;
        picked.apply(out);

        // A device that has been unplugged since it was chosen is still in the
        // config and still what the app is trying to open. Saying so is more
        // use than silently falling back and leaving the picker looking right.
        if let Some(name) = &config.playback.output_device
            && !devices.iter().any(|d| &d.name == name)
        {
            note(ui, theme, "That device is not available right now.");
        }

        let sized = row(
            ui,
            theme,
            "Buffer size",
            "Larger buffers cost latency but survive a busy machine",
            |ui| {
                let mut changed = false;
                let p = &mut config.playback;

                let selected = match p.buffer_frames {
                    Some(frames) => format!("{frames} frames"),
                    None => "Automatic".to_owned(),
                };

                egui::ComboBox::from_id_salt("buffer_frames")
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        changed |= ui
                            .selectable_value(&mut p.buffer_frames, None, "Automatic")
                            .changed();

                        for &frames in BUFFER_CHOICES {
                            changed |= ui
                                .selectable_value(
                                    &mut p.buffer_frames,
                                    Some(frames),
                                    format!("{frames} frames"),
                                )
                                .changed();
                        }
                    });
                changed
            },
        );
        out.reopen_device |= sized.0;
        sized.apply(out);

        if devices.is_empty() {
            note(ui, theme, "No output devices were found.");
        }
    });
}

fn library_section(
    ui: &mut Ui,
    theme: &Theme,
    config: &mut Config,
    analysis: Option<crate::analysis_job::Status>,
    out: &mut SettingsOutcome,
) {
    section(ui, theme, "Library", |ui| {
        let m = &theme.metrics;

        ui.label(
            RichText::new("Music folders")
                .text_style(TextStyle::Name("nav".into()))
                .color(col(theme.palette.text_primary)),
        );
        ui.add_space(m.space(0.5));

        if config.library.watched_folders.is_empty() {
            note(
                ui,
                theme,
                "No folders added yet. Resonance only reads these locations.",
            );
        } else {
            for (index, folder) in config.library.watched_folders.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(folder.display().to_string())
                            .text_style(TextStyle::Name("subtitle".into()))
                            .color(col(theme.palette.text_secondary)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if widgets::icon_button_labelled(
                            ui,
                            theme,
                            Icon::Close,
                            m.space(2.5),
                            false,
                            "Remove this folder",
                        )
                        .clicked()
                        {
                            out.remove_folder = Some(index);
                        }
                    });
                });
            }
        }

        ui.add_space(m.space(1.0));
        if widgets::accent_button(ui, theme, "Add folder").clicked() {
            out.add_folder_requested = true;
        }

        ui.add_space(m.space(1.5));
        let l = &mut config.library;

        row(
            ui,
            theme,
            "Open on",
            "The library section to start in",
            |ui| {
                let mut changed = false;
                egui::ComboBox::from_id_salt("grouping")
                    .selected_text(grouping_label(l.default_grouping))
                    .show_ui(ui, |ui| {
                        for g in [
                            Grouping::Songs,
                            Grouping::Artists,
                            Grouping::Albums,
                            Grouping::Genres,
                            Grouping::Folders,
                        ] {
                            changed |= ui
                                .selectable_value(&mut l.default_grouping, g, grouping_label(g))
                                .changed();
                        }
                    });
                changed
            },
        )
        .apply(out);

        row(ui, theme, "Sort by", "Default track ordering", |ui| {
            let mut changed = false;
            egui::ComboBox::from_id_salt("sort")
                .selected_text(sort_label(l.default_sort))
                .show_ui(ui, |ui| {
                    for s in [
                        SortKey::Title,
                        SortKey::Artist,
                        SortKey::Album,
                        SortKey::Year,
                        SortKey::Duration,
                        SortKey::DateAdded,
                        SortKey::PlayCount,
                        SortKey::LastPlayed,
                    ] {
                        changed |= ui
                            .selectable_value(&mut l.default_sort, s, sort_label(s))
                            .changed();
                    }
                });
            changed
        })
        .apply(out);

        row(
            ui,
            theme,
            "Ignore leading 'The'",
            "Sorts 'The Wandering Hours' under W",
            |ui| ui.checkbox(&mut l.ignore_leading_articles, "").changed(),
        )
        .apply(out);

        row(
            ui,
            theme,
            "Watch for changes",
            "Re-check the folders every minute or so",
            |ui| ui.checkbox(&mut l.watch_for_changes, "").changed(),
        )
        .apply(out);

        row(
            ui,
            theme,
            "Analyse audio",
            "Improves suggestions, especially for files with no tags",
            |ui| ui.checkbox(&mut l.analyze_audio_features, "").changed(),
        )
        .apply(out);

        if l.analyze_audio_features {
            note(
                ui,
                theme,
                "Decodes each track once, in the background. It takes a while on a \
                 large library and can be stopped at any point — nothing is lost.",
            );

            if let Some(status) = analysis {
                if let Some(fraction) = status.fraction() {
                    ui.add(
                        egui::ProgressBar::new(fraction)
                            .desired_height(theme.metrics.space(0.75))
                            .fill(col(theme.palette.accent)),
                    );
                    ui.add_space(theme.metrics.space(0.5));
                }

                note(ui, theme, &status.summary());
            } else {
                note(ui, theme, "Starting…");
            }
        }

        ui.add_space(m.space(1.0));
        row(
            ui,
            theme,
            "Allow tag editing",
            "Lets Resonance write tags back to your files",
            |ui| ui.checkbox(&mut l.allow_tag_editing, "").changed(),
        )
        .apply(out);

        if config.library.allow_tag_editing {
            warn(
                ui,
                theme,
                "Tag edits modify your actual music files. Every change is \
                 reversible from the edit history.",
            );
        }
    });
}

/// Every write Resonance has made to a music file, newest first.
///
/// Only shown when tag editing is on. An empty history under a switch that is
/// off would be a section about a feature the user has not asked for.
fn tag_history_section(
    ui: &mut Ui,
    theme: &Theme,
    config: &Config,
    history: &[mp_core::library::TagEdit],
    out: &mut SettingsOutcome,
) {
    if !config.library.allow_tag_editing {
        return;
    }

    let m = &theme.metrics;
    let p = theme.palette;

    section(ui, theme, "Edit history", |ui| {
        if history.is_empty() {
            note(
                ui,
                theme,
                "No tags have been edited yet. Right-click a track in Songs to edit it.",
            );
            return;
        }

        note(
            ui,
            theme,
            "Undo restores the values this edit changed. It refuses if the file \
             has been changed since, rather than overwriting whatever did it.",
        );
        ui.add_space(m.space(1.0));

        for entry in history {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(entry.summary())
                            .text_style(TextStyle::Name("caption".into()))
                            .color(col(if entry.is_reverted() {
                                p.text_muted
                            } else {
                                p.text_primary
                            })),
                    );
                    ui.label(
                        RichText::new(file_name(&entry.path))
                            .text_style(TextStyle::Name("caption".into()))
                            .color(col(p.text_muted)),
                    );
                });

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if entry.is_reverted() {
                        ui.label(
                            RichText::new("undone")
                                .text_style(TextStyle::Name("caption".into()))
                                .color(col(p.text_muted)),
                        );
                    } else if ui.button("Undo").clicked() {
                        out.undo_tag_edit = Some(entry.id);
                    }
                });
            });

            ui.add_space(m.space(0.75));
        }
    });
}

/// The filename alone: the full path is on the editor, and here it would wrap
/// across three lines and bury the change it belongs to.
fn file_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into(),
    )
}

/// Settings bundles: the way out of, and back into, this app.
fn backup_section(ui: &mut Ui, theme: &Theme, config: &mut Config, out: &mut SettingsOutcome) {
    let m = &theme.metrics;

    section(ui, theme, "Backup", |ui| {
        note(
            ui,
            theme,
            "A bundle holds your settings and playlists in one file you can \
             copy to another machine. It is an ordinary zip, so you can open it \
             and look inside.",
        );
        ui.add_space(m.space(1.0));

        row(
            ui,
            theme,
            "Include play history",
            "Play counts and when each track was played",
            |ui| {
                ui.checkbox(&mut config.privacy.bundle_statistics, "")
                    .changed()
            },
        )
        .apply(out);

        ui.add_space(m.space(1.0));

        ui.horizontal(|ui| {
            if widgets::accent_button(ui, theme, "Export bundle").clicked() {
                out.export_bundle = true;
            }

            ui.add_space(m.space(0.75));

            // Two buttons rather than one with a mode picker, because the
            // difference between them is the whole decision and burying it in
            // a dropdown is how someone loses their settings.
            if ui
                .button("Import (replace)")
                .on_hover_text("Overwrite your settings, and replace playlists of the same name")
                .clicked()
            {
                out.import_bundle_replace = true;
            }

            ui.add_space(m.space(0.5));

            if ui
                .button("Import (merge)")
                .on_hover_text("Keep your settings; add only playlists you do not already have")
                .clicked()
            {
                out.import_bundle_merge = true;
            }
        });
    });
}

fn visualizer_section(ui: &mut Ui, theme: &Theme, config: &mut Config, out: &mut SettingsOutcome) {
    section(ui, theme, "Visualizer", |ui| {
        let v = &mut config.visualizer;

        row(ui, theme, "Style", "Which visualizer to draw", |ui| {
            let mut changed = false;
            egui::ComboBox::from_id_salt("viz_kind")
                .selected_text(viz_label(v.kind))
                .show_ui(ui, |ui| {
                    for kind in visualizer::ALL_KINDS {
                        // Anything not yet built stays visible but disabled, so
                        // the list matches the plan and nobody wonders where a
                        // missing entry went.
                        let available = visualizer::is_available(kind);
                        ui.add_enabled_ui(available, |ui| {
                            changed |= ui
                                .selectable_value(&mut v.kind, kind, viz_label(kind))
                                .changed();
                        });
                    }
                });
            changed
        })
        .apply(out);

        if !visualizer::is_available(v.kind) {
            warn(
                ui,
                theme,
                "This visualizer is not built yet — the spectrum is drawn instead.",
            );
        }

        row(
            ui,
            theme,
            "Colour",
            "Where the visualizer takes its colour",
            |ui| {
                let mut changed = false;
                egui::ComboBox::from_id_salt("viz_colour")
                    .selected_text(colour_label(v.color_mode))
                    .show_ui(ui, |ui| {
                        for mode in [
                            VizColorMode::Accent,
                            VizColorMode::AlbumArt,
                            VizColorMode::Spectrum,
                            VizColorMode::Custom,
                        ] {
                            changed |= ui
                                .selectable_value(&mut v.color_mode, mode, colour_label(mode))
                                .changed();
                        }
                    });
                changed
            },
        )
        .apply(out);

        if v.color_mode == VizColorMode::AlbumArt {
            note(
                ui,
                theme,
                "Takes a dark-to-light ramp from the current cover, deepest at the bass end. Falls back to the accent colour for a sleeve with no colour in it.",
            );
        }

        if v.color_mode == VizColorMode::Custom {
            row(
                ui,
                theme,
                "Custom colour",
                "Any hex colour, e.g. #7C5CFF",
                |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut v.custom_color)
                            .desired_width(theme.metrics.space(10.0))
                            .hint_text("#7C5CFF"),
                    )
                    .changed()
                },
            )
            .apply(out);
        }

        row(
            ui,
            theme,
            "Sensitivity",
            "Input gain into the visualizer",
            |ui| {
                ui.add(egui::Slider::new(&mut v.sensitivity, 0.1..=4.0).step_by(0.05))
                    .changed()
            },
        )
        .apply(out);

        row(ui, theme, "Smoothing", "Higher is calmer", |ui| {
            ui.add(egui::Slider::new(&mut v.smoothing, 0.0..=0.95).step_by(0.01))
                .changed()
        })
        .apply(out);

        row(ui, theme, "Bands", "Spectrum resolution", |ui| {
            ui.add(egui::Slider::new(&mut v.bar_count, 8..=256))
                .changed()
        })
        .apply(out);

        row(
            ui,
            theme,
            "Peak markers",
            "Hold at each band's recent maximum, then fall away",
            |ui| ui.checkbox(&mut v.show_peak_caps, "").changed(),
        )
        .apply(out);

        row(
            ui,
            theme,
            "Frame rate",
            "Upper limit while the visualizer is on screen",
            |ui| {
                ui.add(egui::Slider::new(&mut v.fps_cap, 15..=144).suffix(" fps"))
                    .changed()
            },
        )
        .apply(out);

        row(
            ui,
            theme,
            "Save power",
            "Drop to 30fps when the window is not focused",
            |ui| ui.checkbox(&mut v.low_power_when_unfocused, "").changed(),
        )
        .apply(out);

        note(
            ui,
            theme,
            "The visualizer only runs while its view is open, so it costs nothing anywhere else in the app.",
        );
    });
}

/// The keyboard bindings, listed straight from the table that implements them.
///
/// Generated rather than written out, so a binding cannot be added without
/// appearing here or be listed here without actually working.
fn shortcuts_section(ui: &mut Ui, theme: &Theme) {
    section(ui, theme, "Keyboard", |ui| {
        let m = theme.metrics;
        let p = theme.palette;

        note(
            ui,
            theme,
            "Single-key shortcuts are ignored while you are typing in a text box.",
        );

        for (modifiers, key, action) in crate::shortcuts::BINDINGS {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(action.label())
                        .text_style(TextStyle::Body)
                        .color(col(p.text_primary)),
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(crate::shortcuts::describe(*modifiers, *key))
                            .text_style(TextStyle::Monospace)
                            .color(col(p.text_secondary)),
                    );
                });
            });
            ui.add_space(m.space(0.5));
        }
    });
}

/// The section that distinguishes this build from the offline one.
///
/// Written to be read before the switch is touched rather than after. Every
/// sentence a user needs in order to decide is on screen: which service, what
/// leaves the machine, how often, and where to go and check afterwards. The
/// "what is sent" line comes from [`mp_net::Source::sends`] rather than being
/// typed here, so the description and the code cannot drift apart.
fn online_section(
    ui: &mut Ui,
    theme: &Theme,
    config: &mut Config,
    network: Network<'_>,
    out: &mut SettingsOutcome,
) {
    let m = &theme.metrics;

    section(ui, theme, "Online", |ui| {
        note(
            ui,
            theme,
            "This is the networked build of Resonance. It can look things up online, and it does nothing of the kind until you switch it on below.",
        );
        ui.add_space(m.space(1.0));

        row(
            ui,
            theme,
            "Fetch lyrics",
            "For tracks with no lyrics in their tags and no .lrc beside them",
            |ui| ui.checkbox(&mut config.privacy.online_lyrics, "").changed(),
        )
        .apply(out);

        if config.privacy.online_lyrics {
            row(
                ui,
                theme,
                "Try other releases",
                "If the exact recording is not found, accept another release of the same song",
                |ui| {
                    ui.checkbox(&mut config.privacy.online_lyrics_any_release, "")
                        .changed()
                },
            )
            .apply(out);

            ui.add_space(m.space(1.0));

            if config.privacy.online_lyrics_any_release {
                note(
                    ui,
                    theme,
                    "Many more matches on files saved from YouTube, whose album and length rarely match any release. The words are still matched on artist and title, so they belong to the right song — but any timings come from a different pressing and may drift.",
                );
                ui.add_space(m.space(1.0));
            }

            let source = network.source;
            note(ui, theme, &format!("Lyrics come from {}.", source.label));
            note(ui, theme, &format!("What is sent: {}", source.sends));
            note(
                ui,
                theme,
                "One request per track, only when you open the full-screen view, and never twice for the same track. Answers are cached on this machine.",
            );
        }

        ui.add_space(m.space(1.5));

        // The log is the feature that makes any of the above checkable rather
        // than merely stated, so it is shown whether or not fetching is on —
        // including the case where the honest number is zero.
        note(ui, theme, &network.summary());

        ui.add_space(m.space(1.0));

        ui.horizontal(|ui| {
            if network.log_path.is_some()
                && widgets::accent_button(ui, theme, "Show the activity log").clicked()
            {
                out.show_activity_log = true;
            }

            ui.add_space(m.space(0.75));

            if ui
                .button("Clear cached lyrics")
                .on_hover_text("Forget every lyric fetched so far, so they are looked up again")
                .clicked()
            {
                out.clear_lyrics_cache = true;
            }
        });
    });
}

fn privacy_section(ui: &mut Ui, theme: &Theme, config: &mut Config, out: &mut SettingsOutcome) {
    section(ui, theme, "Privacy", |ui| {
        note(
            ui,
            theme,
            "Your library, artwork and suggestions are all built on this machine. The only thing that leaves it is under Online, above, and it is off unless you turn it on.",
        );

        row(
            ui,
            theme,
            "Play history",
            "Track play counts locally, for smart playlists",
            |ui| {
                ui.checkbox(&mut config.privacy.track_play_history, "")
                    .changed()
            },
        )
        .apply(out);
    });
}

// ---------------------------------------------------------------------------
// Layout helpers
// ---------------------------------------------------------------------------

/// A titled group of settings on an elevated card.
fn section(ui: &mut Ui, theme: &Theme, title: &str, contents: impl FnOnce(&mut Ui)) {
    let m = &theme.metrics;

    ui.label(
        RichText::new(title)
            .text_style(TextStyle::Name("title".into()))
            .color(col(theme.palette.text_primary)),
    );
    ui.add_space(m.space(1.0));

    egui::Frame::new()
        .fill(theme.card_fill())
        .corner_radius(egui::CornerRadius::same(m.radius_large))
        .inner_margin(egui::Margin::same(m.space(2.0) as i8))
        .show(ui, contents);

    ui.add_space(m.space(3.0));
}

/// Wraps a control so it always reports whether it changed.
struct RowResult(bool);

impl RowResult {
    fn apply(self, out: &mut SettingsOutcome) {
        out.changed = out.changed.or(self.0);
    }
}

/// One labelled setting: name and description left, control right.
fn row(
    ui: &mut Ui,
    theme: &Theme,
    label: &str,
    description: &str,
    control: impl FnOnce(&mut Ui) -> bool,
) -> RowResult {
    let m = &theme.metrics;
    let mut changed = false;

    ui.horizontal(|ui| {
        // The control is given its width first and the label takes what is
        // left, rather than the label claiming a fixed share of the row.
        //
        // The other way round looked fine at full width and broke everywhere
        // else: a narrow window left a combo box or a slider less room than it
        // needed, and it was clipped off the right-hand edge — including the
        // Undo button in the edit history, which simply could not be reached.
        // Text can wrap; a slider cannot.
        let available = ui.available_width();
        let control_width = m.space(CONTROL_WIDTH_UNITS).min(available * 0.5);
        let label_width = (available - control_width - m.space(1.0)).max(m.space(6.0));

        ui.vertical(|ui| {
            ui.set_width(label_width);
            ui.label(
                RichText::new(label)
                    .text_style(TextStyle::Body)
                    .color(col(theme.palette.text_primary)),
            );
            if !description.is_empty() {
                ui.label(
                    RichText::new(description)
                        .text_style(TextStyle::Name("caption".into()))
                        .color(col(theme.palette.text_muted)),
                );
            }
        });

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.set_min_width(control_width);
            changed = control(ui);
        });
    });

    ui.add_space(m.space(1.25));
    RowResult(changed)
}

/// A combo for one panel's background source.
///
/// Shared by both rows so the two can never offer different options, and so a
/// style added to the enum appears in both without being wired twice.
fn surface_picker(ui: &mut Ui, salt: &str, current: &mut SurfaceStyle) -> bool {
    let mut changed = false;

    egui::ComboBox::from_id_salt(salt)
        .selected_text(current.label())
        .show_ui(ui, |ui| {
            for style in SurfaceStyle::ALL {
                if ui
                    .selectable_label(*current == style, style.label())
                    .on_hover_text(style.description())
                    .clicked()
                    && *current != style
                {
                    *current = style;
                    changed = true;
                }
            }
        });

    changed
}

fn note(ui: &mut Ui, theme: &Theme, text: &str) {
    ui.label(
        RichText::new(text)
            .text_style(TextStyle::Name("caption".into()))
            .color(col(theme.palette.text_muted)),
    );
    ui.add_space(theme.metrics.space(0.75));
}

fn warn(ui: &mut Ui, theme: &Theme, text: &str) {
    ui.label(
        RichText::new(text)
            .text_style(TextStyle::Name("caption".into()))
            .color(col(theme.palette.warning)),
    );
    ui.add_space(theme.metrics.space(0.75));
}

// ---------------------------------------------------------------------------
// Enum labels
// ---------------------------------------------------------------------------

fn theme_label(mode: ThemeMode) -> &'static str {
    match mode {
        ThemeMode::Dark => "Dark",
        ThemeMode::Light => "Light",
        ThemeMode::Adaptive => "Adaptive (from album art)",
    }
}

fn grouping_label(g: Grouping) -> &'static str {
    match g {
        Grouping::Songs => "Songs",
        Grouping::Artists => "Artists",
        Grouping::Albums => "Albums",
        Grouping::Genres => "Genres",
        Grouping::Folders => "Folders",
    }
}

fn sort_label(s: SortKey) -> &'static str {
    match s {
        SortKey::Title => "Title",
        SortKey::Artist => "Artist",
        SortKey::Album => "Album",
        SortKey::Year => "Year",
        SortKey::Duration => "Duration",
        SortKey::DateAdded => "Date added",
        SortKey::PlayCount => "Play count",
        SortKey::LastPlayed => "Last played",
    }
}

fn colour_label(mode: VizColorMode) -> &'static str {
    match mode {
        VizColorMode::Accent => "Accent",
        VizColorMode::AlbumArt => "Album art",
        VizColorMode::Spectrum => "Spectrum",
        VizColorMode::Custom => "Custom",
    }
}

fn viz_label(kind: VisualizerKind) -> &'static str {
    match kind {
        VisualizerKind::None => "None",
        VisualizerKind::SpectrumBars => "Spectrum bars",
        VisualizerKind::Oscilloscope => "Oscilloscope",
        VisualizerKind::RadialSpectrum => "Radial spectrum",
        VisualizerKind::WaveformRibbon => "Waveform ribbon",
        VisualizerKind::AuroraBloom => "Aurora bloom",
        VisualizerKind::ParticleField => "Particle field",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_accumulates_rather_than_overwrites() {
        let mut out = SettingsOutcome::default();
        assert!(!out.changed.0);

        RowResult(false).apply(&mut out);
        assert!(!out.changed.0);

        RowResult(true).apply(&mut out);
        assert!(out.changed.0);

        // A later unchanged row must not clear an earlier change.
        RowResult(false).apply(&mut out);
        assert!(out.changed.0, "a false row erased a previous true");
    }

    #[test]
    fn every_enum_variant_has_a_label() {
        for mode in [ThemeMode::Dark, ThemeMode::Light, ThemeMode::Adaptive] {
            assert!(!theme_label(mode).is_empty());
        }
        for g in [
            Grouping::Songs,
            Grouping::Artists,
            Grouping::Albums,
            Grouping::Genres,
            Grouping::Folders,
        ] {
            assert!(!grouping_label(g).is_empty());
        }
        for s in [
            SortKey::Title,
            SortKey::Artist,
            SortKey::Album,
            SortKey::Year,
            SortKey::Duration,
            SortKey::DateAdded,
            SortKey::PlayCount,
            SortKey::LastPlayed,
        ] {
            assert!(!sort_label(s).is_empty());
        }
        for k in [
            VisualizerKind::None,
            VisualizerKind::SpectrumBars,
            VisualizerKind::Oscilloscope,
            VisualizerKind::RadialSpectrum,
            VisualizerKind::WaveformRibbon,
            VisualizerKind::AuroraBloom,
            VisualizerKind::ParticleField,
        ] {
            assert!(!viz_label(k).is_empty());
        }
    }

    fn network(entries: usize, requests: usize) -> Network<'static> {
        Network {
            source: &mp_net::source::LRCLIB,
            entries,
            requests,
            log_path: None,
        }
    }

    /// The reassuring case, and the one a user checks first.
    #[test]
    fn a_build_that_has_done_nothing_says_so_plainly() {
        let summary = network(0, 0).summary();

        assert!(summary.contains("Nothing has been looked up"), "{summary}");
        assert!(
            summary.contains("nothing has left this machine"),
            "{summary}"
        );
    }

    /// Cache hits and skips are lookups, not requests. Reporting the larger
    /// number alone would overstate the traffic; reporting only the smaller
    /// would hide that the feature is being used at all.
    #[test]
    fn the_summary_separates_lookups_from_requests() {
        let summary = network(43, 2).summary();

        assert!(summary.contains("43 lookups"), "{summary}");
        assert!(summary.contains("2 of which were requests"), "{summary}");
    }

    #[test]
    fn a_session_that_only_used_the_cache_says_nothing_left() {
        let summary = network(12, 0).summary();

        assert!(
            summary.contains("none of which left this machine"),
            "{summary}"
        );
    }

    /// "1 lookups" and "1 of which were requests" both looked broken.
    #[test]
    fn the_summary_is_not_plural_about_one_of_anything() {
        let summary = network(1, 1).summary();

        assert!(summary.contains("1 lookup this session"), "{summary}");
        assert!(summary.contains("1 of which was a request"), "{summary}");
        assert!(!summary.contains("1 lookups"), "{summary}");
    }

    /// The Online section prints this straight from the source, so it has to
    /// name the actual fields rather than say "some metadata".
    #[test]
    fn the_source_says_specifically_what_it_sends() {
        let sends = mp_net::source::LRCLIB.sends;

        assert!(sends.contains("artist"), "{sends}");
        assert!(sends.contains("title"), "{sends}");
        assert!(sends.contains("album"), "{sends}");
        assert!(sends.contains("length"), "{sends}");
    }
}
