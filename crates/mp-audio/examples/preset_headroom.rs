//! Report the peak gain of each built-in equalizer preset.
//!
//! Overlapping bands sum, so a preset's peak is not its loudest slider — the
//! only reliable way to pick a preamp is to measure the composite curve.

use mp_audio::dsp::eq::Bank;
use mp_audio::dsp::presets;

fn main() {
    println!(
        "{:<12} {:>10} {:>10} {:>10}",
        "preset", "raw peak", "preamp", "result"
    );
    for preset in presets::ALL {
        let raw = Bank::new(&preset.gains(), 0.0, 48_000.0, true).peak_gain_db();
        let needed = (-raw * 2.0).floor() / 2.0;
        let with_current =
            Bank::new(&preset.gains(), preset.preamp_db, 48_000.0, true).peak_gain_db();
        println!(
            "{:<12} {raw:>9.2} {:>9.1} {with_current:>9.2}",
            preset.name, preset.preamp_db
        );
        if with_current > 0.0 {
            println!("             -> preamp should be {needed:.1}");
        }
    }
}
