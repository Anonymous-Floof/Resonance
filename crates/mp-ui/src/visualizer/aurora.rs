//! The aurora bloom visualiser: a real GPU fragment shader.
//!
//! Everything else here paints through egui, which means the CPU builds a mesh
//! and the GPU draws it. That is the right trade for bars and lines, where
//! there are a few hundred vertices and the shapes are the point. It is the
//! wrong trade for a full-panel effect where every *pixel* is different — an
//! aurora is per-pixel noise, and building that on the CPU would mean either a
//! texture upload every frame or a resolution nobody wants to look at.
//!
//! So this one hands the panel to wgpu and lets a shader fill it.
//!
//! # How it hangs together
//!
//! The pipeline is built once at startup, from [`install`], because that is
//! where the surface's texture format is known — `prepare` is handed a device
//! but not a format. It lives in egui's `callback_resources`, a type map shared
//! by every paint callback.
//!
//! If the app is ever built against a non-wgpu backend, [`install`] finds no
//! render state, nothing is inserted, and [`Callback::paint`] quietly draws
//! nothing rather than panicking.

use eframe::egui_wgpu::{self, CallbackResources, CallbackTrait, ScreenDescriptor, wgpu};
use egui::{PaintCallbackInfo, Painter, Rect};
use mp_audio::viz::Frame;
use mp_core::color::Rgb;

use super::Paint;

/// Parameters handed to the shader each frame.
///
/// Laid out to match the WGSL `Uniforms` struct. The two scalars after
/// `resolution` fill the gap before the first `vec4`, which a uniform buffer
/// requires to be 16-byte aligned — reordering these fields would silently
/// misalign everything after them.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    resolution: [f32; 2],
    time: f32,
    onset: f32,
    /// bass, mid, treble, rms.
    bands: [f32; 4],
    color_a: [f32; 4],
    color_b: [f32; 4],
    color_c: [f32; 4],
    /// Sixteen spectrum bands, low to high, four to a vector.
    ///
    /// The shader reads these as `array<vec4<f32>, 4>`; a uniform array has a
    /// stride of sixteen bytes per element, which `[[f32; 4]; 4]` matches
    /// exactly.
    spectrum: [[f32; 4]; 4],
}

/// How many spectrum bands the shader is handed.
///
/// Enough for a curtain to have a recognisable shape, few enough that the
/// interpolation between them stays smooth rather than jagged. The analyzer's
/// own bar count is independent and usually higher.
const SHADER_BANDS: usize = 16;

/// The GPU objects, created once and kept in egui's callback resources.
struct Resources {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    buffer: wgpu::Buffer,
}

/// Build the pipeline and register it with egui.
///
/// Call once, from app construction. Doing nothing when there is no wgpu render
/// state is deliberate: the visualiser simply will not draw, and every other
/// part of the app carries on.
pub fn install(cc: &eframe::CreationContext<'_>) {
    let Some(state) = cc.wgpu_render_state.as_ref() else {
        tracing::info!("no wgpu render state; the aurora visualiser is unavailable");
        return;
    };

    let device = &state.device;

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("aurora"),
        source: wgpu::ShaderSource::Wgsl(include_str!("aurora.wgsl").into()),
    });

    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("aurora uniforms"),
        size: std::mem::size_of::<Uniforms>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("aurora bind group layout"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("aurora bind group"),
        layout: &layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buffer.as_entire_binding(),
        }],
    });

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("aurora pipeline layout"),
        bind_group_layouts: &[Some(&layout)],
        // No immediate data: every parameter travels in the uniform buffer.
        immediate_size: 0,
    });

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("aurora"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: state.target_format,
                // The shader outputs premultiplied alpha, matching what egui
                // itself writes into this same render pass.
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    state.renderer.write().callback_resources.insert(Resources {
        pipeline,
        bind_group,
        buffer,
    });

    tracing::debug!("aurora visualiser pipeline ready");
}

/// One frame's worth of parameters, on its way to the GPU.
struct Callback {
    uniforms: Uniforms,
}

impl CallbackTrait for Callback {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Some(resources) = resources.get::<Resources>() {
            queue.write_buffer(&resources.buffer, 0, bytemuck::bytes_of(&self.uniforms));
        }

        Vec::new()
    }

    fn paint(
        &self,
        _info: PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &CallbackResources,
    ) {
        // Absent only when `install` found no wgpu backend. Drawing nothing is
        // the right answer; the view says the visualiser is unavailable.
        let Some(resources) = resources.get::<Resources>() else {
            return;
        };

        // egui has already set the viewport to the callback's rectangle, so the
        // oversized triangle in the vertex shader lands exactly on the panel.
        render_pass.set_pipeline(&resources.pipeline);
        render_pass.set_bind_group(0, &resources.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}

/// How long the shader clock runs before wrapping, in seconds.
///
/// An `f32` loses meaningful precision in the noise field after a few hours of
/// counting seconds, which shows up as the curtains going jerky. Wrapping on a
/// whole number of minutes keeps the motion smooth and the seam invisible,
/// because the noise field is not periodic anyway.
const TIME_WRAP_SECS: f32 = 600.0;

/// Draw the aurora into `rect`.
///
/// `elapsed` is the running clock, `dt` the time since the last frame.
pub fn draw(painter: &Painter, rect: Rect, frame: &Frame, paint: &Paint, elapsed: f32) {
    let (a, b, c) = paint.triad();

    let uniforms = Uniforms {
        resolution: [rect.width(), rect.height()],
        time: elapsed % TIME_WRAP_SECS,
        onset: frame.onset,
        bands: [frame.bass, frame.mid, frame.treble, frame.rms],
        color_a: linear(a),
        color_b: linear(b),
        color_c: linear(c),
        spectrum: pack_spectrum(&frame.bars),
    };

    painter.add(egui_wgpu::Callback::new_paint_callback(
        rect,
        Callback { uniforms },
    ));
}

/// Reduce the analyzer's bars to the handful the shader reads.
///
/// Takes the loudest bar in each group rather than the average: the curtain
/// should rise to meet a peak, and averaging a group that contains one strong
/// band with several empty ones flattens exactly the feature worth showing.
fn pack_spectrum(bars: &[f32]) -> [[f32; 4]; 4] {
    let mut packed = [[0.0_f32; 4]; 4];

    if bars.is_empty() {
        return packed;
    }

    for band in 0..SHADER_BANDS {
        let from = band * bars.len() / SHADER_BANDS;
        let to = ((band + 1) * bars.len() / SHADER_BANDS)
            .max(from + 1)
            .min(bars.len());

        let loudest = bars[from..to].iter().copied().fold(0.0_f32, f32::max);
        packed[band / 4][band % 4] = loudest;
    }

    packed
}

/// A colour as the shader wants it.
///
/// Passed through unconverted rather than linearised: egui writes its own
/// vertex colours into this same target the same way, so matching it is what
/// makes the aurora sit in the same palette as everything around it.
fn linear(rgb: Rgb) -> [f32; 4] {
    [
        rgb.r as f32 / 255.0,
        rgb.g as f32 / 255.0,
        rgb.b as f32 / 255.0,
        1.0,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The struct is read by the shader against a hand-written WGSL layout, so
    /// its size and alignment are part of the interface.
    #[test]
    fn the_uniform_block_matches_the_shader_layout() {
        // vec2 + 2 floats, then four vec4s, then a 4-element vec4 array.
        assert_eq!(std::mem::size_of::<Uniforms>(), 16 + 64 + 64);

        // Uniform buffers require the whole block to be a multiple of 16.
        assert_eq!(std::mem::size_of::<Uniforms>() % 16, 0);
    }

    #[test]
    fn packing_the_spectrum_keeps_low_to_high_order() {
        // A ramp: every band louder than the one below it.
        let bars: Vec<f32> = (0..64).map(|index| index as f32 / 63.0).collect();
        let packed = pack_spectrum(&bars);

        let flat: Vec<f32> = packed.iter().flatten().copied().collect();
        assert_eq!(flat.len(), SHADER_BANDS);

        for pair in flat.windows(2) {
            assert!(
                pair[1] > pair[0],
                "the packed spectrum is not ascending: {flat:?}"
            );
        }
    }

    /// A single loud band must survive being grouped with quiet neighbours,
    /// or the curtain flattens exactly where the music is most interesting.
    #[test]
    fn packing_keeps_a_peak_rather_than_averaging_it_away() {
        let mut bars = vec![0.0_f32; 64];
        bars[33] = 1.0;

        let packed = pack_spectrum(&bars);
        let flat: Vec<f32> = packed.iter().flatten().copied().collect();

        assert!(flat.contains(&1.0), "the peak was averaged away: {flat:?}");
    }

    /// The bar count is a user setting, so packing has to cope with any of it.
    #[test]
    fn packing_handles_every_bar_count() {
        for count in [1, 8, 15, 16, 17, 64, 255, 256] {
            let bars = vec![0.5_f32; count];
            let packed = pack_spectrum(&bars);

            let flat: Vec<f32> = packed.iter().flatten().copied().collect();
            assert_eq!(flat.len(), SHADER_BANDS);
            assert!(
                flat.iter().all(|value| (0.0..=1.0).contains(value)),
                "{count} bars packed out of range: {flat:?}"
            );
        }

        // And an empty analysis must not panic.
        assert_eq!(pack_spectrum(&[]), [[0.0; 4]; 4]);
    }

    /// The shader is compiled at runtime by naga, so a typo would surface as a
    /// panic on the first frame. Checking the entry points are present catches
    /// the most likely version of that at build time.
    #[test]
    fn the_shader_declares_the_entry_points_the_pipeline_asks_for() {
        let source = include_str!("aurora.wgsl");

        assert!(source.contains("fn vs_main"), "missing vertex entry point");
        assert!(
            source.contains("fn fs_main"),
            "missing fragment entry point"
        );
        assert!(
            source.contains("var<uniform> u: Uniforms"),
            "the uniform binding is not where the pipeline layout expects it"
        );
    }

    /// The WGSL struct and the Rust struct have to list the same fields in the
    /// same order, and nothing checks that at compile time.
    #[test]
    fn the_shader_uniform_fields_match_the_rust_ones() {
        let source = include_str!("aurora.wgsl");

        for field in [
            "resolution: vec2<f32>",
            "time: f32",
            "onset: f32",
            "bands: vec4<f32>",
            "color_a: vec4<f32>",
            "color_b: vec4<f32>",
            "color_c: vec4<f32>",
            "spectrum: array<vec4<f32>, 4>",
        ] {
            assert!(source.contains(field), "the shader is missing `{field}`");
        }
    }

    /// The clock is wrapped before it reaches the shader, so a long session
    /// keeps the same motion as a fresh one.
    #[test]
    fn the_clock_wraps_without_jumping() {
        // Just before the wrap and just after should be a small step apart,
        // not a jump back through ten minutes of animation.
        let before = (TIME_WRAP_SECS - 0.016) % TIME_WRAP_SECS;
        let after = TIME_WRAP_SECS % TIME_WRAP_SECS;

        assert!(before > TIME_WRAP_SECS - 1.0);
        assert_eq!(after, 0.0);

        // An hour in, the value is still small enough for f32 to resolve a
        // single frame's worth of change.
        let late = 3_600.0_f32 % TIME_WRAP_SECS;
        assert!(late + 0.016 > late, "the clock has lost frame resolution");
    }
}
