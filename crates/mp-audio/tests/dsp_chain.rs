//! End-to-end checks on the signal chain as the audio callback actually runs
//! it — no window, no sound device.
//!
//! The unit tests in `dsp::` verify each piece. These verify the assembly: that
//! the curve the interface draws is the curve the *chain* produces once the
//! preamp, the limiter, the volume smoothing and the fade envelope are all in
//! the path, and that running it costs no allocations.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::f32::consts::PI;

use mp_audio::dsp::chain::{Chain, Params};
use mp_audio::dsp::eq::Bank;
use mp_audio::dsp::limiter::Settings as LimiterSettings;
use mp_audio::dsp::presets;

const RATE: f32 = 48_000.0;
const CHANNELS: usize = 2;

// ---------------------------------------------------------------------------
// Allocation guard
// ---------------------------------------------------------------------------

/// Counts allocations while armed.
///
/// The audio callback must never allocate: the allocator can take a lock, and a
/// lock held by a lower-priority thread will eventually stall the callback and
/// produce an audible dropout. That failure is rare, load-dependent and almost
/// impossible to diagnose after the fact, so it is worth catching mechanically.
struct CountingAllocator;

// Per-thread, not global. A global counter would also tally the allocations of
// every *other* test running in parallel, which reads as a failure in this one
// and sent me looking for a bug in the signal chain that was not there.
thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_allocation();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_allocation();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Record an allocation, if this thread is currently counting.
///
/// `try_with` because thread-locals are themselves torn down during thread
/// exit, and touching a destroyed one would panic inside the allocator.
fn note_allocation() {
    let armed = ARMED.try_with(Cell::get).unwrap_or(false);
    if armed {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Run `body` with allocation counting on, and report how many happened.
fn count_allocations(body: impl FnOnce()) -> usize {
    ALLOCATIONS.with(|count| count.set(0));
    ARMED.with(|armed| armed.set(true));
    body();
    ARMED.with(|armed| armed.set(false));
    ALLOCATIONS.with(Cell::get)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn params_with(bank: Bank, limiter: bool) -> Params {
    let mut params = Params::for_rate(RATE);
    params.bank = bank;
    params.limiter = LimiterSettings::new(limiter, LimiterSettings::DEFAULT_CEILING_DB, RATE);
    params
}

/// A chain that has already settled: envelope up, volume at target, filters
/// past their transient. Measurements taken after this reflect steady state.
fn settled(params: Params) -> Chain {
    let mut chain = Chain::new();
    chain.set_params(params);
    chain.prime_volume(1.0);

    // Long enough for the coefficient crossfade, the fade envelope and the
    // slowest band's ringing to all finish.
    let mut warm = vec![0.0_f32; 4_096 * CHANNELS];
    for _ in 0..32 {
        warm.fill(0.0);
        chain.process(&mut warm, CHANNELS, 1.0);
    }
    chain
}

/// Measure the chain's gain at one frequency, in decibels.
fn measure_db(params: Params, freq: f32) -> f32 {
    let mut chain = settled(params);

    // A modest level, so the limiter is not the thing being measured.
    let amplitude = 0.2_f32;
    let settle_frames = 24_000;
    let measure_frames = 48_000;

    let mut phase = 0.0_f32;
    let step = 2.0 * PI * freq / RATE;

    let mut sum_in = 0.0_f64;
    let mut sum_out = 0.0_f64;
    let mut block = vec![0.0_f32; 512 * CHANNELS];
    let mut frames_done = 0;

    while frames_done < settle_frames + measure_frames {
        for frame in block.chunks_mut(CHANNELS) {
            let value = amplitude * phase.sin();
            phase += step;
            for sample in frame.iter_mut() {
                *sample = value;
            }
        }

        let input: Vec<f32> = block.clone();
        chain.process(&mut block, CHANNELS, 1.0);

        for (index, frame) in block.chunks(CHANNELS).enumerate() {
            if frames_done + index >= settle_frames {
                let dry = f64::from(input[index * CHANNELS]);
                let wet = f64::from(frame[0]);
                sum_in += dry * dry;
                sum_out += wet * wet;
            }
        }

        frames_done += block.len() / CHANNELS;
    }

    20.0 * (sum_out.sqrt() / sum_in.sqrt()).log10() as f32
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The plan's acceptance criterion for the equalizer, applied to the whole
/// chain rather than to a filter in isolation: the response the UI draws has to
/// be the response the user hears, within half a decibel.
#[test]
fn the_assembled_chain_matches_the_curve_the_interface_draws() {
    let bank = Bank::new(&presets::ROCK.gains(), presets::ROCK.preamp_db, RATE, true);

    // Spread across the audible range, deliberately including frequencies
    // between band centres where neighbouring bands sum.
    let probes = [
        40.0, 63.0, 110.0, 250.0, 440.0, 1_000.0, 1_500.0, 3_000.0, 6_000.0, 12_000.0,
    ];

    let mut worst = 0.0_f32;
    for freq in probes {
        let drawn = bank.response_db(freq);
        let heard = measure_db(params_with(bank, false), freq);
        let error = (drawn - heard).abs();

        assert!(
            error < 0.5,
            "at {freq} Hz the curve says {drawn:.2} dB but the chain produced \
             {heard:.2} dB (off by {error:.2} dB)"
        );
        worst = worst.max(error);
    }

    println!("worst curve error across the band: {worst:.3} dB");
}

/// Every built-in preset, not just one.
#[test]
fn every_preset_sounds_like_its_own_curve() {
    for preset in presets::ALL {
        let bank = Bank::new(&preset.gains(), preset.preamp_db, RATE, true);

        for freq in [63.0, 500.0, 4_000.0] {
            let drawn = bank.response_db(freq);
            let heard = measure_db(params_with(bank, false), freq);
            assert!(
                (drawn - heard).abs() < 0.5,
                "{} at {freq} Hz: drawn {drawn:.2} dB, heard {heard:.2} dB",
                preset.name
            );
        }
    }
}

/// A flat, disabled equalizer must be bit-transparent once settled, or every
/// track is quietly coloured by a feature nobody turned on.
#[test]
fn a_disabled_equalizer_is_transparent() {
    let params = params_with(Bank::bypassed(), false);
    let mut chain = settled(params);

    let mut block: Vec<f32> = (0..1_024 * CHANNELS)
        .map(|n| 0.3 * (n as f32 * 0.017).sin())
        .collect();
    let original = block.clone();

    chain.process(&mut block, CHANNELS, 1.0);

    for (index, (processed, dry)) in block.iter().zip(original.iter()).enumerate() {
        assert!(
            (processed - dry).abs() < 1e-4,
            "sample {index} changed from {dry} to {processed}"
        );
    }
}

/// The callback runs on a real-time thread. One allocation is one chance for
/// the allocator's lock to stall it behind a lower-priority thread.
#[test]
fn processing_never_allocates() {
    let hot = Bank::new(&[12.0; 10], -6.0, RATE, true);
    let mut chain = settled(params_with(hot, true));

    let mut block = vec![0.4_f32; 1_024 * CHANNELS];

    // Warm up outside the guard so any first-call laziness is not counted.
    chain.process(&mut block, CHANNELS, 1.0);

    let allocations = count_allocations(|| {
        for _ in 0..200 {
            for sample in block.iter_mut() {
                *sample = 0.4;
            }
            chain.process(&mut block, CHANNELS, 1.0);
        }
    });

    assert_eq!(
        allocations, 0,
        "the signal chain allocated {allocations} times while processing"
    );
}

/// Changing the equalizer is a control-thread action, but the *adoption* of the
/// new coefficients happens inside the callback. That must not allocate either.
#[test]
fn adopting_new_parameters_never_allocates() {
    let mut chain = settled(params_with(Bank::bypassed(), true));
    let mut block = vec![0.2_f32; 512 * CHANNELS];

    let curves: Vec<Params> = presets::ALL
        .iter()
        .map(|preset| {
            params_with(
                Bank::new(&preset.gains(), preset.preamp_db, RATE, true),
                true,
            )
        })
        .collect();

    // Build the parameter sets first; only the handover is measured.
    let allocations = count_allocations(|| {
        for params in &curves {
            chain.set_params(*params);
            chain.process(&mut block, CHANNELS, 1.0);
        }
    });

    assert_eq!(allocations, 0, "adopting parameters allocated");
}

/// The limiter has to hold the ceiling for real material, not just for the
/// synthetic worst case its own unit test uses.
#[test]
fn a_boosted_sweep_never_escapes_the_ceiling() {
    let hot = Bank::new(&[12.0; 10], 0.0, RATE, true);
    let params = params_with(hot, true);
    let ceiling = params.limiter.ceiling;

    let mut chain = settled(params);

    // A logarithmic sweep across the whole band at near full scale: every
    // filter in the bank gets excited in turn.
    let seconds = 4.0;
    let frames = (RATE * seconds) as usize;
    let mut block = vec![0.0_f32; 512 * CHANNELS];
    let mut phase = 0.0_f32;
    let mut done = 0;

    while done < frames {
        for frame in block.chunks_mut(CHANNELS) {
            let t = done as f32 / frames as f32;
            let freq = 20.0 * (1_000.0_f32).powf(t);
            phase += 2.0 * PI * freq / RATE;
            let value = 0.95 * phase.sin();
            for sample in frame.iter_mut() {
                *sample = value;
            }
        }

        chain.process(&mut block, CHANNELS, 1.0);

        for sample in &block {
            assert!(
                sample.abs() <= ceiling + 1e-4,
                "a boosted sweep reached {sample}, above the {ceiling} ceiling"
            );
        }

        done += block.len() / CHANNELS;
    }
}

/// IIR filters accumulate. A long session must not drift into denormals, NaN,
/// or a slowly growing DC offset.
#[test]
fn a_long_session_stays_healthy() {
    let hot = Bank::new(
        &presets::LOUDNESS.gains(),
        presets::LOUDNESS.preamp_db,
        RATE,
        true,
    );
    let mut chain = settled(params_with(hot, true));

    let minutes = 5.0;
    let frames = (RATE * 60.0 * minutes) as usize;
    let mut block = vec![0.0_f32; 4_096 * CHANNELS];
    let mut phase = 0.0_f32;
    let mut done = 0;
    let mut dc_sum = 0.0_f64;
    let mut counted = 0_u64;

    while done < frames {
        for frame in block.chunks_mut(CHANNELS) {
            // A mix of tones, so no single band dominates.
            phase += 1.0;
            let value =
                0.3 * ((phase * 0.01).sin() + (phase * 0.113).sin() + (phase * 0.457).sin()) / 3.0;
            for sample in frame.iter_mut() {
                *sample = value;
            }
        }

        chain.process(&mut block, CHANNELS, 1.0);

        for sample in &block {
            assert!(
                sample.is_finite(),
                "output went non-finite after {done} frames"
            );
            dc_sum += f64::from(*sample);
            counted += 1;
        }

        done += block.len() / CHANNELS;
    }

    let dc = (dc_sum / counted as f64).abs();
    assert!(
        dc < 1e-3,
        "a DC offset of {dc} built up over {minutes} minutes"
    );
}

/// Switching presets mid-playback is a normal thing to do, and it must not
/// produce a click at any of the switch points.
#[test]
fn switching_presets_mid_playback_never_clicks() {
    let mut chain = settled(params_with(Bank::bypassed(), false));

    let mut worst_step = 0.0_f32;
    let mut previous = 0.0_f32;
    let mut phase = 0.0_f32;
    let step = 2.0 * PI * 220.0 / RATE;

    for preset in presets::ALL.iter().chain(presets::ALL.iter().rev()) {
        let bank = Bank::new(&preset.gains(), preset.preamp_db, RATE, true);
        chain.set_params(params_with(bank, false));

        // A quarter second on each, which comfortably outlasts the crossfade.
        let mut block = vec![0.0_f32; 12_000 * CHANNELS];
        for frame in block.chunks_mut(CHANNELS) {
            let value = 0.5 * phase.sin();
            phase += step;
            for sample in frame.iter_mut() {
                *sample = value;
            }
        }

        chain.process(&mut block, CHANNELS, 1.0);

        for frame in block.chunks(CHANNELS) {
            worst_step = worst_step.max((frame[0] - previous).abs());
            previous = frame[0];
        }
    }

    // A 220 Hz sine at 48 kHz moves by at most ~0.014 per sample at this
    // amplitude; anything approaching a tenth would be an audible click.
    assert!(
        worst_step < 0.1,
        "preset switching produced a {worst_step:.4} step between samples"
    );
}

// ---------------------------------------------------------------------------
// The visualiser tap
// ---------------------------------------------------------------------------

/// The tap runs inside the callback, so it is under the same rule as the rest
/// of the chain — and it is the piece most likely to break it, because a ring
/// buffer *sounds* like something that might allocate.
#[test]
fn tapping_for_the_visualiser_never_allocates() {
    let (tap, mut monitor) = mp_audio::viz::channel();

    let mut chain = settled(params_with(
        Bank::new(
            &presets::ALL[1].gains(),
            presets::ALL[1].preamp_db,
            RATE,
            true,
        ),
        true,
    ));
    chain.set_tap(Some(tap));

    let mut block = vec![0.0_f32; 512 * CHANNELS];
    for (index, sample) in block.iter_mut().enumerate() {
        *sample = (index as f32 * 0.01).sin() * 0.5;
    }

    // Deliberately never drained, so the ring fills and every later push takes
    // the rejection path. If dropping were going to allocate, it would here.
    let allocations = count_allocations(|| {
        for _ in 0..64 {
            chain.process(&mut block, CHANNELS, 1.0);
        }
    });

    assert_eq!(
        allocations, 0,
        "the visualiser tap allocated {allocations} times"
    );

    // And it really was carrying audio, rather than passing the test by doing
    // nothing at all.
    assert!(monitor.poll() > 0, "the tap delivered no samples");
}

/// Detaching has to leave the chain producing exactly what it did before —
/// the tap is an observer, not a stage.
#[test]
fn the_tap_does_not_change_the_audio() {
    let bank = Bank::new(
        &presets::ALL[1].gains(),
        presets::ALL[1].preamp_db,
        RATE,
        true,
    );

    let source: Vec<f32> = (0..512 * CHANNELS)
        .map(|index| (index as f32 * 2.0 * PI * 440.0 / RATE).sin() * 0.4)
        .collect();

    let mut untapped = settled(params_with(bank, true));
    let mut plain = source.clone();
    untapped.process(&mut plain, CHANNELS, 1.0);

    let (tap, _monitor) = mp_audio::viz::channel();
    let mut tapped = settled(params_with(bank, true));
    tapped.set_tap(Some(tap));
    let mut observed = source.clone();
    tapped.process(&mut observed, CHANNELS, 1.0);

    assert_eq!(
        plain, observed,
        "attaching the visualiser tap changed the output samples"
    );
}

/// The tap sits before the volume control on purpose: turning the sound down
/// must not shrink the spectrum.
#[test]
fn the_tap_is_taken_before_the_volume_control() {
    let block_frames = 256;

    let reading_at = |volume: f32| -> f32 {
        let (tap, mut monitor) = mp_audio::viz::channel();
        let mut chain = settled(params_with(Bank::bypassed(), false));
        chain.set_tap(Some(tap));
        chain.prime_volume(volume);

        let mut block = vec![0.5_f32; block_frames * CHANNELS];
        chain.process(&mut block, CHANNELS, volume);

        monitor.poll();
        let mut out = vec![0.0; block_frames];
        monitor.latest(&mut out);
        out.iter().fold(0.0_f32, |peak, s| peak.max(s.abs()))
    };

    let loud = reading_at(1.0);
    let quiet = reading_at(0.05);

    assert!(loud > 0.4, "the tap read only {loud} at full volume");
    assert!(
        (loud - quiet).abs() < 0.01,
        "turning the volume down changed the visualiser reading from {loud} to {quiet}"
    );
}

/// Pausing should make the visualiser settle, which means the fade envelope
/// has to reach it.
#[test]
fn fading_out_settles_the_visualiser() {
    let (tap, mut monitor) = mp_audio::viz::channel();
    let mut chain = settled(params_with(Bank::bypassed(), false));
    chain.set_tap(Some(tap));

    chain.fade_out();

    // Long enough for a 10 ms fade at this rate to complete several times over.
    let mut block = vec![0.5_f32; 4096 * CHANNELS];
    chain.process(&mut block, CHANNELS, 1.0);

    monitor.poll();
    let mut out = vec![0.0; 512];
    monitor.latest(&mut out);

    let tail = out.iter().fold(0.0_f32, |peak, s| peak.max(s.abs()));
    assert!(
        tail < 0.001,
        "the visualiser was still seeing {tail} after a full fade-out"
    );
}
