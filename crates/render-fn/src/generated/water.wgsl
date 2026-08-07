#import bevy_pbr::{
    forward_io::{VertexOutput, FragmentOutput},
    mesh_functions,
    mesh_view_bindings::{globals, view},
    view_transformations::position_world_to_clip,
}

struct WaterSettings {
    shallow_color: vec4<f32>,
    deep_color: vec4<f32>,
    foam_color: vec4<f32>,
    surface: vec4<f32>,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> water: WaterSettings;

fn water_height(position: vec2<f32>, time: f32) -> f32 {
    var height = 0.0;
    height += sin(dot(position, vec2(0.62, 0.78)) * 1.05 + time * 1.10) * 0.032;
    height += sin(dot(position, vec2(-0.85, 0.53)) * 1.71 + time * 1.55) * 0.020;
    height += sin(dot(position, vec2(0.31, -0.95)) * 2.90 + time * 2.15) * 0.011;
    height += sin(time * 0.29) * 0.010;
    return height;
}

fn water_normal(position: vec2<f32>, time: f32) -> vec3<f32> {
    let phase_a = dot(position, vec2(0.62, 0.78)) * 1.05 + time * 1.10;
    let phase_b = dot(position, vec2(-0.85, 0.53)) * 1.71 + time * 1.55;
    let phase_c = dot(position, vec2(0.31, -0.95)) * 2.90 + time * 2.15;
    let gradient =
        vec2(0.62, 0.78) * (cos(phase_a) * 1.05 * 0.032)
        + vec2(-0.85, 0.53) * (cos(phase_b) * 1.71 * 0.020)
        + vec2(0.31, -0.95) * (cos(phase_c) * 2.90 * 0.011);
    return normalize(vec3(-gradient.x, 1.0, -gradient.y));
}

@vertex
fn vertex(
    @builtin(instance_index) instance_index: u32,
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
) -> VertexOutput {
    var out: VertexOutput;
    let world_from_local = mesh_functions::get_world_from_local(instance_index);
    let original_world = mesh_functions::mesh_position_local_to_world(
        world_from_local,
        vec4(position, 1.0),
    );
    var displaced = original_world.xyz;

    // Animate the top surface and the side walls' upper seam together.
    if position.y > -0.001 {
        displaced.y += water_height(original_world.xz, globals.time);
    }

    out.position = position_world_to_clip(displaced);
    out.world_position = vec4(displaced, 1.0);
    if normal.y > 0.5 {
        out.world_normal = water_normal(original_world.xz, globals.time);
    } else {
        out.world_normal = mesh_functions::mesh_normal_local_to_world(normal, instance_index);
    }
    out.uv = uv;
    return out;
}

@fragment
fn fragment(in: VertexOutput) -> FragmentOutput {
    let normal = normalize(in.world_normal);
    let is_surface = smoothstep(0.45, 0.80, normal.y);
    let view_direction = normalize(view.world_position - in.world_position.xyz);

    // A tiny amount of broad directional shading makes the moving geometry
    // readable. There are deliberately no foam, caustics, sparkles, texture
    // lines, shoreline bands, or reflection effects.
    let light_direction = normalize(vec3(0.35, 0.90, 0.22));
    let wave_shading = 0.92 + max(dot(normal, light_direction), 0.0) * 0.08;
    var surface_color = mix(water.shallow_color.rgb, water.deep_color.rgb, 0.22)
        * wave_shading;

    // Smooth sky reflection only. Fresnel makes it stronger at glancing
    // angles, while the moving vertex normal gently shifts it with the waves.
    let fresnel = pow(
        clamp(1.0 - max(dot(normal, view_direction), 0.0), 0.0, 1.0),
        3.0,
    );
    let sky_reflection = vec3(0.18, 0.48, 0.82);
    surface_color = mix(surface_color, sky_reflection, 0.10 + fresnel * 0.42);

    // One broad, blue-white highlight gives the surface a readable sheen
    // without creating a foam-like white outline or patterned streaks.
    let sun_direction = normalize(vec3(0.38, 0.86, 0.34));
    let half_direction = normalize(view_direction + sun_direction);
    let sun_glint = pow(max(dot(normal, half_direction), 0.0), 44.0);
    surface_color += vec3(0.26, 0.48, 0.62) * sun_glint * 0.34;
    let side_color = mix(
        water.shallow_color.rgb * 0.94,
        water.deep_color.rgb * 0.84,
        smoothstep(0.0, 1.0, in.uv.y),
    );

    var out: FragmentOutput;
    out.color = vec4(
        mix(side_color, surface_color, is_surface),
        mix(0.74, water.surface.y, is_surface),
    );
    return out;
}
