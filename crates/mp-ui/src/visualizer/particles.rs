//! A particle field that erupts on beats.
//!
//! Unlike the aurora this one stays on the CPU. A few hundred particles is a
//! few hundred circles, egui batches them into a single mesh, and the
//! simulation is a couple of adds per particle per frame — putting that on the
//! GPU would mean a compute pass, a storage buffer and a second pipeline to
//! save an amount of work that does not register. The aurora needed the GPU
//! because it shades every pixel; this does not.
//!
//! The interesting part is the spawning. Particles are emitted on *onsets* —
//! the analyzer's beat detector — rather than on a timer, so the field pulses
//! with the drums instead of drifting at a constant rate that happens to look
//! busy.

use egui::{Painter, Pos2, Rect, Vec2};
use mp_audio::viz::Frame;

use super::Paint;
use crate::theme::col_alpha;

/// Hard ceiling on live particles.
///
/// Reached only during sustained loud passages. The cost is linear and the
/// visual difference above this is nil, so it is a cap rather than a budget to
/// be spent.
const MAX_PARTICLES: usize = 420;

/// Emitted at a full-strength onset.
const BURST: usize = 34;

/// Seconds a particle lives.
const LIFE_SECS: f32 = 2.4;

/// Downward drift, in fractions of the panel height per second squared.
///
/// Gentle: particles should hang and fade, not fall like rain.
const GRAVITY: f32 = 0.22;

/// How much of its speed a particle keeps each second.
const DRAG: f32 = 0.55;

#[derive(Debug, Clone, Copy)]
struct Particle {
    /// Position in panel space, `0.0..=1.0` on both axes.
    position: Vec2,
    /// Velocity in panel widths per second.
    velocity: Vec2,
    /// Remaining life in seconds.
    life: f32,
    /// Radius in points at full life.
    size: f32,
    /// Where in the spectrum this particle takes its colour from.
    tone: f32,
}

pub struct Field {
    particles: Vec<Particle>,
    rng: Rng,
    /// Onset level last frame, so a sustained beat does not re-trigger every
    /// frame it stays above the threshold.
    was_beating: bool,
}

impl Field {
    pub fn new() -> Self {
        Self {
            particles: Vec::with_capacity(MAX_PARTICLES),
            rng: Rng::new(0x5EED_1234_ABCD_0001),
            was_beating: false,
        }
    }

    /// Number of live particles.
    #[cfg(test)]
    fn live(&self) -> usize {
        self.particles.len()
    }

    /// Advance the simulation and emit on a beat.
    pub fn update(&mut self, frame: &Frame, dt: f32) {
        // A stalled UI should not teleport every particle off the panel.
        let dt = dt.clamp(0.0, 1.0 / 15.0);

        self.integrate(dt);

        if !frame.active {
            // Nothing playing: let what is left fade out rather than clearing,
            // so stopping looks like settling rather than a cut.
            self.was_beating = false;
            return;
        }

        // Edge-triggered. The onset signal stays high for a moment after a
        // hit, and emitting on every frame it is high would turn one drum beat
        // into a continuous jet.
        let beating = frame.onset > 0.35;
        if beating && !self.was_beating {
            self.burst(frame);
        }
        self.was_beating = beating;

        // A slow trickle keeps the field alive through quiet passages, scaled
        // by loudness so silence really is empty.
        if frame.rms > 0.02 && self.rng.chance(frame.rms * 0.7) {
            self.emit(1, frame, 0.45);
        }
    }

    /// Move everything and retire what has expired.
    fn integrate(&mut self, dt: f32) {
        // Per-second drag converted to this frame's share.
        let damping = DRAG.powf(dt);

        for particle in &mut self.particles {
            particle.velocity.y += GRAVITY * dt;
            particle.velocity *= damping;
            particle.position += particle.velocity * dt;
            particle.life -= dt;
        }

        self.particles
            .retain(|particle| particle.life > 0.0 && particle.position.y < 1.35);
    }

    fn burst(&mut self, frame: &Frame) {
        let strength = frame.onset.clamp(0.0, 1.0);
        let count = (BURST as f32 * strength).round() as usize;
        self.emit(count, frame, 1.0);
    }

    fn emit(&mut self, count: usize, frame: &Frame, vigour: f32) {
        for _ in 0..count {
            if self.particles.len() >= MAX_PARTICLES {
                return;
            }

            // Along the bottom, so the field rises into the panel. Bass pushes
            // the emission wider, which reads as a bigger hit.
            let spread = 0.5 + frame.bass * 0.45;
            let x = 0.5 + self.rng.signed() * spread;

            // Upward, with the speed set by how hard the beat was.
            let speed = (0.25 + frame.onset * 0.55) * vigour;
            let angle = self.rng.signed() * 0.85;

            self.particles.push(Particle {
                position: Vec2::new(x, 1.02),
                velocity: Vec2::new(
                    angle * speed * 0.6,
                    -(speed * (0.65 + self.rng.unit() * 0.6)),
                ),
                life: LIFE_SECS * (0.55 + self.rng.unit() * 0.45),
                size: 1.4 + self.rng.unit() * 2.6 + frame.bass * 2.0,
                // Treble-heavy material spawns particles further up the
                // spectrum, so the colours follow the music in spectrum mode.
                tone: (self.rng.unit() * 0.55 + frame.treble * 0.45).clamp(0.0, 1.0),
            });
        }
    }

    pub fn draw(&self, painter: &Painter, rect: Rect, paint: &Paint) {
        for particle in &self.particles {
            let fade = (particle.life / LIFE_SECS).clamp(0.0, 1.0);

            // Fade in over the first moments as well as out at the end, so
            // particles appear rather than pop.
            let age = 1.0 - fade;
            let alpha = fade * (age / 0.08).clamp(0.0, 1.0);

            let centre = Pos2::new(
                rect.left() + particle.position.x * rect.width(),
                rect.top() + particle.position.y * rect.height(),
            );

            // Skip anything outside the panel rather than relying on the
            // clip rect, which would still tessellate it.
            if !rect.expand(particle.size * 2.0).contains(centre) {
                continue;
            }

            let colour = paint.at(particle.tone);

            // A soft halo under a bright core: two circles is enough to
            // suggest a glow without a blur pass.
            painter.circle_filled(centre, particle.size * 2.2, col_alpha(colour, alpha * 0.16));
            painter.circle_filled(centre, particle.size * fade, col_alpha(colour, alpha));
        }
    }
}

impl Default for Field {
    fn default() -> Self {
        Self::new()
    }
}

/// The same xorshift the queue uses, kept local rather than shared.
///
/// A visualiser wants a generator it can spin freely without any chance of
/// perturbing playback order, which is what sharing one would risk.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `0.0..1.0`.
    fn unit(&mut self) -> f32 {
        // Top 24 bits: an f32 has 24 bits of mantissa, so this uses all the
        // precision available and no more.
        (self.next_u64() >> 40) as f32 / (1u32 << 24) as f32
    }

    /// `-1.0..1.0`.
    fn signed(&mut self) -> f32 {
        self.unit() * 2.0 - 1.0
    }

    /// True with the given probability.
    fn chance(&mut self, probability: f32) -> bool {
        self.unit() < probability
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beat(onset: f32) -> Frame {
        Frame {
            onset,
            rms: 0.4,
            bass: 0.5,
            treble: 0.3,
            active: true,
            ..Frame::default()
        }
    }

    fn quiet() -> Frame {
        Frame {
            active: false,
            ..Frame::default()
        }
    }

    /// The whole point: particles come from beats, not from a timer.
    #[test]
    fn a_beat_emits_particles() {
        let mut field = Field::new();

        field.update(&quiet(), 1.0 / 60.0);
        assert_eq!(field.live(), 0);

        field.update(&beat(1.0), 1.0 / 60.0);
        assert!(
            field.live() > 10,
            "a full beat emitted only {}",
            field.live()
        );
    }

    /// The onset signal decays over several frames. Emitting on each of them
    /// would turn one drum hit into a continuous jet.
    #[test]
    fn one_beat_emits_once_however_long_it_stays_high() {
        let mut field = Field::new();

        field.update(&beat(1.0), 1.0 / 60.0);
        let after_first = field.live();

        // The same beat, still above the threshold, for several more frames.
        // `rms` is zeroed so the quiet-passage trickle cannot muddy the count.
        let mut held = beat(1.0);
        held.rms = 0.0;
        for _ in 0..10 {
            field.update(&held, 1.0 / 60.0);
        }

        assert!(
            field.live() <= after_first,
            "a held onset kept emitting: {} then {}",
            after_first,
            field.live()
        );
    }

    /// A second, separate hit should emit again.
    #[test]
    fn a_new_beat_after_a_lull_emits_again() {
        let mut field = Field::new();

        field.update(&beat(1.0), 1.0 / 60.0);
        let first = field.live();

        // Onset falls back below the threshold, then rises again.
        let mut lull = beat(0.0);
        lull.rms = 0.0;
        field.update(&lull, 1.0 / 60.0);

        let mut second = beat(1.0);
        second.rms = 0.0;
        field.update(&second, 1.0 / 60.0);

        assert!(
            field.live() > first,
            "the second beat added nothing: {} then {}",
            first,
            field.live()
        );
    }

    #[test]
    fn particles_expire() {
        let mut field = Field::new();
        field.update(&beat(1.0), 1.0 / 60.0);
        assert!(field.live() > 0);

        // Well past the longest life, with nothing new arriving.
        for _ in 0..300 {
            field.update(&quiet(), 1.0 / 60.0);
        }

        assert_eq!(field.live(), 0, "particles outlived their lifetime");
    }

    /// Silence has to empty the field, not freeze it.
    #[test]
    fn silence_stops_emission() {
        let mut field = Field::new();

        for _ in 0..120 {
            field.update(&quiet(), 1.0 / 60.0);
        }

        assert_eq!(field.live(), 0);
    }

    /// Sustained loud material must not grow the field without bound.
    #[test]
    fn the_field_is_capped() {
        let mut field = Field::new();

        for tick in 0..2_000 {
            // Alternating so the edge trigger fires as often as possible.
            let onset = if tick % 2 == 0 { 1.0 } else { 0.0 };
            field.update(&beat(onset), 1.0 / 60.0);

            assert!(
                field.live() <= MAX_PARTICLES,
                "the field reached {} particles",
                field.live()
            );
        }
    }

    /// A hitch in the UI should not fling everything off the panel.
    #[test]
    fn a_long_frame_does_not_teleport_particles() {
        let mut field = Field::new();
        field.update(&beat(1.0), 1.0 / 60.0);

        let before = field.live();
        field.update(&quiet(), 5.0);

        assert!(
            field.live() > before / 2,
            "a five-second frame wiped out {} of {before} particles",
            before - field.live()
        );
    }

    #[test]
    fn the_generator_stays_in_range() {
        let mut rng = Rng::new(7);

        for _ in 0..10_000 {
            let unit = rng.unit();
            assert!((0.0..1.0).contains(&unit), "unit() produced {unit}");

            let signed = rng.signed();
            assert!((-1.0..1.0).contains(&signed), "signed() produced {signed}");
        }
    }

    /// A generator seeded to zero would be stuck there forever.
    #[test]
    fn the_generator_survives_a_zero_seed() {
        let mut rng = Rng::new(0);

        let first = rng.unit();
        let second = rng.unit();

        assert_ne!(first, second, "the generator is stuck");
    }
}
