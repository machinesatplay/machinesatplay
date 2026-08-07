//! Stylized material support for the game's original avatar models.
//!
//! The material keeps skin warm, hair highlights controlled, and silhouettes
//! readable without making the primitive-based character look glossy or
//! photographic.

use bevy::asset::embedded_asset;
use bevy::mesh::VertexAttributeValues;
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey};
use bevy::prelude::*;
use bevy::reflect::TypePath;
use bevy::render::render_resource::{
    AsBindGroup, Face, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError,
};
use bevy::shader::ShaderRef;

use crate::ATTRIBUTE_AVATAR_COLOR;

pub(crate) struct AvatarRenderPlugin;

impl Plugin for AvatarRenderPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "avatar_material.wgsl");
        app.add_plugins(MaterialPlugin::<AvatarMaterial>::default())
            .add_systems(PreUpdate, unpack_avatar_vertex_colors);
    }
}

fn unpack_avatar_vertex_colors(
    mut meshes: ResMut<Assets<Mesh>>,
    mut events: MessageReader<AssetEvent<Mesh>>,
) {
    let ids: Vec<_> = events
        .read()
        .filter_map(|event| match event {
            AssetEvent::Added { id } | AssetEvent::Modified { id } => Some(*id),
            _ => None,
        })
        .collect();

    for id in ids {
        let Some(mut mesh) = meshes.get_mut(id) else {
            continue;
        };
        let Some(values) = mesh.remove_attribute(ATTRIBUTE_AVATAR_COLOR) else {
            continue;
        };
        match values {
            VertexAttributeValues::Unorm8x4(colors) => {
                let colors = colors
                    .into_iter()
                    .map(|color| color.map(|channel| channel as f32 / 255.0))
                    .collect::<Vec<_>>();
                mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
            }
            values => {
                mesh.insert_attribute(ATTRIBUTE_AVATAR_COLOR, values);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AvatarPart {
    Face,
    Forehead,
    Hair,
    Mask,
    NoseLine,
    Glasses,
    Body,
    Pants,
    Jacket,
    JacketTrim,
    Shoes,
}

impl AvatarPart {
    pub(crate) fn from_material_name(name: &str) -> Option<Self> {
        match name {
            "face" | "nose" => Some(Self::Face),
            "forehead" => Some(Self::Forehead),
            "hair" => Some(Self::Hair),
            "face_mask" => Some(Self::Mask),
            "nose_line" => Some(Self::NoseLine),
            "glasses" => Some(Self::Glasses),
            "body" => Some(Self::Body),
            "pants" => Some(Self::Pants),
            "jacket" => Some(Self::Jacket),
            "jacket_trim" => Some(Self::JacketTrim),
            "shoes" => Some(Self::Shoes),
            _ => None,
        }
    }

    fn stylized_parameters(self) -> (Vec3, Vec3, Vec3, f32, bool) {
        // These are hand-tuned illustration coefficients, not PBR inputs.
        match self {
            Self::Face | Self::Forehead => (
                Vec3::new(0.85, 0.75, 0.75),
                Vec3::splat(0.75),
                Vec3::splat(0.30),
                1.2,
                false,
            ),
            Self::Hair => (Vec3::ONE, Vec3::splat(0.70), Vec3::splat(0.35), 10.0, true),
            Self::Mask | Self::NoseLine | Self::Glasses => {
                (Vec3::ONE, Vec3::splat(0.70), Vec3::ZERO, 40.0, true)
            }
            Self::Body | Self::Jacket | Self::JacketTrim | Self::Shoes => (
                Vec3::splat(0.95622),
                Vec3::splat(0.49673),
                Vec3::splat(0.24099),
                3.0,
                false,
            ),
            Self::Pants => (
                Vec3::splat(0.95622),
                Vec3::splat(1.08497),
                Vec3::splat(0.24090),
                3.0,
                false,
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, ShaderType)]
struct AvatarShading {
    base_color: Vec4,
    light_dir: Vec4,
    light_ambient: Vec4,
    light_diffuse: Vec4,
    light_specular: Vec4,
    material_ambient: Vec4,
    material_diffuse: Vec4,
    material_specular: Vec4,
    rim_color: Vec4,
    // x: specular power, y: anisotropic specular, z: has texture,
    // w: alpha cutoff (negative disables it).
    params: Vec4,
}

#[derive(Asset, TypePath, AsBindGroup, Debug, Clone)]
#[bind_group_data(AvatarMaterialKey)]
pub(crate) struct AvatarMaterial {
    #[uniform(0)]
    shading: AvatarShading,
    #[texture(1)]
    #[sampler(2)]
    base_texture: Option<Handle<Image>>,
    alpha_mode: AlphaMode,
    double_sided: bool,
    depth_bias: f32,
    part: AvatarPart,
}

impl AvatarMaterial {
    pub(crate) fn from_standard(
        source: &StandardMaterial,
        part: AvatarPart,
        shirt_color: Color,
    ) -> Self {
        let base_color = match part {
            AvatarPart::Body => shirt_color,
            // Keep the assembled body's softened denim treatment rather than
            // rendering the source's flat charcoal pants.
            AvatarPart::Pants => Color::srgb(0.23, 0.29, 0.40),
            _ => source.base_color,
        }
        .to_linear();
        let (ambient, diffuse, specular, specular_power, anisotropic) = part.stylized_parameters();
        let alpha_cutoff = match source.alpha_mode {
            AlphaMode::Mask(cutoff) => cutoff,
            AlphaMode::AlphaToCoverage => 0.5,
            _ => -1.0,
        };

        Self {
            shading: AvatarShading {
                base_color: Vec4::new(
                    base_color.red,
                    base_color.green,
                    base_color.blue,
                    base_color.alpha,
                ),
                // Portrait light evaluated in camera space for stable faces.
                light_dir: Vec4::new(-0.45315, 0.42262, 0.78489, 0.0),
                light_ambient: Vec3::splat(0.73).extend(0.0),
                light_diffuse: Vec3::splat(0.60).extend(0.0),
                // The reference's 0.70 was authored for its legacy output
                // transform. In Bevy's linear HDR path that becomes a white
                // plastic hotspot, so retain the shape at a calibrated level.
                light_specular: Vec3::splat(0.40).extend(0.0),
                material_ambient: ambient.extend(0.0),
                material_diffuse: diffuse.extend(0.0),
                material_specular: specular.extend(0.0),
                rim_color: Vec3::splat(
                    if matches!(
                        part,
                        AvatarPart::Body
                            | AvatarPart::Pants
                            | AvatarPart::Jacket
                            | AvatarPart::JacketTrim
                            | AvatarPart::Shoes
                    ) {
                        0.40
                    } else {
                        0.30
                    },
                )
                .extend(2.0),
                params: Vec4::new(
                    specular_power,
                    if anisotropic { 1.0 } else { 0.0 },
                    if source.base_color_texture.is_some() {
                        1.0
                    } else {
                        0.0
                    },
                    alpha_cutoff,
                ),
            },
            base_texture: source.base_color_texture.clone(),
            alpha_mode: source.alpha_mode,
            double_sided: source.cull_mode.is_none(),
            depth_bias: source.depth_bias,
            part,
        }
    }

    pub(crate) fn set_player_color(&mut self, color: Color) {
        if self.part != AvatarPart::Body {
            return;
        }
        let color = color.to_linear();
        self.shading.base_color = Vec4::new(color.red, color.green, color.blue, color.alpha);
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct AvatarMaterialKey {
    double_sided: bool,
}

impl From<&AvatarMaterial> for AvatarMaterialKey {
    fn from(material: &AvatarMaterial) -> Self {
        Self {
            double_sided: material.double_sided,
        }
    }
}

impl Material for AvatarMaterial {
    fn fragment_shader() -> ShaderRef {
        "embedded://render_fn/generated/avatar_material.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        self.alpha_mode
    }

    fn depth_bias(&self) -> f32 {
        self.depth_bias
    }

    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &bevy::mesh::MeshVertexBufferLayoutRef,
        key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        descriptor.primitive.cull_mode = if key.bind_group_data.double_sided {
            None
        } else {
            Some(Face::Back)
        };
        Ok(())
    }
}
