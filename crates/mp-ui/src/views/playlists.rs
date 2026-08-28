//! The playlists view, and the builder tools that live inside it.
//!
//! Two screens in one: a list of playlists, and — once one is opened — its
//! tracks alongside whichever builder tool is open. The tools are here rather
//! than in a separate place because they are only ever used *on* a playlist,
//! and a builder you have to navigate away to reach is one that gets used once.
//!
//! Nothing here touches the database. Every action is reported back through
//! [`Outcome`] and applied by the caller, which is what keeps a slider drag or
//! a mis-click from writing to disk mid-frame.

use egui::{RichText, TextStyle, Ui};
use mp_core::library::model::{Order, TrackId};
use mp_core::library::smart::{Field, Match, Node, Op, Rule};
use mp_core::library::{Playlist, PlaylistId};

use crate::playlists::{PlaylistState, Tool};
use crate::theme::{Theme, col, col_alpha};
use crate::widgets::{self, icons::Icon};

/// What the view wants the caller to do.
#[derive(Debug, Default)]
pub struct Outcome {
    pub create: bool,
    pub create_smart: bool,
    pub open: Option<PlaylistId>,
    pub close: bool,
    pub delete: Option<PlaylistId>,
    pub rename: Option<(PlaylistId, String)>,

    /// Play the open playlist, starting at this index.
    pub play_from: Option<usize>,
    pub remove_at: Option<usize>,
    /// Move the item at `.0` to `.1`.
    pub move_item: Option<(usize, usize)>,

    pub set_tool: Option<Tool>,
    /// The library browser's search text changed.
    pub set_query: Option<String>,
    /// The library browser's folder filter changed. Outer `Option` is "was it
    /// touched", inner is "to what" — `Some(None)` means "all folders".
    pub set_folder: Option<Option<std::path::PathBuf>>,
    pub toggle_pick: Option<TrackId>,
    pub pick_all: bool,
    pub clear_picks: bool,
    pub add_picked: bool,
    pub refresh_suggestions: bool,

    /// The rule draft was edited.
    pub rules_edited: bool,
    pub apply_rules: bool,

    /// Write the open playlist out as an M3U8 file.
    pub export: Option<PlaylistId>,
    /// Read an M3U8 file in as a new playlist.
    pub import: bool,
}

/// Scratch state the view owns between frames.
#[derive(Debug, Default)]
pub struct Editing {
    /// The playlist whose name is being edited, and the text so far.
    pub renaming: Option<(PlaylistId, String)>,
}

pub fn show(
    ui: &mut Ui,
    theme: &Theme,
    state: &mut PlaylistState,
    editing: &mut Editing,
) -> Outcome {
    let mut outcome = Outcome::default();

    // Lifted out so the rule editor can hold it mutably while everything else
    // reads the state. Put back below, whatever happens in between.
    let mut draft = state.take_draft();

    {
        let state: &PlaylistState = state;

        match state.open_playlist() {
            Some(playlist) => detail(
                ui,
                theme,
                state,
                playlist,
                editing,
                draft.as_mut(),
                &mut outcome,
            ),
            None => index(ui, theme, state, editing, &mut outcome),
        }
    }

    state.put_draft(draft);
    outcome
}

// ---------------------------------------------------------------------------
// The list
// ---------------------------------------------------------------------------

fn index(
    ui: &mut Ui,
    theme: &Theme,
    state: &PlaylistState,
    editing: &mut Editing,
    outcome: &mut Outcome,
) {
    let m = theme.metrics;
    let p = theme.palette;

    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Playlists")
                .text_style(TextStyle::Heading)
                .color(col(p.text_primary)),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::accent_button(ui, theme, "New playlist").clicked() {
                outcome.create = true;
            }
            ui.add_space(m.space(0.75));
            if ui
                .button("New smart playlist")
                .on_hover_text("A playlist described by rules, which keeps itself up to date")
                .clicked()
            {
                outcome.create_smart = true;
            }

            ui.add_space(m.space(0.75));
            if ui
                .button("Import")
                .on_hover_text("Read an .m3u or .m3u8 playlist file")
                .clicked()
            {
                outcome.import = true;
            }
        });
    });

    ui.add_space(m.space(1.5));

    if state.playlists().is_empty() {
        widgets::empty_state(
            ui,
            theme,
            Icon::Playlists,
            "No playlists yet",
            "Make one, then use the builder to fill it from your library.",
        );
        return;
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for playlist in state.playlists() {
                row(ui, theme, playlist, editing, outcome);
            }
        });
}

fn row(
    ui: &mut Ui,
    theme: &Theme,
    playlist: &Playlist,
    editing: &mut Editing,
    outcome: &mut Outcome,
) {
    let m = theme.metrics;
    let p = theme.palette;

    let renaming = editing
        .renaming
        .as_ref()
        .is_some_and(|(id, _)| *id == playlist.id);

    egui::Frame::new()
        .fill(theme.card_fill())
        .corner_radius(egui::CornerRadius::same(m.radius_medium))
        .inner_margin(egui::Margin::same(m.space(1.25) as i8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if renaming {
                    // Borrowed only for the edit, so the rest of the row can
                    // still read the playlist it belongs to.
                    let Some((_, text)) = editing.renaming.as_mut() else {
                        return;
                    };

                    let response = ui.add(
                        egui::TextEdit::singleline(text).desired_width(m.space(20.0)),
                    );
                    response.request_focus();

                    let confirmed = response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));

                    if confirmed {
                        outcome.rename = Some((playlist.id, text.clone()));
                        editing.renaming = None;
                    } else if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                        editing.renaming = None;
                    }
                } else {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(&playlist.name)
                                    .text_style(TextStyle::Name("title".into()))
                                    .color(col(p.text_primary)),
                            );

                            // Smart playlists are marked, because "why did this
                            // change on its own" is otherwise a real puzzle.
                            if playlist.is_smart() {
                                ui.label(
                                    RichText::new("smart")
                                        .text_style(TextStyle::Name("caption".into()))
                                        .color(col(p.accent)),
                                )
                                .on_hover_text("Follows its rules, so its contents change as your library does");
                            }
                        });

                        ui.label(
                            RichText::new(playlist.subtitle())
                                .text_style(TextStyle::Name("caption".into()))
                                .color(col(p.text_muted)),
                        );
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if widgets::icon_button_labelled(
                            ui,
                            theme,
                            Icon::Close,
                            m.space(2.5),
                            false,
                            "Delete this playlist",
                        )
                        .clicked()
                        {
                            outcome.delete = Some(playlist.id);
                        }

                        if ui.button("Rename").clicked() {
                            editing.renaming = Some((playlist.id, playlist.name.clone()));
                        }

                        if widgets::accent_button(ui, theme, "Open").clicked() {
                            outcome.open = Some(playlist.id);
                        }
                    });
                }
            });
        });

    ui.add_space(m.space(0.75));
}

// ---------------------------------------------------------------------------
// One playlist
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn detail(
    ui: &mut Ui,
    theme: &Theme,
    state: &PlaylistState,
    playlist: &Playlist,
    editing: &mut Editing,
    draft: Option<&mut mp_core::library::SmartRules>,
    outcome: &mut Outcome,
) {
    let m = theme.metrics;
    let p = theme.palette;
    let _ = editing;

    ui.horizontal(|ui| {
        if widgets::icon_button_labelled(
            ui,
            theme,
            Icon::ChevronLeft,
            m.space(3.0),
            false,
            "Back to playlists",
        )
        .clicked()
        {
            outcome.close = true;
        }

        ui.vertical(|ui| {
            ui.label(
                RichText::new(&playlist.name)
                    .text_style(TextStyle::Heading)
                    .color(col(p.text_primary)),
            );
            ui.label(
                RichText::new(playlist.subtitle())
                    .text_style(TextStyle::Name("caption".into()))
                    .color(col(p.text_muted)),
            );
        });

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // A smart playlist has no stored order to edit, so it offers rules
            // instead of the reorder controls the track list would show.
            if playlist.is_smart()
                && ui
                    .selectable_label(state.tool() == Tool::Rules, "Rules")
                    .clicked()
            {
                outcome.set_tool = Some(Tool::Rules);
            }

            if ui
                .selectable_label(state.tool() == Tool::Similar, "Add similar")
                .on_hover_text("Find more of the same, from your own library")
                .clicked()
            {
                outcome.set_tool = Some(Tool::Similar);
            }

            // A stored playlist can be filled by hand; a smart one is filled
            // by its rules, so offering a browser there would be a button that
            // silently did nothing.
            if !playlist.is_smart()
                && ui
                    .selectable_label(state.tool() == Tool::Library, "Add tracks")
                    .on_hover_text("Browse your library and pick what to add")
                    .clicked()
            {
                outcome.set_tool = Some(Tool::Library);
            }

            ui.add_space(m.space(0.5));

            if !state.tracks().is_empty() && widgets::accent_button(ui, theme, "Play").clicked() {
                outcome.play_from = Some(0);
            }

            ui.add_space(m.space(0.5));

            // Exporting an empty playlist writes a file with nothing in it,
            // which is a confusing thing to be handed.
            if !state.tracks().is_empty()
                && ui
                    .button("Export")
                    .on_hover_text(if playlist.is_smart() {
                        "Save what these rules currently match as an .m3u8 file"
                    } else {
                        "Save as an .m3u8 file other players can read"
                    })
                    .clicked()
            {
                outcome.export = Some(playlist.id);
            }
        });
    });

    ui.add_space(m.space(1.25));
    widgets::separator(ui, theme);
    ui.add_space(m.space(1.0));

    // The tool panel and the track list share what is left, rather than the
    // panel taking a fixed slice. A fixed one showed three rows of a
    // three-hundred-track browser with half the view empty beneath it, which is
    // the wrong way round: the tool is what the user is looking at.
    //
    // An empty playlist needs almost no room for its list, so the panel takes
    // nearly everything; a full one splits the difference.
    let available = ui.available_height();
    let share = if state.tracks().is_empty() {
        0.82
    } else {
        0.55
    };
    let panel_height = (available * share).max(m.space(14.0));

    match state.tool() {
        Tool::Library => browse_panel(ui, theme, state, outcome, panel_height),
        Tool::Similar => similar_panel(ui, theme, state, outcome, panel_height),
        Tool::Rules => rules_panel(ui, theme, draft, outcome),
        Tool::None => {}
    }

    if state.tool() != Tool::None {
        ui.add_space(m.space(1.0));
        widgets::separator(ui, theme);
        ui.add_space(m.space(1.0));
    }

    track_list(ui, theme, state, playlist, outcome);
}

fn track_list(
    ui: &mut Ui,
    theme: &Theme,
    state: &PlaylistState,
    playlist: &Playlist,
    outcome: &mut Outcome,
) {
    let m = theme.metrics;
    let p = theme.palette;

    if state.tracks().is_empty() {
        // With a tool open the big empty state is redundant — the way to fill
        // the playlist is on screen already — and it costs the panel the room
        // it needs. One line is enough.
        if state.tool() != Tool::None {
            ui.label(
                RichText::new("Nothing in this playlist yet.")
                    .text_style(TextStyle::Name("caption".into()))
                    .color(col(p.text_muted)),
            );
            return;
        }

        let body = if playlist.is_smart() {
            "No track matches these rules yet. Loosen them, or add a rule."
        } else {
            // Names the button that exists. An earlier version invited the
            // user to "add tracks from the library" when there was no way to
            // do so, which is the interface promising something it cannot do.
            "Nothing here yet. Use \"Add tracks\" to pick from your library."
        };

        widgets::empty_state(ui, theme, Icon::Playlists, "Empty playlist", body);
        return;
    }

    let reorderable = !playlist.is_smart();
    let count = state.tracks().len();

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (index, track) in state.tracks().iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{:>3}", index + 1))
                            .text_style(TextStyle::Name("caption".into()))
                            .color(col(p.text_muted)),
                    );
                    ui.add_space(m.space(0.5));

                    ui.vertical(|ui| {
                        ui.label(RichText::new(&track.title).color(col(p.text_primary)));
                        ui.label(
                            RichText::new(track.subtitle())
                                .text_style(TextStyle::Name("caption".into()))
                                .color(col(p.text_muted)),
                        );
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if reorderable
                            && widgets::icon_button_labelled(
                                ui,
                                theme,
                                Icon::Close,
                                m.space(2.25),
                                false,
                                "Remove from this playlist",
                            )
                            .clicked()
                        {
                            outcome.remove_at = Some(index);
                        }

                        if reorderable {
                            // Buttons rather than drag-and-drop. A drag in a
                            // virtualised scrolling list is a great deal of
                            // fiddly state to get subtly wrong, and two arrows
                            // do the same job with no ambiguity about where an
                            // item will land.
                            ui.add_enabled_ui(index + 1 < count, |ui| {
                                if ui.small_button("▼").on_hover_text("Move down").clicked() {
                                    outcome.move_item = Some((index, index + 1));
                                }
                            });
                            ui.add_enabled_ui(index > 0, |ui| {
                                if ui.small_button("▲").on_hover_text("Move up").clicked() {
                                    outcome.move_item = Some((index, index - 1));
                                }
                            });
                        }

                        if let Some(duration) = track.duration {
                            ui.label(
                                RichText::new(widgets::format_duration(duration.as_secs_f64()))
                                    .text_style(TextStyle::Name("caption".into()))
                                    .color(col(p.text_muted)),
                            );
                        }

                        if ui
                            .small_button("Play")
                            .on_hover_text("Play the playlist from here")
                            .clicked()
                        {
                            outcome.play_from = Some(index);
                        }
                    });
                });

                ui.add_space(m.space(0.4));
            }
        });
}

// ---------------------------------------------------------------------------
// Add from the library
// ---------------------------------------------------------------------------

fn browse_panel(
    ui: &mut Ui,
    theme: &Theme,
    state: &PlaylistState,
    outcome: &mut Outcome,
    max_height: f32,
) {
    let m = theme.metrics;
    let p = theme.palette;

    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Add tracks")
                .text_style(TextStyle::Name("title".into()))
                .color(col(p.text_primary)),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let picked = state.picked_count();

            ui.add_enabled_ui(picked > 0, |ui| {
                if widgets::accent_button(ui, theme, &format!("Add {picked}")).clicked() {
                    outcome.add_picked = true;
                }
            });

            if picked > 0 && ui.button("None").clicked() {
                outcome.clear_picks = true;
            }
            if !state.candidates().is_empty() && ui.button("All").clicked() {
                outcome.pick_all = true;
            }
        });
    });

    ui.add_space(m.space(0.75));

    ui.horizontal(|ui| {
        let mut query = state.query().to_owned();
        if ui
            .add(
                egui::TextEdit::singleline(&mut query)
                    .desired_width(m.space(18.0))
                    .hint_text("Search your library"),
            )
            .changed()
        {
            outcome.set_query = Some(query);
        }

        ui.add_space(m.space(0.75));

        // The plan's "add from folder": a folder is how most collections are
        // actually organised, so filtering by one is the fastest way to add an
        // album's worth at a time.
        let current = state
            .folder_filter()
            .map(|folder| {
                folder
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| folder.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "All folders".to_owned());

        egui::ComboBox::from_id_salt("browse_folder")
            .selected_text(current)
            .width(m.space(14.0))
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(state.folder_filter().is_none(), "All folders")
                    .clicked()
                {
                    outcome.set_folder = Some(None);
                }

                for folder in state.folders() {
                    let selected = state.folder_filter() == Some(folder.path.as_path());
                    if ui
                        .selectable_label(
                            selected,
                            format!("{} ({})", folder.name, folder.track_count),
                        )
                        .clicked()
                    {
                        outcome.set_folder = Some(Some(folder.path.clone()));
                    }
                }
            });
    });

    ui.add_space(m.space(0.5));

    if state.candidates().is_empty() {
        ui.label(
            RichText::new("Nothing matches.")
                .text_style(TextStyle::Name("caption".into()))
                .color(col(p.text_muted)),
        );
        return;
    }

    egui::ScrollArea::vertical()
        .id_salt("browse")
        .max_height(max_height)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for track in state.candidates() {
                ui.horizontal(|ui| {
                    let mut picked = state.is_picked(track.id);
                    if ui.checkbox(&mut picked, "").changed() {
                        outcome.toggle_pick = Some(track.id);
                    }

                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&track.title).color(col(p.text_primary)));

                            // Adding a track twice is legitimate but rarely
                            // intended, so it is marked rather than blocked.
                            if state.already_holds(track.id) {
                                ui.label(
                                    RichText::new("already in")
                                        .text_style(TextStyle::Name("caption".into()))
                                        .color(col_alpha(p.text_muted, 0.9)),
                                );
                            }
                        });

                        ui.label(
                            RichText::new(track.subtitle())
                                .text_style(TextStyle::Name("caption".into()))
                                .color(col(p.text_muted)),
                        );
                    });
                });

                ui.add_space(m.space(0.3));
            }
        });

    if state.is_truncated() {
        ui.add_space(m.space(0.4));
        ui.label(
            RichText::new(format!(
                "Showing the first {}. Search or pick a folder to narrow it down.",
                crate::playlists::BROWSE_LIMIT
            ))
            .text_style(TextStyle::Name("caption".into()))
            .color(col(p.text_muted)),
        );
    }
}

// ---------------------------------------------------------------------------
// Add similar
// ---------------------------------------------------------------------------

fn similar_panel(
    ui: &mut Ui,
    theme: &Theme,
    state: &PlaylistState,
    outcome: &mut Outcome,
    max_height: f32,
) {
    let m = theme.metrics;
    let p = theme.palette;

    ui.horizontal(|ui| {
        ui.label(
            RichText::new("More like this")
                .text_style(TextStyle::Name("title".into()))
                .color(col(p.text_primary)),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let picked = state.picked_count();

            ui.add_enabled_ui(picked > 0, |ui| {
                if widgets::accent_button(ui, theme, &format!("Add {picked}")).clicked() {
                    outcome.add_picked = true;
                }
            });

            if ui.button("Refresh").clicked() {
                outcome.refresh_suggestions = true;
            }
            if picked > 0 && ui.button("None").clicked() {
                outcome.clear_picks = true;
            }
            if !state.suggestions().is_empty() && ui.button("All").clicked() {
                outcome.pick_all = true;
            }
        });
    });

    ui.add_space(m.space(0.5));

    if state.suggestions().is_empty() {
        ui.label(
            RichText::new(
                "Nothing to suggest yet. Add a track or two to this playlist first — \
                 the suggestions are drawn from what is already in it.",
            )
            .text_style(TextStyle::Name("caption".into()))
            .color(col(p.text_muted)),
        );
        return;
    }

    egui::ScrollArea::vertical()
        .id_salt("suggestions")
        .max_height(max_height)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for suggestion in state.suggestions() {
                ui.horizontal(|ui| {
                    let mut picked = state.is_picked(suggestion.track.id);
                    if ui.checkbox(&mut picked, "").changed() {
                        outcome.toggle_pick = Some(suggestion.track.id);
                    }

                    ui.vertical(|ui| {
                        ui.label(RichText::new(&suggestion.track.title).color(col(p.text_primary)));

                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(suggestion.track.subtitle())
                                    .text_style(TextStyle::Name("caption".into()))
                                    .color(col(p.text_muted)),
                            );

                            // The reason chips: a recommendation you cannot
                            // interrogate is one you cannot correct.
                            for reason in &suggestion.reasons {
                                ui.label(
                                    RichText::new(reason.chip())
                                        .text_style(TextStyle::Name("caption".into()))
                                        .color(col_alpha(p.accent, 0.9)),
                                );
                            }
                        });
                    });
                });

                ui.add_space(m.space(0.4));
            }
        });
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

fn rules_panel(
    ui: &mut Ui,
    theme: &Theme,
    draft: Option<&mut mp_core::library::SmartRules>,
    outcome: &mut Outcome,
) {
    let m = theme.metrics;
    let p = theme.palette;

    let Some(rules) = draft else {
        return;
    };

    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Rules")
                .text_style(TextStyle::Name("title".into()))
                .color(col(p.text_primary)),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::accent_button(ui, theme, "Apply").clicked() {
                outcome.apply_rules = true;
            }
        });
    });

    ui.add_space(m.space(0.5));

    if rules.root.nodes.is_empty() {
        ui.label(
            RichText::new(
                "No rules yet, so this matches your whole library. \
                 Add one to narrow it down.",
            )
            .text_style(TextStyle::Name("caption".into()))
            .color(col(p.warning)),
        );
        ui.add_space(m.space(0.5));
    }

    if rule_editor(ui, theme, rules) {
        outcome.rules_edited = true;
    }
}

/// The editable rule rows.
///
/// Returns whether anything was changed. Nothing is written until Apply — a
/// live-applied half-typed rule would keep emptying the playlist as the user
/// typed into it.
fn rule_editor(ui: &mut Ui, theme: &Theme, rules: &mut mp_core::library::SmartRules) -> bool {
    let m = theme.metrics;
    let mut changed = false;

    ui.horizontal(|ui| {
        let mut all = rules.root.matching == Match::All;
        if ui
            .selectable_label(all, "Match all")
            .on_hover_text("Every rule must be true")
            .clicked()
        {
            all = true;
            changed = true;
        }
        if ui
            .selectable_label(!all, "Match any")
            .on_hover_text("Any one rule is enough")
            .clicked()
        {
            all = false;
            changed = true;
        }
        rules.root.matching = if all { Match::All } else { Match::Any };
    });

    ui.add_space(m.space(0.5));

    let mut remove = None;

    for (index, node) in rules.root.nodes.iter_mut().enumerate() {
        // Nested groups are stored and evaluated, but the editor only offers
        // flat rules for now — a tree editor is a lot of interface for a case
        // that a second smart playlist usually covers better.
        let Node::Rule(rule) = node else {
            continue;
        };

        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt(("rule_field", index))
                .selected_text(rule.field.label())
                .width(m.space(11.0))
                .show_ui(ui, |ui| {
                    for field in Field::ALL {
                        if ui
                            .selectable_value(&mut rule.field, field, field.label())
                            .changed()
                        {
                            changed = true;
                        }
                    }
                });

            // The operator list depends on the field, so a field change can
            // leave an operator that no longer applies. Snapping it back to a
            // valid one keeps the rule meaningful.
            let allowed = Op::for_kind(rule.field.kind());
            if !allowed.contains(&rule.op) {
                rule.op = allowed[0];
                changed = true;
            }

            egui::ComboBox::from_id_salt(("rule_op", index))
                .selected_text(rule.op.label())
                .width(m.space(10.0))
                .show_ui(ui, |ui| {
                    for op in allowed {
                        if ui.selectable_value(&mut rule.op, *op, op.label()).changed() {
                            changed = true;
                        }
                    }
                });

            if rule.op.takes_a_value()
                && ui
                    .add(
                        egui::TextEdit::singleline(&mut rule.value)
                            .desired_width(m.space(12.0))
                            .hint_text(hint_for(rule.field)),
                    )
                    .changed()
            {
                changed = true;
            }

            if ui
                .small_button("✕")
                .on_hover_text("Remove this rule")
                .clicked()
            {
                remove = Some(index);
            }
        });

        ui.add_space(m.space(0.3));
    }

    if let Some(index) = remove {
        rules.root.nodes.remove(index);
        changed = true;
    }

    ui.add_space(m.space(0.5));

    ui.horizontal(|ui| {
        if ui.button("Add rule").clicked() {
            rules.root.nodes.push(Node::Rule(Rule::new(
                Field::Artist,
                Op::Contains,
                String::new(),
            )));
            changed = true;
        }

        ui.add_space(m.space(1.0));

        let mut limited = rules.limit.is_some();
        if ui.checkbox(&mut limited, "Limit to").changed() {
            rules.limit = if limited { Some(50) } else { None };
            changed = true;
        }

        if let Some(limit) = rules.limit.as_mut() {
            if ui
                .add(egui::DragValue::new(limit).range(1..=1000))
                .changed()
            {
                changed = true;
            }

            ui.label("by");

            egui::ComboBox::from_id_salt("rule_order")
                .selected_text(rules.order.label())
                .width(m.space(9.0))
                .show_ui(ui, |ui| {
                    for order in Order::ALL {
                        if ui
                            .selectable_value(&mut rules.order, order, order.label())
                            .changed()
                        {
                            changed = true;
                        }
                    }
                });
        }
    });

    changed
}

/// A placeholder that shows what the field expects.
fn hint_for(field: Field) -> &'static str {
    use mp_core::library::smart::Kind;

    match field.kind() {
        Kind::Text => "text",
        Kind::Number => match field {
            Field::Duration => "seconds",
            Field::Year => "e.g. 1995",
            _ => "a number",
        },
        Kind::Date => "days",
        Kind::Flag => "true or false",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field must offer a hint, or a text box appears with no indication
    /// of whether it wants seconds, days or words.
    #[test]
    fn every_field_has_a_hint() {
        for field in Field::ALL {
            assert!(!hint_for(field).is_empty(), "{field:?} has no hint");
        }
    }

    /// The editor snaps an operator back when the field changes under it. That
    /// only works if every field's operator list is non-empty.
    #[test]
    fn every_field_has_at_least_one_operator_to_snap_to() {
        for field in Field::ALL {
            assert!(
                !Op::for_kind(field.kind()).is_empty(),
                "{field:?} has no operators, so the editor could not repair a rule"
            );
        }
    }

    #[test]
    fn a_fresh_outcome_asks_for_nothing() {
        let outcome = Outcome::default();

        assert!(!outcome.create);
        assert!(outcome.open.is_none());
        assert!(outcome.play_from.is_none());
        assert!(!outcome.apply_rules);
    }
}
