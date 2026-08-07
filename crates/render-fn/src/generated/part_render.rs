//! Engine-level classic part geometry and material rendering for Bevy.
//!
//! This module owns the reusable translation from part primitives and
//! built-in surface materials to Bevy meshes and `StandardMaterial` assets.
//! Individual games only supply scene data: shape, size, transform, color,
//! opacity, and material ID.

use bevy::asset::{embedded_asset, AssetId};
use bevy::image::{ImageAddressMode, ImageLoaderSettings, ImageSampler, ImageSamplerDescriptor};
use bevy::math::Affine2;
use bevy::mesh::VertexAttributeValues;
use bevy::pbr::{ExtendedMaterial, MaterialExtension};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, ShaderType, TextureDataOrder, TextureDimension, TextureFormat,
};
use bevy::shader::ShaderRef;
use serde::Deserialize;
use std::collections::HashMap;

const MATERIAL_MANIFEST: &str = include_str!("../../../../assets/part_materials/library.json");

pub(crate) struct PartRenderPlugin;

impl Plugin for PartRenderPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "part_material.wgsl");
        app.add_plugins(MaterialPlugin::<PartMaterial>::default())
            .init_resource::<PartRenderAssets>()
            .add_systems(PreUpdate, prepare_part_material_mipmaps);
    }
}

/// Compact fixed-function-style controls for the bright illustrated world look.
/// Colors are lighting multipliers; `base_tint` is linear scene color.
#[derive(Clone, Copy, Debug, Reflect, ShaderType)]
struct StylizedWorldShading {
    // x: normal strength, y: texture-detail strength, z: shadow-floor strength,
    // w: normal-map shading distance.
    params: Vec4,
    base_tint: Vec4,
    sky_fill: Vec4,
    ground_fill: Vec4,
}

/// The material retains Bevy's shadows and environment reflections, but its
/// relief, texture contrast, and dark-side response are deliberately bounded
/// like a stylized fixed-function material rather than fully photographic.
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
pub(crate) struct PartMaterialExtension {
    #[uniform(100)]
    shading: StylizedWorldShading,
}

impl PartMaterialExtension {
    pub(crate) fn set_world_fill(
        &mut self,
        sky_fill: [f32; 3],
        ground_fill: [f32; 3],
        shadow_floor: f32,
    ) {
        self.shading.sky_fill = Vec3::from_array(sky_fill).extend(0.0);
        self.shading.ground_fill = Vec3::from_array(ground_fill).extend(0.0);
        self.shading.params.z = shadow_floor;
    }
}

impl MaterialExtension for PartMaterialExtension {
    fn fragment_shader() -> ShaderRef {
        "embedded://render_fn/generated/part_material.wgsl".into()
    }

    fn deferred_fragment_shader() -> ShaderRef {
        "embedded://render_fn/generated/part_material.wgsl".into()
    }
}

pub(crate) type PartMaterial = ExtendedMaterial<StandardMaterial, PartMaterialExtension>;

#[derive(Deserialize)]
struct MaterialManifest {
    materials: Vec<MaterialDefinition>,
}

#[derive(Deserialize)]
struct MaterialDefinition {
    name: String,
    slug: Option<String>,
    tiling: f32,
    roughness: f32,
}

#[derive(Clone)]
struct MaterialMaps {
    diffuse: Handle<Image>,
    normal: Handle<Image>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MaterialMapKind {
    Diffuse,
    Normal,
}

#[derive(Resource)]
pub(crate) struct PartRenderAssets {
    definitions: Vec<MaterialDefinition>,
    maps: HashMap<u8, MaterialMaps>,
    pending_mipmaps: HashMap<AssetId<Image>, MaterialMapKind>,
    materials: HashMap<(u32, u8, u8, u8), Handle<PartMaterial>>,
    primitives: HashMap<([u32; 3], u32), Handle<Mesh>>,
}

impl FromWorld for PartRenderAssets {
    fn from_world(_world: &mut World) -> Self {
        let manifest: MaterialManifest = serde_json::from_str(MATERIAL_MANIFEST)
            .expect("part material manifest should be valid");
        Self {
            definitions: manifest.materials,
            maps: HashMap::new(),
            pending_mipmaps: HashMap::new(),
            materials: HashMap::new(),
            primitives: HashMap::new(),
        }
    }
}

fn prepare_part_material_mipmaps(
    mut parts: ResMut<PartRenderAssets>,
    mut images: ResMut<Assets<Image>>,
) {
    let mut completed = Vec::new();
    for (&asset_id, &kind) in &parts.pending_mipmaps {
        let Some(mut image) = images.get_mut(asset_id) else {
            continue;
        };
        match generate_material_mipmaps(&mut image, kind) {
            Ok(()) => completed.push(asset_id),
            Err(reason) => {
                warn!("Unable to generate part material {kind:?} mipmaps: {reason}");
                completed.push(asset_id);
            }
        }
    }
    for asset_id in completed {
        parts.pending_mipmaps.remove(&asset_id);
    }
}

fn generate_material_mipmaps(image: &mut Image, kind: MaterialMapKind) -> Result<(), String> {
    if image.texture_descriptor.mip_level_count > 1 {
        return Ok(());
    }
    if image.texture_descriptor.dimension != TextureDimension::D2
        || image.texture_descriptor.size.depth_or_array_layers != 1
    {
        return Err("expected a single-layer 2D image".into());
    }
    let format = image.texture_descriptor.format;
    if !matches!(
        format,
        TextureFormat::Rgba8Unorm | TextureFormat::Rgba8UnormSrgb
    ) {
        return Err(format!("expected RGBA8 data, received {format:?}"));
    }
    let width = image.texture_descriptor.size.width;
    let height = image.texture_descriptor.size.height;
    let expected_base_len = width as usize * height as usize * 4;
    let Some(base_data) = image.data.as_ref() else {
        return Err("image has no CPU pixel data".into());
    };
    if base_data.len() != expected_base_len {
        return Err(format!(
            "expected {expected_base_len} base-level bytes, received {}",
            base_data.len()
        ));
    }

    let (data, mip_level_count) = build_mip_chain(base_data, width, height, kind, format.is_srgb());
    image.data = Some(data);
    image.data_order = TextureDataOrder::MipMajor;
    image.texture_descriptor.mip_level_count = mip_level_count;
    Ok(())
}

fn build_mip_chain(
    base_data: &[u8],
    base_width: u32,
    base_height: u32,
    kind: MaterialMapKind,
    is_srgb: bool,
) -> (Vec<u8>, u32) {
    let mut chain = base_data.to_vec();
    let mut previous = base_data.to_vec();
    let mut width = base_width;
    let mut height = base_height;
    let mut mip_level_count = 1;

    while width > 1 || height > 1 {
        let next_width = (width / 2).max(1);
        let next_height = (height / 2).max(1);
        let next = downsample_rgba8_level(
            &previous,
            width,
            height,
            next_width,
            next_height,
            kind,
            is_srgb,
        );
        chain.extend_from_slice(&next);
        previous = next;
        width = next_width;
        height = next_height;
        mip_level_count += 1;
    }

    (chain, mip_level_count)
}

fn downsample_rgba8_level(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
    kind: MaterialMapKind,
    is_srgb: bool,
) -> Vec<u8> {
    let mut target = Vec::with_capacity(target_width as usize * target_height as usize * 4);
    for target_y in 0..target_height {
        for target_x in 0..target_width {
            let mut sum = [0.0_f32; 4];
            for offset_y in 0..2 {
                for offset_x in 0..2 {
                    let source_x = (target_x * 2 + offset_x).min(source_width - 1);
                    let source_y = (target_y * 2 + offset_y).min(source_height - 1);
                    let index = ((source_y * source_width + source_x) * 4) as usize;
                    if kind == MaterialMapKind::Normal {
                        sum[0] += source[index] as f32 / 127.5 - 1.0;
                        sum[1] += source[index + 1] as f32 / 127.5 - 1.0;
                        sum[2] += source[index + 2] as f32 / 127.5 - 1.0;
                    } else {
                        for channel in 0..3 {
                            let value = source[index + channel] as f32 / 255.0;
                            sum[channel] += if is_srgb {
                                srgb_to_linear(value)
                            } else {
                                value
                            };
                        }
                    }
                    sum[3] += source[index + 3] as f32 / 255.0;
                }
            }

            if kind == MaterialMapKind::Normal {
                let normal = (Vec3::new(sum[0], sum[1], sum[2]) * 0.25).normalize_or(Vec3::Z);
                target.extend_from_slice(&[
                    encode_unit_vector_component(normal.x),
                    encode_unit_vector_component(normal.y),
                    encode_unit_vector_component(normal.z),
                    ((sum[3] * 0.25).clamp(0.0, 1.0) * 255.0).round() as u8,
                ]);
            } else {
                target.extend_from_slice(&[
                    encode_color(sum[0] * 0.25, is_srgb),
                    encode_color(sum[1] * 0.25, is_srgb),
                    encode_color(sum[2] * 0.25, is_srgb),
                    ((sum[3] * 0.25).clamp(0.0, 1.0) * 255.0).round() as u8,
                ]);
            }
        }
    }
    target
}

fn srgb_to_linear(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn encode_color(value: f32, is_srgb: bool) -> u8 {
    let encoded = if !is_srgb {
        value
    } else if value <= 0.0031308 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    (encoded.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn encode_unit_vector_component(value: f32) -> u8 {
    ((value * 0.5 + 0.5).clamp(0.0, 1.0) * 255.0).round() as u8
}

impl PartRenderAssets {
    fn ensure_maps(&mut self, asset_server: &AssetServer, material_id: u8) {
        if self.maps.contains_key(&material_id) {
            return;
        }
        let Some(slug) = self
            .definitions
            .get(usize::from(material_id))
            .and_then(|definition| definition.slug.clone())
        else {
            return;
        };
        let root = format!("part_materials/{slug}");
        let diffuse = load_repeating_image(asset_server, format!("{root}/diffuse.png"), true);
        let normal = load_repeating_image(asset_server, format!("{root}/normal.png"), false);
        self.pending_mipmaps
            .insert(diffuse.id(), MaterialMapKind::Diffuse);
        self.pending_mipmaps
            .insert(normal.id(), MaterialMapKind::Normal);
        self.maps
            .insert(material_id, MaterialMaps { diffuse, normal });
    }

    pub(crate) fn is_ready(&self, asset_server: &AssetServer) -> bool {
        self.pending_mipmaps.is_empty()
            && self.maps.values().all(|maps| {
                asset_server.load_state(&maps.diffuse).is_loaded()
                    && asset_server.load_state(&maps.normal).is_loaded()
            })
    }

    pub(crate) fn load_failure(&self, asset_server: &AssetServer) -> Option<String> {
        self.maps.values().find_map(|maps| {
            [&maps.diffuse, &maps.normal]
                .into_iter()
                .find_map(|handle| match asset_server.load_state(handle) {
                    bevy::asset::LoadState::Failed(error) => Some(error.to_string()),
                    _ => None,
                })
        })
    }

    pub(crate) fn material(
        &mut self,
        asset_server: &AssetServer,
        materials: &mut Assets<PartMaterial>,
        rgb: u32,
        alpha: u8,
        material_id: u8,
        reflectance: u8,
    ) -> Handle<PartMaterial> {
        let key = (rgb, alpha, material_id, reflectance);
        if let Some(handle) = self.materials.get(&key) {
            return handle.clone();
        }

        self.ensure_maps(asset_server, material_id);
        let definition = self
            .definitions
            .get(usize::from(material_id))
            .unwrap_or(&self.definitions[0]);
        let red = ((rgb >> 16) & 0xff) as u8;
        let green = ((rgb >> 8) & 0xff) as u8;
        let blue = (rgb & 0xff) as u8;
        let color = Color::srgba_u8(red, green, blue, alpha);
        let linear_color = color.to_linear();
        let material_maps = self.maps.get(&material_id);
        let mut material = StandardMaterial {
            base_color: color,
            base_color_texture: material_maps.map(|maps| maps.diffuse.clone()),
            normal_map_texture: material_maps.map(|maps| maps.normal.clone()),
            // The stored maps use the opposite green-channel convention.
            flip_normal_map_y: material_maps.is_some(),
            perceptual_roughness: definition.roughness,
            // The material name does not select a metallic workflow. Input
            // reflectance controls the environment mix.
            metallic: 0.0,
            reflectance: f32::from(reflectance) / 255.0 * 0.5,
            uv_transform: Affine2::from_scale(Vec2::splat(definition.tiling)),
            alpha_mode: if alpha < 255 {
                AlphaMode::Blend
            } else {
                AlphaMode::Opaque
            },
            ..default()
        };

        match definition.name.as_str() {
            "ice" => {
                material.specular_transmission = 0.08;
                material.ior = 1.31;
            }
            _ => {}
        }

        let handle = materials.add(PartMaterial {
            base: material,
            extension: PartMaterialExtension {
                shading: StylizedWorldShading {
                    params: Vec4::new(
                        stylized_normal_strength(material_id),
                        stylized_texture_detail(material_id),
                        0.72,
                        // Fade mapped normals after twelve world units to keep
                        // distant ground texture relief from becoming noise.
                        40.0 * 0.3,
                    ),
                    base_tint: Vec4::new(
                        linear_color.red,
                        linear_color.green,
                        linear_color.blue,
                        linear_color.alpha,
                    ),
                    // Soft hemisphere fill: open blue sky above,
                    // warm sunlit ground below. These are light multipliers,
                    // intentionally not converted to linear display colors.
                    sky_fill: Vec4::new(0.72, 0.86, 1.0, 0.0),
                    ground_fill: Vec4::new(1.0, 0.82, 0.55, 0.0),
                },
            },
        });
        self.materials.insert(key, handle.clone());
        handle
    }

    /// Returns a cached unit primitive whose UV density represents its final
    /// size in source units. The caller still applies `world_size` as the
    /// entity's transform scale.
    pub(crate) fn primitive(
        &mut self,
        meshes: &mut Assets<Mesh>,
        world_size: Vec3,
        world_scale: f32,
    ) -> Handle<Mesh> {
        let key = (
            world_size.to_array().map(f32::to_bits),
            world_scale.to_bits(),
        );
        if let Some(handle) = self.primitives.get(&key) {
            return handle.clone();
        }

        let mut mesh = Mesh::from(Cuboid::new(1.0, 1.0, 1.0));
        apply_part_uvs(&mut mesh, world_size / world_scale.max(f32::EPSILON));
        mesh.generate_tangents()
            .expect("part primitives should support generated tangents");
        let handle = meshes.add(mesh);
        self.primitives.insert(key, handle.clone());
        handle
    }
}

fn stylized_normal_strength(material_id: u8) -> f32 {
    match material_id {
        14 => 0.16,    // grass: visible surface grain without harsh PBR relief
        1 => 0.0,      // plastic/smooth authored-color geometry
        3 | 4 => 0.12, // wood
        6 => 0.12,     // stone
        12 => 0.10,    // metal
        _ => 0.15,
    }
}

fn stylized_texture_detail(material_id: u8) -> f32 {
    match material_id {
        14 => 0.75,
        1 => 0.0,
        3 | 4 => 0.40,
        6 => 0.35,
        12 => 0.25,
        _ => 0.40,
    }
}

fn load_repeating_image(asset_server: &AssetServer, path: String, is_srgb: bool) -> Handle<Image> {
    asset_server
        .load_builder()
        .with_settings(move |settings: &mut ImageLoaderSettings| {
            settings.is_srgb = is_srgb;
            settings.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
                address_mode_u: ImageAddressMode::Repeat,
                address_mode_v: ImageAddressMode::Repeat,
                anisotropy_clamp: 8,
                ..ImageSamplerDescriptor::linear()
            });
        })
        .load(path)
}

fn apply_part_uvs(mesh: &mut Mesh, texture_size: Vec3) {
    let positions = match mesh.attribute(Mesh::ATTRIBUTE_POSITION) {
        Some(VertexAttributeValues::Float32x3(values)) => values.clone(),
        _ => panic!("part primitive is missing float3 positions"),
    };
    let normals = match mesh.attribute(Mesh::ATTRIBUTE_NORMAL) {
        Some(VertexAttributeValues::Float32x3(values)) => values.clone(),
        _ => panic!("part primitive is missing float3 normals"),
    };
    let uvs = positions
        .iter()
        .zip(&normals)
        .map(|(position, normal)| {
            projected_face_uv(
                Vec3::from_array(*position),
                Vec3::from_array(*normal),
                texture_size,
            )
            .to_array()
        })
        .collect::<Vec<_>>();
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
}

fn projected_face_uv(position: Vec3, normal: Vec3, texture_size: Vec3) -> Vec2 {
    let position = position * texture_size;
    let normal = normal.abs();
    if normal.x >= normal.y && normal.x >= normal.z {
        Vec2::new(position.z, position.y)
    } else if normal.y >= normal.z {
        Vec2::new(position.x, position.z)
    } else {
        Vec2::new(position.x, position.y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_matches_importer_contract() {
        let manifest: MaterialManifest = serde_json::from_str(MATERIAL_MANIFEST).unwrap();
        assert_eq!(manifest.materials.len(), 17);
        assert_eq!(manifest.materials[3].name, "wood");
        assert_eq!(manifest.materials[11].name, "treadplate");
        assert_eq!(manifest.materials[16].name, "ice");
    }

    #[test]
    fn projected_uvs_are_measured_in_source_units() {
        assert_eq!(
            projected_face_uv(Vec3::new(0.5, 0.5, 0.5), Vec3::Z, Vec3::new(10.0, 4.0, 2.0)),
            Vec2::new(5.0, 2.0)
        );
    }

    #[test]
    fn mip_chain_reaches_one_pixel() {
        let base = vec![128; 4 * 2 * 4];
        let (chain, levels) = build_mip_chain(&base, 4, 2, MaterialMapKind::Diffuse, true);
        assert_eq!(levels, 3);
        assert_eq!(chain.len(), (4 * 2 + 2 + 1) * 4);
    }

    #[test]
    fn diffuse_mips_average_in_linear_light() {
        let black = [0, 0, 0, 255];
        let white = [255, 255, 255, 255];
        let source = [black, white, black, white].concat();
        let mip = downsample_rgba8_level(&source, 2, 2, 1, 1, MaterialMapKind::Diffuse, true);
        // Linear 50% gray encodes to approximately sRGB 188, not 128.
        assert!(mip[0].abs_diff(188) <= 1);
        assert_eq!(mip[0], mip[1]);
        assert_eq!(mip[1], mip[2]);
        assert_eq!(mip[3], 255);
    }

    #[test]
    fn normal_mips_renormalize_vectors() {
        // Two pairs of opposing 45-degree tangent normals should average to
        // the flat +Z direction after vector-aware filtering.
        let positive_x = [218, 128, 218, 255];
        let negative_x = [37, 128, 218, 255];
        let source = [positive_x, negative_x, positive_x, negative_x].concat();
        let mip = downsample_rgba8_level(&source, 2, 2, 1, 1, MaterialMapKind::Normal, false);
        assert!(mip[0].abs_diff(128) <= 1);
        assert!(mip[1].abs_diff(128) <= 1);
        assert_eq!(mip[2], 255);
        assert_eq!(mip[3], 255);
    }
}
