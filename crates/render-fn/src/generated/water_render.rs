//! Animated water for pools and fountains.

use bevy::asset::{embedded_asset, RenderAssetUsages};
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, ShaderType};
use bevy::shader::ShaderRef;

const WATER_GRID_SUBDIVISIONS: u32 = 64;

pub(crate) struct WaterRenderPlugin;

impl Plugin for WaterRenderPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "water.wgsl");
        app.add_plugins(MaterialPlugin::<WaterMaterial>::default())
            .init_resource::<WaterRenderAssets>();
    }
}

#[derive(Clone, Copy, Debug, Reflect, ShaderType)]
struct WaterSettings {
    shallow_color: Vec4,
    deep_color: Vec4,
    foam_color: Vec4,
    // x: shoreline width, y: base opacity, z: detail-normal strength,
    // w: caustic strength.
    surface: Vec4,
}

#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
pub(crate) struct WaterMaterial {
    #[uniform(0)]
    settings: WaterSettings,
}

impl Material for WaterMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://render_fn/generated/water.wgsl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "embedded://render_fn/generated/water.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Blend
    }

    fn enable_shadows() -> bool {
        false
    }
}

#[derive(Resource, Default)]
pub(crate) struct WaterRenderAssets {
    material: Option<Handle<WaterMaterial>>,
    surface_mesh: Option<Handle<Mesh>>,
}

impl WaterRenderAssets {
    pub(crate) fn material(
        &mut self,
        materials: &mut Assets<WaterMaterial>,
    ) -> Handle<WaterMaterial> {
        self.material
            .get_or_insert_with(|| {
                materials.add(WaterMaterial {
                    settings: WaterSettings {
                        shallow_color: linear_rgb8(0x27, 0xc9, 0xf0),
                        deep_color: linear_rgb8(0x06, 0x63, 0xc4),
                        foam_color: linear_rgb8(0xee, 0xf6, 0xf4),
                        surface: Vec4::new(0.060, 0.78, 0.11, 0.22),
                    },
                })
            })
            .clone()
    }

    pub(crate) fn surface_mesh(&mut self, meshes: &mut Assets<Mesh>) -> Handle<Mesh> {
        self.surface_mesh
            .get_or_insert_with(|| meshes.add(water_grid(WATER_GRID_SUBDIVISIONS)))
            .clone()
    }
}

fn linear_rgb8(red: u8, green: u8, blue: u8) -> Vec4 {
    let color = Color::srgb_u8(red, green, blue).to_linear();
    Vec4::new(color.red, color.green, color.blue, 1.0)
}

fn water_grid(subdivisions: u32) -> Mesh {
    let stride = subdivisions + 1;
    let mut positions = Vec::with_capacity((stride * stride) as usize);
    let mut normals = Vec::with_capacity((stride * stride) as usize);
    let mut uvs = Vec::with_capacity((stride * stride) as usize);
    let mut indices = Vec::with_capacity((subdivisions * subdivisions * 6) as usize);

    for z in 0..=subdivisions {
        for x in 0..=subdivisions {
            let u = x as f32 / subdivisions as f32;
            let v = z as f32 / subdivisions as f32;
            positions.push([u - 0.5, 0.0, v - 0.5]);
            normals.push([0.0, 1.0, 0.0]);
            uvs.push([u, v]);
        }
    }
    for z in 0..subdivisions {
        for x in 0..subdivisions {
            let i = z * stride + x;
            indices.extend_from_slice(&[i, i + stride, i + 1, i + 1, i + stride, i + stride + 1]);
        }
    }

    // Four segmented skirts turn the visual into the same closed volume as
    // the swim region. Their upper vertices are displaced by the wave shader,
    // so there is no gap between the animated surface and the side walls.
    let mut add_side = |start: Vec2, end: Vec2, normal: [f32; 3], reverse_winding: bool| {
        for segment in 0..subdivisions {
            let t0 = segment as f32 / subdivisions as f32;
            let t1 = (segment + 1) as f32 / subdivisions as f32;
            let a = start.lerp(end, t0);
            let b = start.lerp(end, t1);
            let base = positions.len() as u32;
            positions.extend_from_slice(&[
                [a.x, 0.0, a.y],
                [b.x, 0.0, b.y],
                [a.x, -1.0, a.y],
                [b.x, -1.0, b.y],
            ]);
            normals.extend_from_slice(&[normal; 4]);
            uvs.extend_from_slice(&[[t0, 0.0], [t1, 0.0], [t0, 1.0], [t1, 1.0]]);
            if reverse_winding {
                indices.extend_from_slice(&[
                    base,
                    base + 2,
                    base + 1,
                    base + 1,
                    base + 2,
                    base + 3,
                ]);
            } else {
                indices.extend_from_slice(&[
                    base,
                    base + 1,
                    base + 2,
                    base + 1,
                    base + 3,
                    base + 2,
                ]);
            }
        }
    };
    add_side(
        Vec2::new(-0.5, -0.5),
        Vec2::new(0.5, -0.5),
        [0.0, 0.0, -1.0],
        false,
    );
    add_side(
        Vec2::new(-0.5, 0.5),
        Vec2::new(0.5, 0.5),
        [0.0, 0.0, 1.0],
        true,
    );
    add_side(
        Vec2::new(-0.5, -0.5),
        Vec2::new(-0.5, 0.5),
        [-1.0, 0.0, 0.0],
        true,
    );
    add_side(
        Vec2::new(0.5, -0.5),
        Vec2::new(0.5, 0.5),
        [1.0, 0.0, 0.0],
        false,
    );

    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
    .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
    .with_inserted_indices(Indices::U32(indices))
}
