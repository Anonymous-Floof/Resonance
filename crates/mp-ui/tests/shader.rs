//! Compile the visualiser shader without a GPU.
//!
//! WGSL is compiled at *runtime*, by naga, when the pipeline is built. A typo
//! or a type error therefore does not fail the build — it panics on the first
//! frame the aurora is drawn, on whichever machine happens to open that view.
//! Running the same front end naga uses turns that into a test failure here.
//!
//! This validates rather than merely parses: parsing catches syntax, but the
//! errors worth catching are the semantic ones — a swizzle that does not exist,
//! a `vec3` assigned to a `vec4`, a uniform field that drifted out of step with
//! the Rust struct.

use naga::front::wgsl;
use naga::valid::{Capabilities, ValidationFlags, Validator};

const SOURCE: &str = include_str!("../src/visualizer/aurora.wgsl");

#[test]
fn the_aurora_shader_compiles() {
    let module = match wgsl::parse_str(SOURCE) {
        Ok(module) => module,
        Err(err) => panic!(
            "the aurora shader does not parse:\n{}",
            err.emit_to_string(SOURCE)
        ),
    };

    // The default capability set, which is what a plain wgpu device offers.
    // Validating against anything richer would let a shader through here that
    // failed on a real machine.
    let mut validator = Validator::new(ValidationFlags::all(), Capabilities::default());

    if let Err(err) = validator.validate(&module) {
        panic!(
            "the aurora shader does not validate:\n{}",
            err.emit_to_string(SOURCE)
        );
    }
}

#[test]
fn the_shader_declares_the_entry_points_the_pipeline_binds() {
    let module = wgsl::parse_str(SOURCE).expect("the shader should parse");

    let names: Vec<&str> = module
        .entry_points
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();

    // These exact strings are passed to `create_render_pipeline`. A rename on
    // one side only would fail at pipeline creation, on a user's machine.
    assert!(
        names.contains(&"vs_main"),
        "no vertex entry point named vs_main, found {names:?}"
    );
    assert!(
        names.contains(&"fs_main"),
        "no fragment entry point named fs_main, found {names:?}"
    );
}

/// The vertex shader draws three vertices and relies on the viewport egui sets.
/// If it ever grew a vertex buffer, the pipeline — which declares none — would
/// have to grow one too.
#[test]
fn the_vertex_stage_takes_no_vertex_buffers() {
    let module = wgsl::parse_str(SOURCE).expect("the shader should parse");

    let vertex = module
        .entry_points
        .iter()
        .find(|entry| entry.name == "vs_main")
        .expect("vs_main should exist");

    for argument in &vertex.function.arguments {
        let is_builtin = matches!(argument.binding, Some(naga::Binding::BuiltIn(_)));

        assert!(
            is_builtin,
            "vs_main takes a non-builtin input, but the pipeline declares no \
             vertex buffers"
        );
    }
}
