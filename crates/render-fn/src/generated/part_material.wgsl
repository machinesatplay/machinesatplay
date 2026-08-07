#import bevy_pbr::{
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::alpha_discard,
    mesh_view_bindings::view,
}

#ifdef PREPASS_PIPELINE
#import bevy_pbr::{
    prepass_io::{VertexOutput, FragmentOutput},
    pbr_deferred_functions::deferred_output,
}
#else
#import bevy_pbr::{
    forward_io::{VertexOutput, FragmentOutput},
    pbr_functions::{apply_pbr_lighting, main_pass_post_lighting_processing},
}
#endif

struct PartMaterialExtension {
    // x: normal strength, y: texture-detail strength, z: shadow-floor strength,
    // w: normal-map shading distance.
    params: vec4<f32>,
    base_tint: vec4<f32>,
    sky_fill: vec4<f32>,
    ground_fill: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(100)
var<uniform> part_material: PartMaterialExtension;

@fragment
fn fragment(
    in: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> FragmentOutput {
    var pbr_input = pbr_input_from_standard_material(in, is_front);

    // Fade mapped relief into the geometric normal with distance. This keeps
    // distant texture detail quiet while retaining two-sided normal handling.
    let view_position = view.view_from_world * pbr_input.world_position;
    let view_depth = max(-view_position.z, 0.0);
    let normal_fade = clamp(
        1.0 - view_depth / max(part_material.params.w, 0.0001),
        0.0,
        1.0,
    ) * part_material.params.x;
    pbr_input.N = normalize(mix(
        normalize(pbr_input.world_normal),
        pbr_input.N,
        normal_fade,
    ));

    // The source material maps supply a hint of surface identity, but the
    // authored object color remains dominant. This is the low-frequency,
    // clean-color balance used across the illustrated environments.
    pbr_input.material.base_color = vec4(
        mix(
            part_material.base_tint.rgb,
            pbr_input.material.base_color.rgb,
            part_material.params.y,
        ),
        pbr_input.material.base_color.a,
    );

    pbr_input.material.base_color = alpha_discard(
        pbr_input.material,
        pbr_input.material.base_color,
    );

#ifdef PREPASS_PIPELINE
    let out = deferred_output(in, pbr_input);
#else
    var out: FragmentOutput;
    out.color = apply_pbr_lighting(pbr_input);

    // Clamp only the unlit side of the material to a colored hemisphere fill.
    // Direct light, cast shadows, and reflections remain intact; the clamp
    // prevents them from producing dead gray/black faces.
    let hemisphere = clamp(
        normalize(pbr_input.world_normal).y * 0.5 + 0.5,
        0.0,
        1.0,
    );
    let fill = mix(
        part_material.ground_fill.rgb,
        part_material.sky_fill.rgb,
        hemisphere,
    );
    let color_floor = pbr_input.material.base_color.rgb
        * fill * part_material.params.z;
    out.color = vec4(max(out.color.rgb, color_floor), out.color.a);
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
#endif

    return out;
}
