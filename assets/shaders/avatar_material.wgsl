// Stylized character shading for the original avatar head and body geometry.
// Hand-tuned diffuse, highlight, and rim terms keep the primitive shapes soft.

#import bevy_pbr::{
    forward_io::VertexOutput,
    mesh_view_bindings::view,
}

struct AvatarShading {
    base_color: vec4<f32>,
    light_dir: vec4<f32>,
    light_ambient: vec4<f32>,
    light_diffuse: vec4<f32>,
    light_specular: vec4<f32>,
    material_ambient: vec4<f32>,
    material_diffuse: vec4<f32>,
    material_specular: vec4<f32>,
    // rgb: rim color, a: rim power.
    rim_color: vec4<f32>,
    // x: specular power, y: anisotropic specular, z: has texture,
    // w: alpha cutoff (negative disables it).
    params: vec4<f32>,
};

@group(#{MATERIAL_BIND_GROUP}) @binding(0)
var<uniform> material: AvatarShading;
@group(#{MATERIAL_BIND_GROUP}) @binding(1)
var base_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(2)
var base_sampler: sampler;

@fragment
fn fragment(
    in: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> @location(0) vec4<f32> {
    var base = material.base_color;
#ifdef VERTEX_UVS_A
    if material.params.z > 0.5 {
        base *= textureSample(base_texture, base_sampler, in.uv);
    }
#endif
    if material.params.w >= 0.0 && base.a < material.params.w {
        discard;
    }

    // Evaluate portrait light and rim in camera space, so the face
    // remains readable as the player and camera turn through the world.
    var world_normal = normalize(in.world_normal);
    if !is_front {
        world_normal = -world_normal;
    }
    let n = normalize((view.view_from_world * vec4(world_normal, 0.0)).xyz);
    let light = normalize(material.light_dir.xyz);
    let eye = vec3(0.0, 0.0, 1.0);

    // A small diffuse floor prevents the shadow side of a face from turning
    // gray and clay-like.
    let diffuse_factor = max(dot(light, n), 0.1);
    let ambient = material.light_ambient.rgb * material.material_ambient.rgb;
    let diffuse = material.light_diffuse.rgb * material.material_diffuse.rgb
        * diffuse_factor;

    let reflected = reflect(-light, n);
    let blinn = pow(max(dot(reflected, eye), 0.0), material.params.x);
    var reflection = blinn;
    var specular_strength = 1.0;
    var vertex_color = vec4<f32>(1.0);
#ifdef VERTEX_COLORS
    vertex_color = in.color;
#endif
#ifdef VERTEX_TANGENTS
    if material.params.y > 0.5 {
        let world_tangent = normalize(in.world_tangent.xyz);
        let tangent = normalize((view.view_from_world * vec4(world_tangent, 0.0)).xyz);
        let light_tangent = clamp(dot(light, tangent), -1.0, 1.0);
        let eye_tangent = clamp(dot(eye, tangent), -1.0, 1.0);
        let tangent_normal = sqrt(max(1.0 - light_tangent * light_tangent, 0.0));
        let tangent_view = sqrt(max(1.0 - eye_tangent * eye_tangent, 0.0));
        let anisotropic = pow(max(
            tangent_normal * tangent_view - light_tangent * eye_tangent,
            0.0,
        ), material.params.x);
        reflection = mix(anisotropic, blinn, vertex_color.r);
        specular_strength = vertex_color.g;
    }
#endif
    // Keep highlights inside the part's color family. Untinted white specular
    // is what makes pastel hair and the shirt read as wet plastic in HDR.
    let specular_tint = mix(vec3<f32>(1.0), base.rgb, 0.65);
    let specular = material.light_specular.rgb * material.material_specular.rgb
        * reflection * specular_strength * specular_tint;

    let rim_width = vertex_color.a;
    let rim = material.rim_color.rgb * pow(
        rim_width * (1.0 - abs(clamp(n.z, -1.0, 1.0))),
        material.rim_color.a,
    );

    let color = (ambient + diffuse) * base.rgb + specular + rim;
    return vec4(color, base.a);
}
