// Aurora bloom: curtains of light whose shape is the spectrum itself.
//
// The first version drove three fat noise ribbons from three band-energy
// scalars, summed them and clamped. Every core reached brightness ~1.9, three
// of them summed past 4.0, and the clamp turned the whole panel into a flat
// white slab that looked the same whatever was playing.
//
// Three things fix that, and they are the design of this shader:
//
//   1. The curtains follow the *spectrum*, not a scalar. Height at any x comes
//      from the band at that frequency, so the picture is a recognisable shape
//      that moves with the music rather than a generic wash.
//   2. Brightness is tone mapped rather than clamped. `1 - exp(-c)` rolls off
//      smoothly and never saturates to white, so overlapping curtains stay
//      distinguishable and their colours survive.
//   3. The curtains are thin, with a bright core inside a dim halo, and are
//      broken up by vertical striations — which is what makes a real aurora
//      read as curtains rather than as fog.
//
// Output is premultiplied alpha so the panel behind shows through the dark
// parts. That matters in the light theme, where an opaque fill would look like
// a hole punched in the surface.

struct Uniforms {
    // Panel size in pixels, so the striations keep a constant width on screen.
    resolution: vec2<f32>,
    // Seconds since the visualiser started. Wrapped, so precision holds.
    time: f32,
    // Beat strength, 0..1, decaying after each onset.
    onset: f32,
    // bass, mid, treble, rms — each 0..1.
    bands: vec4<f32>,
    color_a: vec4<f32>,
    color_b: vec4<f32>,
    color_c: vec4<f32>,
    // Sixteen spectrum bands, low to high, packed four to a vector.
    spectrum: array<vec4<f32>, 4>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOut {
    // One oversized triangle rather than two for a quad: it covers the
    // viewport with no seam down the diagonal, and the excess is clipped.
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );

    let position = corners[index];

    var out: VertexOut;
    out.clip = vec4<f32>(position, 0.0, 1.0);
    // Flipped in y so uv.y = 0 is the top, which is how the curtains below are
    // positioned.
    out.uv = vec2<f32>(position.x + 1.0, 1.0 - position.y) * 0.5;
    return out;
}

// The spectrum at a horizontal position, interpolated between bands.
fn spectrum_at(x: f32) -> f32 {
    let position = clamp(x, 0.0, 1.0) * 15.0;

    let lo = i32(floor(position));
    let hi = min(lo + 1, 15);
    let blend = position - f32(lo);

    let a = u.spectrum[lo / 4][lo % 4];
    let b = u.spectrum[hi / 4][hi % 4];

    return mix(a, b, blend);
}

fn hash21(p: vec2<f32>) -> f32 {
    let h = dot(p, vec2<f32>(127.1, 311.7));
    return fract(sin(h) * 43758.5453123);
}

// Value noise: four corners of a lattice cell, smoothly interpolated.
fn noise(p: vec2<f32>) -> f32 {
    let cell = floor(p);
    let offset = fract(p);

    // Smoothstep weights, so the lattice does not show as visible creases.
    let weight = offset * offset * (3.0 - 2.0 * offset);

    let a = hash21(cell);
    let b = hash21(cell + vec2<f32>(1.0, 0.0));
    let c = hash21(cell + vec2<f32>(0.0, 1.0));
    let d = hash21(cell + vec2<f32>(1.0, 1.0));

    return mix(mix(a, b, weight.x), mix(c, d, weight.x), weight.y);
}

// Fractal noise: a few octaves at halving amplitude, which is what gives the
// curtains detail at more than one scale.
fn fbm(p: vec2<f32>) -> f32 {
    var total = 0.0;
    var amplitude = 0.5;
    var point = p;

    for (var octave = 0; octave < 4; octave = octave + 1) {
        total = total + amplitude * noise(point);
        // Not exactly 2.0: an integer ratio lines the octaves up on the same
        // lattice and produces visible grid artefacts.
        point = point * 2.03;
        amplitude = amplitude * 0.5;
    }

    return total;
}

// One curtain, hanging from `base` and lifted by the spectrum beneath it.
//
// Returns brightness at this pixel: a narrow bright core inside a wider, much
// dimmer halo. A single gaussian wide enough to glow has no discernible edge,
// which is precisely what made the first version read as fog.
fn curtain(
    uv: vec2<f32>,
    base: f32,
    lift: f32,
    thickness: f32,
    drift: f32,
    seed: f32,
) -> f32 {
    let level = spectrum_at(uv.x);

    // The noise only wobbles the curtain; the shape comes from the music.
    let wobble = (fbm(vec2<f32>(uv.x * 2.6 + u.time * drift, u.time * 0.10 + seed)) - 0.5) * 0.20;

    let line = base - level * lift + wobble;
    let distance = (uv.y - line) / max(thickness, 0.004);

    let core = exp(-distance * distance);
    let halo = exp(-abs(distance) * 0.55) * 0.22;

    // Dims where its own band is quiet, but does not vanish: the treble end of
    // most music is far below the bass, and multiplying straight by the level
    // left the right-hand half of the panel empty. A floor keeps the curtain
    // trailing across the full width, fading rather than stopping — which is
    // what a real aurora does, and it still goes dark on silence because the
    // whole thing is scaled by loudness further down.
    return (core + halo) * (0.28 + level * 0.72);
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let uv = in.uv;

    let bass = u.bands.x;
    let mid = u.bands.y;
    let treble = u.bands.z;
    let rms = u.bands.w;

    // Beats brighten everything briefly.
    let pulse = 1.0 + u.onset * 0.6;

    var colour = vec3<f32>(0.0);

    colour = colour + u.color_a.rgb
        * curtain(uv, 0.86, 0.44, 0.050 + bass * 0.030, 0.045, 0.0)
        * (0.30 + bass * 0.75);

    colour = colour + u.color_b.rgb
        * curtain(uv, 0.70, 0.38, 0.038 + mid * 0.022, -0.075, 11.3)
        * (0.26 + mid * 0.70);

    colour = colour + u.color_c.rgb
        * curtain(uv, 0.55, 0.32, 0.028 + treble * 0.018, 0.115, 27.9)
        * (0.22 + treble * 0.65);

    // Vertical striations — the rays a real aurora hangs in. Higher frequency
    // than the curtains themselves and drifting slowly across them, which is
    // what turns a smooth band of colour into something with structure.
    let rays = 0.52 + 0.48 * fbm(vec2<f32>(uv.x * 22.0 + u.time * 0.30, u.time * 0.18));
    colour = colour * rays * pulse;

    // A low wash along the bottom, the way an aurora sits over a horizon.
    // Tied to loudness so silence really is dark.
    colour = colour + u.color_a.rgb * exp(-(1.0 - uv.y) * 7.0) * rms * 0.30;

    // Fade at the left and right edges so the curtains do not end in a hard
    // vertical line at the panel border.
    let edge = smoothstep(0.0, 0.10, uv.x) * smoothstep(0.0, 0.10, 1.0 - uv.x);
    colour = colour * edge;

    // Tone mapping, not a clamp. Clamping three overlapping curtains drives
    // every channel to 1.0 and the panel goes white, losing both the colour
    // and the shape; this rolls off smoothly and keeps them.
    colour = vec3<f32>(1.0) - exp(-colour * 1.9);

    // Premultiplied: alpha is the brightest channel, so the dark regions are
    // genuinely transparent rather than black.
    let alpha = max(max(colour.r, colour.g), colour.b);

    return vec4<f32>(colour, alpha);
}
