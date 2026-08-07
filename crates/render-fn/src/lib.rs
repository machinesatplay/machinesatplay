//! rendering systems for mach scenes, avatars, materials, and captures.

#[cfg(not(target_family = "wasm"))]
mod capture;
mod generated;
mod gltf_skinning;

use bevy::{
    gltf::GltfPlugin, mesh::MeshVertexAttribute, prelude::*, render::render_resource::VertexFormat,
    transform::TransformSystems,
};
use std::{collections::BTreeMap, path::PathBuf};

pub use gltf_skinning::RendererGltfExtensionsPlugin;
pub use render_api::*;

const ATTRIBUTE_AVATAR_COLOR: MeshVertexAttribute =
    MeshVertexAttribute::new("AvatarColor", 1_733_645_921, VertexFormat::Unorm8x4);

/// Stable ordering points for the host's ECS-to-render bridge.
#[derive(Clone, Debug, Hash, PartialEq, Eq, SystemSet)]
pub enum RendererSystems {
    Input,
    Render,
}

/// Latest authoritative visual input and its monotonically increasing revision.
#[derive(Resource, Clone, Debug)]
pub struct RendererInput {
    scene: RenderScene,
    frame: Option<RenderFrame>,
    revision: u64,
}

impl RendererInput {
    pub fn new(scene: RenderScene) -> Self {
        Self {
            scene,
            frame: None,
            revision: 0,
        }
    }

    pub fn submit(&mut self, mut frame: RenderFrame) -> u64 {
        frame.players.sort_by_key(|player| player.id);
        self.frame = Some(frame);
        self.revision = self.revision.wrapping_add(1).max(1);
        self.revision
    }

    pub fn scene(&self) -> &RenderScene {
        &self.scene
    }

    pub fn frame(&self) -> Option<&RenderFrame> {
        self.frame.as_ref()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlayerRenderStatus {
    pub avatar_ready: bool,
    pub locomotion: f32,
}

#[derive(Resource, Clone, Debug, Default)]
pub struct RendererStatus {
    pub ready: bool,
    pub error: Option<String>,
    pub players: BTreeMap<u64, PlayerRenderStatus>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlayerAnimationStatus {
    pub walk_phase: f32,
    pub walk_amount: f32,
}

/// Per-player animation timing shared with presentation systems such as audio.
#[derive(Resource, Clone, Debug, Default)]
pub struct RendererAnimationStatus {
    pub players: BTreeMap<u64, PlayerAnimationStatus>,
}

#[derive(Resource, Clone, Default)]
pub(crate) struct RendererTarget(pub(crate) Option<bevy::camera::RenderTarget>);

/// Installs the generated visual implementation around the stable input/status seam.
pub struct RendererPlugin {
    scene: RenderScene,
    target: Option<bevy::camera::RenderTarget>,
}

impl RendererPlugin {
    pub fn new(scene: RenderScene) -> Self {
        Self {
            scene,
            target: None,
        }
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn with_target(mut self, target: bevy::camera::RenderTarget) -> Self {
        self.target = Some(target);
        self
    }
}

impl Plugin for RendererPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<RendererGltfExtensionsPlugin>() {
            app.add_plugins(RendererGltfExtensionsPlugin);
        }
        app.insert_resource(RendererInput::new(self.scene.clone()))
            .init_resource::<RendererStatus>()
            .init_resource::<RendererAnimationStatus>()
            .insert_resource(RendererTarget(self.target.clone()))
            .configure_sets(
                PostUpdate,
                (RendererSystems::Input, RendererSystems::Render)
                    .chain()
                    .before(TransformSystems::Propagate),
            );
        generated::build(app);
    }
}

/// The glTF configuration required by both live and headless renderer apps.
pub fn renderer_gltf_plugin() -> GltfPlugin {
    GltfPlugin::default().add_custom_vertex_attribute("COLOR", ATTRIBUTE_AVATAR_COLOR)
}

/// Assets bundled beside a source checkout of this repository.
pub fn source_asset_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("assets")
}

#[cfg(not(target_family = "wasm"))]
pub use capture::{render, RenderError, RenderOptions};
