//! Built-in equalizer presets.
//!
//! Each carries its own preamp rather than leaving the user to discover that a
//! boosted curve clips: a preset that sounds broken the moment it is selected
//! is worse than no preset at all.
//!
//! Those preamps are *measured*, not estimated. Neighbouring bands overlap, so
//! a curve's peak is not its loudest slider — Bass Boost tops out at +6.1 dB
//! from a highest band of +6.0, and Podcast at +4.7 dB from a highest band of
//! +3.5. `cargo run -p mp-audio --example preset_headroom` prints the composite
//! peak of every preset, and `no_preset_clips_when_selected` holds them to it.
//!
//! The curves are conservative. A preset is a starting point someone adjusts,
//! and an extreme one is harder to walk back from than a mild one.

use crate::dsp::eq::BAND_COUNT;

/// A named equalizer curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Preset {
    pub name: &'static str,
    /// One gain per band, in decibels, in [`BAND_FREQUENCIES`] order.
    ///
    /// [`BAND_FREQUENCIES`]: crate::dsp::eq::BAND_FREQUENCIES
    pub gains_db: [f32; BAND_COUNT],
    /// Applied ahead of the bands, so the preset does not clip on selection.
    pub preamp_db: f32,
    /// One line explaining what it is for.
    pub description: &'static str,
}

impl Preset {
    pub fn gains(&self) -> Vec<f32> {
        self.gains_db.to_vec()
    }

    /// Whether a set of gains matches this preset, so the UI can show which
    /// one is selected after a settings file is loaded.
    pub fn matches(&self, gains_db: &[f32], preamp_db: f32) -> bool {
        if gains_db.len() != BAND_COUNT {
            return false;
        }
        (preamp_db - self.preamp_db).abs() < 0.05
            && gains_db
                .iter()
                .zip(self.gains_db.iter())
                .all(|(a, b)| (a - b).abs() < 0.05)
    }
}

//                       31.5  63   125  250  500   1k    2k    4k    8k   16k
pub const FLAT: Preset = Preset {
    name: "Flat",
    gains_db: [0.0; BAND_COUNT],
    preamp_db: 0.0,
    description: "No colouration at all",
};

pub const BASS_BOOST: Preset = Preset {
    name: "Bass Boost",
    gains_db: [6.0, 5.0, 3.5, 1.5, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    preamp_db: -6.5,
    description: "Weight and depth, for small speakers and headphones",
};

pub const BASS_CUT: Preset = Preset {
    name: "Bass Cut",
    gains_db: [-6.0, -5.0, -3.0, -1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    preamp_db: 0.0,
    description: "Tames boomy rooms and desk resonance",
};

pub const VOCAL: Preset = Preset {
    name: "Vocal",
    gains_db: [-2.0, -1.5, 0.0, 1.0, 2.5, 3.5, 3.0, 1.5, 0.0, -1.0],
    preamp_db: -5.0,
    description: "Brings voices forward out of a dense mix",
};

pub const ROCK: Preset = Preset {
    name: "Rock",
    gains_db: [4.0, 3.0, 1.0, -0.5, -1.5, 0.0, 1.5, 3.0, 3.5, 3.0],
    preamp_db: -4.5,
    description: "Scooped middle, lifted ends",
};

pub const ELECTRONIC: Preset = Preset {
    name: "Electronic",
    gains_db: [5.0, 4.0, 1.5, 0.0, -1.0, 0.5, 1.0, 2.5, 4.0, 4.5],
    preamp_db: -5.0,
    description: "Sub weight and air, for produced music",
};

pub const ACOUSTIC: Preset = Preset {
    name: "Acoustic",
    gains_db: [2.5, 2.0, 1.0, 0.0, 0.5, 1.0, 1.5, 2.0, 2.0, 1.0],
    preamp_db: -3.0,
    description: "A gentle lift at both ends for unamplified music",
};

pub const PODCAST: Preset = Preset {
    name: "Podcast",
    gains_db: [-6.0, -4.0, -1.0, 2.0, 3.5, 3.5, 2.5, 1.0, -1.0, -3.0],
    preamp_db: -5.0,
    description: "Speech clarity, with rumble and hiss removed",
};

pub const LOUDNESS: Preset = Preset {
    name: "Loudness",
    gains_db: [6.0, 4.5, 2.0, 0.0, -1.0, -1.5, -1.0, 1.0, 3.5, 5.0],
    preamp_db: -5.5,
    description: "Compensates for quiet listening, where the ear loses the ends",
};

pub const NIGHT: Preset = Preset {
    name: "Late Night",
    gains_db: [-4.0, -3.0, -1.0, 1.0, 2.0, 2.0, 1.5, 0.5, -1.0, -2.0],
    preamp_db: -3.0,
    description: "Keeps detail audible without the bass carrying through walls",
};

/// Every built-in preset, in the order the UI should offer them.
pub const ALL: &[Preset] = &[
    FLAT, BASS_BOOST, BASS_CUT, VOCAL, ROCK, ELECTRONIC, ACOUSTIC, PODCAST, LOUDNESS, NIGHT,
];

/// Find a preset by name.
pub fn by_name(name: &str) -> Option<&'static Preset> {
    ALL.iter()
        .find(|preset| preset.name.eq_ignore_ascii_case(name))
}

/// The preset matching these settings, if any.
pub fn matching(gains_db: &[f32], preamp_db: f32) -> Option<&'static Preset> {
    ALL.iter()
        .find(|preset| preset.matches(gains_db, preamp_db))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::eq::Bank;
    use mp_core::config::Equalizer;

    const RATE: f32 = 48_000.0;

    #[test]
    fn every_preset_has_a_full_set_of_bands() {
        for preset in ALL {
            assert_eq!(preset.gains_db.len(), BAND_COUNT, "{}", preset.name);
            assert!(!preset.description.is_empty(), "{}", preset.name);
        }
    }

    /// Presets are stored in the same config as user settings, so they have to
    /// survive the same clamping.
    #[test]
    fn every_preset_is_within_the_settings_range() {
        for preset in ALL {
            for gain in preset.gains_db {
                assert!(
                    gain.abs() <= Equalizer::MAX_GAIN_DB,
                    "{} has a {gain} dB band",
                    preset.name
                );
            }
            assert!(
                preset.preamp_db.abs() <= Equalizer::MAX_PREAMP_DB,
                "{} has a {} dB preamp",
                preset.name,
                preset.preamp_db
            );
        }
    }

    /// The property that makes presets safe to click: selecting one must not
    /// push the signal into the limiter.
    #[test]
    fn no_preset_clips_when_selected() {
        for preset in ALL {
            let bank = Bank::new(&preset.gains(), preset.preamp_db, RATE, true);
            let peak = bank.peak_gain_db();
            assert!(
                peak <= 1.0,
                "{} peaks at {peak:.2} dB, which would clip a loud master",
                preset.name
            );
        }
    }

    /// ...but they must not be so cautious that they do nothing either.
    #[test]
    fn every_preset_except_flat_actually_changes_something() {
        for preset in ALL {
            let bank = Bank::new(&preset.gains(), preset.preamp_db, RATE, true);
            let changed = [40.0, 200.0, 1_000.0, 5_000.0, 12_000.0]
                .iter()
                .any(|freq| bank.response_db(*freq).abs() > 1.0);

            if preset.name == FLAT.name {
                assert!(!changed, "Flat should be flat");
            } else {
                assert!(changed, "{} does nothing audible", preset.name);
            }
        }
    }

    #[test]
    fn the_shipped_default_equalizer_is_a_real_preset() {
        // `mp-core` has to spell the default curve out as literals, because it
        // sits below this crate and cannot name `ROCK`. That is a duplicated
        // constant, and duplicated constants drift; this is the thing that
        // notices. Without it, editing a preset here would leave every new
        // install with a curve labelled "Rock" that is not Rock.
        let default = mp_core::config::Equalizer::default();
        let named = default.preset.as_deref().expect("a named default preset");
        let preset = ALL
            .iter()
            .find(|p| p.name == named)
            .unwrap_or_else(|| panic!("default preset {named:?} is not a built-in"));

        assert_eq!(
            default.gains_db.as_slice(),
            preset.gains_db.as_slice(),
            "the default gains no longer match the {named} preset"
        );
        assert_eq!(default.preamp_db, preset.preamp_db);
    }

    #[test]
    fn presets_have_distinct_names_and_curves() {
        for (index, preset) in ALL.iter().enumerate() {
            for other in &ALL[index + 1..] {
                assert_ne!(preset.name, other.name);
                assert_ne!(
                    preset.gains_db, other.gains_db,
                    "{} and {} are the same curve",
                    preset.name, other.name
                );
            }
        }
    }

    #[test]
    fn a_preset_can_be_found_again_from_its_own_settings() {
        for preset in ALL {
            let found = matching(&preset.gains(), preset.preamp_db);
            assert_eq!(
                found.map(|p| p.name),
                Some(preset.name),
                "{} did not match itself",
                preset.name
            );
        }
    }

    #[test]
    fn an_edited_curve_no_longer_matches_its_preset() {
        let mut gains = ROCK.gains();
        gains[4] += 2.0;
        assert!(matching(&gains, ROCK.preamp_db).is_none());
    }

    #[test]
    fn lookup_by_name_ignores_case() {
        assert_eq!(by_name("bass boost").map(|p| p.name), Some("Bass Boost"));
        assert_eq!(by_name("FLAT").map(|p| p.name), Some("Flat"));
        assert!(by_name("nonexistent").is_none());
    }

    /// The bass presets should genuinely move bass, not just claim to.
    #[test]
    fn the_bass_presets_do_what_they_say() {
        let boost = Bank::new(&BASS_BOOST.gains(), 0.0, RATE, true);
        let cut = Bank::new(&BASS_CUT.gains(), 0.0, RATE, true);

        assert!(boost.response_db(50.0) > 3.0);
        assert!(cut.response_db(50.0) < -3.0);

        // ...and leave the top end alone.
        assert!(boost.response_db(8_000.0).abs() < 1.0);
        assert!(cut.response_db(8_000.0).abs() < 1.0);
    }

    /// Podcast exists to make speech clear: it must lift the vocal range and
    /// cut the rumble below it.
    #[test]
    fn the_podcast_preset_favours_the_voice() {
        let bank = Bank::new(&PODCAST.gains(), 0.0, RATE, true);
        assert!(bank.response_db(40.0) < -3.0, "rumble should be removed");
        assert!(bank.response_db(1_000.0) > 2.0, "speech should be lifted");
    }
}
