//! The tag editor: the only screen in Resonance that writes to your music.
//!
//! It is deliberately two steps. The first is an ordinary form. The second
//! shows exactly which fields will change, from what to what, and only then
//! offers to write. That second step is not ceremony — it is the difference
//! between "I meant to fix the year" and "I have just overwritten the artist
//! on a file I cannot get back", and it costs one click.
//!
//! The preview shown is computed by the same code that performs the write, so
//! there is no way for the two to disagree.

use mp_core::library::TrackId;
use mp_core::library::tags::{Change, Editable};

/// Which step of the editor the user is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Filling in the form.
    Editing,
    /// Looking at what will change, deciding whether to go ahead.
    Confirming,
}

/// The editor's state, held across frames while it is open.
#[derive(Debug, Default)]
pub struct TagEditor {
    open_for: Option<TrackId>,
    /// What the file is called, for the dialog's heading.
    title: String,
    /// The file itself, so the user can see what they are about to change.
    path: String,

    /// One text buffer per field, in [`Editable::ALL`] order.
    fields: Vec<(Editable, String)>,
    /// What the file held when the editor opened, for the reset button.
    original: Vec<(Editable, String)>,

    step: Option<Step>,
    /// The change list, once the user has asked to see it.
    pending: Vec<Change>,
    /// A failure to report — a locked file, a format with nowhere to put a
    /// field. Cleared when the user changes anything.
    error: Option<String>,
}

impl TagEditor {
    pub fn is_open(&self) -> bool {
        self.open_for.is_some()
    }

    pub fn track(&self) -> Option<TrackId> {
        self.open_for
    }

    /// Open on a track, seeded with what the file currently holds.
    pub fn open(
        &mut self,
        track: TrackId,
        title: &str,
        path: &str,
        values: &[(Editable, Option<String>)],
    ) {
        let fields: Vec<(Editable, String)> = Editable::ALL
            .iter()
            .map(|field| {
                let value = values
                    .iter()
                    .find(|(other, _)| other == field)
                    .and_then(|(_, value)| value.clone())
                    .unwrap_or_default();
                (*field, value)
            })
            .collect();

        self.open_for = Some(track);
        self.title = title.to_owned();
        self.path = path.to_owned();
        self.original = fields.clone();
        self.fields = fields;
        self.step = Some(Step::Editing);
        self.pending.clear();
        self.error = None;
    }

    pub fn close(&mut self) {
        *self = Self::default();
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn fields_mut(&mut self) -> &mut [(Editable, String)] {
        &mut self.fields
    }

    pub fn is_confirming(&self) -> bool {
        self.step == Some(Step::Confirming)
    }

    pub fn pending(&self) -> &[Change] {
        &self.pending
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether anything in the form differs from what was loaded.
    ///
    /// Drives whether Save is offered at all, so a dialog opened by accident
    /// cannot write anything however many times it is clicked.
    pub fn is_dirty(&self) -> bool {
        self.fields
            .iter()
            .zip(&self.original)
            .any(|((_, now), (_, before))| now.trim() != before.trim())
    }

    /// Put every field back to what the file holds.
    pub fn reset(&mut self) {
        self.fields = self.original.clone();
        self.error = None;
    }

    /// Turn the form into an edit the library can apply.
    pub fn edit(&self) -> mp_core::library::tags::Edit {
        let mut edit = mp_core::library::tags::Edit::default();

        for ((field, now), (_, before)) in self.fields.iter().zip(&self.original) {
            // Only fields the user actually touched are included, so an
            // untouched field can never be rewritten — not even to the same
            // value it already had.
            if now.trim() == before.trim() {
                continue;
            }

            edit = if now.trim().is_empty() {
                edit.clear(*field)
            } else {
                edit.set(*field, now.as_str())
            };
        }

        edit
    }

    /// Move to the confirmation step with the changes to be shown.
    pub fn confirm_with(&mut self, changes: Vec<Change>) {
        self.pending = changes;
        self.step = Some(Step::Confirming);
        self.error = None;
    }

    /// Go back to the form.
    pub fn back(&mut self) {
        self.step = Some(Step::Editing);
        self.pending.clear();
    }

    pub fn fail(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
        self.step = Some(Step::Editing);
        self.pending.clear();
    }

    /// Note that the file now holds what the form says, without closing.
    ///
    /// Used after a successful write so the dialog is no longer dirty and a
    /// second Save cannot re-apply the same edit.
    pub fn settle(&mut self) {
        self.original = self.fields.clone();
        self.pending.clear();
        self.step = Some(Step::Editing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(pairs: &[(Editable, &str)]) -> Vec<(Editable, Option<String>)> {
        Editable::ALL
            .iter()
            .map(|field| {
                let value = pairs
                    .iter()
                    .find(|(other, _)| other == field)
                    .map(|(_, value)| (*value).to_owned());
                (*field, value)
            })
            .collect()
    }

    fn opened() -> TagEditor {
        let mut editor = TagEditor::default();
        editor.open(
            7,
            "A Song",
            "C:/music/a.mp3",
            &values(&[(Editable::Title, "A Song"), (Editable::Artist, "Someone")]),
        );
        editor
    }

    #[test]
    fn it_opens_seeded_with_what_the_file_holds() {
        let editor = opened();

        assert!(editor.is_open());
        assert_eq!(editor.track(), Some(7));
        assert_eq!(editor.fields.len(), Editable::ALL.len());

        let title = editor
            .fields
            .iter()
            .find(|(field, _)| *field == Editable::Title)
            .unwrap();
        assert_eq!(title.1, "A Song");
    }

    /// A dialog nobody has typed in must not be able to write anything.
    #[test]
    fn a_freshly_opened_editor_is_not_dirty_and_produces_no_edit() {
        let editor = opened();

        assert!(!editor.is_dirty());
        assert!(editor.edit().is_empty());
    }

    #[test]
    fn typing_makes_it_dirty_and_produces_exactly_that_field() {
        let mut editor = opened();

        for (field, value) in editor.fields_mut() {
            if *field == Editable::Album {
                *value = "New Album".into();
            }
        }

        assert!(editor.is_dirty());

        let edit = editor.edit();
        assert_eq!(edit.fields.len(), 1, "only the touched field");
        assert_eq!(edit.fields[0].0, Editable::Album);
        assert_eq!(edit.fields[0].1.as_deref(), Some("New Album"));
    }

    /// Emptying a field that had a value is a real instruction, not a no-op.
    #[test]
    fn clearing_a_field_produces_a_clear() {
        let mut editor = opened();

        for (field, value) in editor.fields_mut() {
            if *field == Editable::Artist {
                value.clear();
            }
        }

        let edit = editor.edit();
        assert_eq!(edit.fields.len(), 1);
        assert_eq!(edit.fields[0], (Editable::Artist, None));
    }

    /// Retyping the same value differently spaced is not an edit.
    #[test]
    fn whitespace_only_differences_are_not_edits() {
        let mut editor = opened();

        for (field, value) in editor.fields_mut() {
            if *field == Editable::Title {
                *value = "  A Song  ".into();
            }
        }

        assert!(!editor.is_dirty());
        assert!(editor.edit().is_empty());
    }

    #[test]
    fn reset_puts_everything_back() {
        let mut editor = opened();

        for (_, value) in editor.fields_mut() {
            value.push_str(" ruined");
        }
        assert!(editor.is_dirty());

        editor.reset();
        assert!(!editor.is_dirty());
    }

    #[test]
    fn confirming_and_going_back_moves_between_the_two_steps() {
        let mut editor = opened();
        assert!(!editor.is_confirming());

        editor.confirm_with(vec![Change {
            field: Editable::Title,
            before: Some("A Song".into()),
            after: Some("Another".into()),
        }]);

        assert!(editor.is_confirming());
        assert_eq!(editor.pending().len(), 1);

        editor.back();
        assert!(!editor.is_confirming());
        assert!(editor.pending().is_empty());
    }

    /// After a write the form matches the file again, so clicking Save twice
    /// cannot apply the same change twice.
    #[test]
    fn settling_after_a_write_clears_the_dirty_state() {
        let mut editor = opened();

        for (field, value) in editor.fields_mut() {
            if *field == Editable::Genre {
                *value = "Shoegaze".into();
            }
        }
        assert!(editor.is_dirty());

        editor.settle();

        assert!(!editor.is_dirty());
        assert!(editor.edit().is_empty());
        assert!(!editor.is_confirming());
    }

    #[test]
    fn a_failure_returns_to_the_form_with_the_reason() {
        let mut editor = opened();
        editor.confirm_with(vec![]);

        editor.fail("the file is read-only");

        assert!(!editor.is_confirming());
        assert_eq!(editor.error(), Some("the file is read-only"));
        assert!(editor.pending().is_empty());
    }

    #[test]
    fn closing_forgets_everything() {
        let mut editor = opened();
        editor.close();

        assert!(!editor.is_open());
        assert!(editor.track().is_none());
        assert!(!editor.is_dirty());
    }
}
