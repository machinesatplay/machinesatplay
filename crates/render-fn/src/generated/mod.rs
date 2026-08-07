//! scene, material, avatar, animation, IK, sky, and camera code.

mod avatar_ik;
mod avatar_render;
mod part_render;
mod water_render;

use self::avatar_render::{AvatarMaterial, AvatarPart, AvatarRenderPlugin};
use self::part_render::{PartMaterial, PartRenderAssets, PartRenderPlugin};
use self::water_render::{WaterMaterial, WaterRenderAssets, WaterRenderPlugin};
use crate::{
    PlayerAnimationStatus, PlayerRenderStatus, RendererAnimationStatus, RendererInput,
    RendererStatus, RendererSystems, RendererTarget,
};
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::gltf::GltfMaterialName;
use bevy::light::{DirectionalLightShadowMap, NotShadowCaster, NotShadowReceiver, Skybox};
use bevy::prelude::*;
use bevy::render::render_resource::{TextureViewDescriptor, TextureViewDimension};
use render_api::RenderPlayer;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

const GROUND_Y: f32 = 0.75;
const MAP_SCALE: f32 = 0.3;

#[derive(Resource, Default)]
pub(crate) struct SemanticClock {
    revision: u64,
    elapsed: f64,
    delta: f64,
}

pub(crate) fn build(app: &mut App) {
    app.add_plugins((PartRenderPlugin, WaterRenderPlugin, AvatarRenderPlugin));
    app.add_plugins(avatar_ik::AvatarIkPlugin);
    app.init_resource::<SemanticClock>();
    app.add_systems(Startup, setup_scene);
    app.add_systems(
        Update,
        (
            prepare_skybox,
            (tag_body_parts, style_avatar_materials, mark_avatar_ready).chain(),
        ),
    );
    app.add_systems(
        PostUpdate,
        consume_renderer_input.in_set(RendererSystems::Input),
    );
    app.add_systems(
        PostUpdate,
        (
            update_avatar_colors,
            sync_player_transforms,
            animate_character,
            avatar_ik::apply_foot_ik,
            apply_sky,
            camera_follow,
            update_renderer_status,
        )
            .chain()
            .in_set(RendererSystems::Render),
    );
}

const BASE_AMBIENT: f32 = 157.0 / 255.0;
const STUD_SCALE: f32 = 0.3;
const CAMERA_OCCLUSION_NEAR_PLANE: f32 = 0.5 * STUD_SCALE;
const CAMERA_OCCLUSION_WALL_MARGIN: f32 = 0.25 * STUD_SCALE;
const CAMERA_OCCLUSION_MIN_DISTANCE: f32 = 2.15 * STUD_SCALE;

#[derive(Clone, Copy)]
struct CameraObstacle {
    center: Vec3,
    half_size: Vec3,
}

#[derive(Resource, Default)]
struct CameraObstacles(Vec<CameraObstacle>);

// With Bevy's default exposure (EV100 9.7), output = nits / ~1000, so 1000
// displays the sky texture exactly as authored. Anything lower renders the
// whole sky proportionally darker/grayer than the source image.
const SKYBOX_DAY_BRIGHTNESS: f32 = 1000.0;
const SKYBOX_NIGHT_BRIGHTNESS: f32 = 700.0;

/// Everything the day/night transition interpolates. Colors are sRGB triples.
struct SkyRig {
    skybox_brightness: f32,
    key_color: [f32; 3],
    key_illuminance: f32,
    ambient_color: [f32; 3],
    ambient_brightness: f32,
    world_sky_fill: [f32; 3],
    world_ground_fill: [f32; 3],
    world_shadow_floor: f32,
}

const DAY_RIG: SkyRig = SkyRig {
    skybox_brightness: SKYBOX_DAY_BRIGHTNESS,
    // Warm cream sun (#fff3d6).
    key_color: [1.0, 0.953, 0.839],
    key_illuminance: 7_800.0,
    ambient_color: [BASE_AMBIENT, BASE_AMBIENT, BASE_AMBIENT],
    ambient_brightness: 650.0,
    world_sky_fill: [0.72, 0.86, 1.0],
    world_ground_fill: [1.0, 0.82, 0.55],
    world_shadow_floor: 0.72,
};

const NIGHT_RIG: SkyRig = SkyRig {
    skybox_brightness: SKYBOX_NIGHT_BRIGHTNESS,
    // Blue moonlight, much dimmer than the sun.
    key_color: [0.62, 0.72, 1.0],
    key_illuminance: 1800.0,
    // Friendly readable night: cool blue ambient, never murky.
    ambient_color: [0.55, 0.62, 0.85],
    ambient_brightness: 550.0,
    world_sky_fill: [0.42, 0.54, 0.88],
    world_ground_fill: [0.22, 0.27, 0.48],
    world_shadow_floor: 0.48,
};

#[derive(Component)]
struct SunKeyLight;

#[derive(Resource)]
struct SkyboxAssets {
    day: Handle<Image>,
    night: Handle<Image>,
    day_ready: bool,
    night_ready: bool,
}

#[derive(Clone)]
struct LoadedGameWorld(render_api::GameWorld);

impl LoadedGameWorld {
    fn position(&self, part: &render_api::GamePart) -> Vec3 {
        Vec3::from_array(part.position) * self.0.world_scale
    }

    fn size(&self, part: &render_api::GamePart) -> Vec3 {
        Vec3::from_array(part.size) * self.0.world_scale
    }

    fn color(&self, part: &render_api::GamePart) -> u32 {
        u32::from_str_radix(part.color.strip_prefix('#').unwrap_or(&part.color), 16)
            .expect("validated world color")
    }

    fn material(&self, part: &render_api::GamePart) -> u8 {
        match part.material.to_ascii_lowercase().as_str() {
            "water" => 0,
            "plastic" => 1,
            "brick" => 2,
            "wood" => 3,
            "planks" => 4,
            "marble" => 5,
            "stone" | "slate" => 6,
            "concrete" => 7,
            "granite" => 8,
            "cobblestone" => 9,
            "gravel" => 10,
            "treadplate" | "tread-plate" => 11,
            "metal" => 12,
            "fabric" => 13,
            "grass" => 14,
            "sand" => 15,
            "ice" => 16,
            _ => 1,
        }
    }
}

#[derive(Component, Clone, Copy)]
pub(crate) struct VisualPlayerId(pub(crate) u64);

#[derive(Component, Clone, Copy)]
pub(crate) struct PlayerPosition(pub(crate) Vec3);

#[derive(Component, Clone, Copy)]
pub(crate) struct PlayerVelocity(pub(crate) Vec3);

#[derive(Component, Clone, Copy)]
pub(crate) struct PlayerAlive(pub(crate) bool);

#[derive(Component, Clone, Copy)]
pub(crate) struct PlayerGrounded(pub(crate) bool);

#[derive(Component, Clone, Copy)]
pub(crate) struct PlayerSwimming(pub(crate) bool);

#[derive(Component, Clone, Copy)]
pub(crate) struct PlayerClimbing(pub(crate) bool);

#[derive(Component, Clone, Copy)]
pub(crate) struct PlayerSeated(pub(crate) bool);

#[derive(Component, Clone, Copy)]
struct PlayerColor {
    color: Color,
    rgb: [u8; 3],
}

#[allow(clippy::too_many_arguments)]
fn setup_scene(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut part_materials: ResMut<Assets<PartMaterial>>,
    mut parts: ResMut<PartRenderAssets>,
    mut water_materials: ResMut<Assets<WaterMaterial>>,
    mut water: ResMut<WaterRenderAssets>,
    input: Res<RendererInput>,
    target: Res<RendererTarget>,
) {
    commands.insert_resource(SkyboxAssets {
        day: asset_server.load("sky/day_skybox.png"),
        night: asset_server.load("sky/night_skybox.png"),
        day_ready: false,
        night_ready: false,
    });
    commands.insert_resource(ClearColor(Color::srgb(0.48, 0.75, 0.88)));
    commands.insert_resource(DirectionalLightShadowMap { size: 1024 });

    let mut camera = commands.spawn((
        Name::new("Main Camera"),
        Camera3d::default(),
        Tonemapping::None,
        Transform::from_xyz(0.0, 7.0, 12.0).looking_at(Vec3::Y, Vec3::Y),
        Skybox {
            image: None,
            brightness: SKYBOX_DAY_BRIGHTNESS,
            rotation: Quat::IDENTITY,
        },
        AmbientLight {
            color: srgb3(DAY_RIG.ambient_color),
            brightness: DAY_RIG.ambient_brightness,
            ..default()
        },
    ));
    if let Some(target) = target.0.clone() {
        camera.insert(target);
    }

    commands.spawn((
        Name::new("Sun Directional Key"),
        SunKeyLight,
        DirectionalLight {
            color: srgb3(DAY_RIG.key_color),
            illuminance: DAY_RIG.key_illuminance,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_xyz(-25.0, 40.0, 18.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    let world = LoadedGameWorld(input.scene().world.clone());
    commands.insert_resource(CameraObstacles(
        world
            .0
            .parts
            .iter()
            .filter(|part| part.collidable && !part.swimmable)
            .map(|part| CameraObstacle {
                center: world.position(part),
                half_size: world.size(part) * 0.5,
            })
            .collect(),
    ));
    spawn_movement_playground(
        &mut commands,
        &asset_server,
        &mut meshes,
        &mut part_materials,
        &mut parts,
        &mut water_materials,
        &mut water,
        &world,
    );
}

#[allow(clippy::too_many_arguments)]
fn spawn_movement_playground(
    commands: &mut Commands,
    asset_server: &AssetServer,
    meshes: &mut Assets<Mesh>,
    part_materials: &mut Assets<PartMaterial>,
    parts: &mut PartRenderAssets,
    water_materials: &mut Assets<WaterMaterial>,
    water: &mut WaterRenderAssets,
    world: &LoadedGameWorld,
) {
    commands
        .spawn((
            Name::new("Movement Playground"),
            Transform::default(),
            Visibility::default(),
        ))
        .with_children(|parent| {
            for part in &world.0.parts {
                let position = world.position(part);
                let size = world.size(part);
                if part.swimmable {
                    let mesh = water.surface_mesh(meshes);
                    parent.spawn((
                        Name::new(part.name.clone()),
                        Mesh3d(mesh),
                        MeshMaterial3d(water.material(water_materials)),
                        Transform::from_translation(position + Vec3::Y * (size.y * 0.5))
                            .with_scale(size),
                        NotShadowCaster,
                        NotShadowReceiver,
                    ));
                } else {
                    let mesh = parts.primitive(meshes, size, world.0.world_scale);
                    parent.spawn((
                        Name::new(part.name.clone()),
                        Mesh3d(mesh),
                        MeshMaterial3d(parts.material(
                            asset_server,
                            part_materials,
                            world.color(part),
                            part.alpha,
                            world.material(part),
                            0,
                        )),
                        Transform::from_translation(position).with_scale(size),
                    ));
                }
            }
        });
}

fn prepare_skybox(
    asset_server: Res<AssetServer>,
    mut images: ResMut<Assets<Image>>,
    mut skybox_asset: ResMut<SkyboxAssets>,
    mut skyboxes: Query<(Entity, &mut Skybox)>,
) {
    if !skybox_asset.day_ready {
        skybox_asset.day_ready = prepare_cubemap(&skybox_asset.day, &asset_server, &mut images);
    }
    if !skybox_asset.night_ready {
        skybox_asset.night_ready = prepare_cubemap(&skybox_asset.night, &asset_server, &mut images);
    }

    // Only the initial assignment happens here; after that the day/night
    // transition animator owns the skybox image and brightness.
    if !skybox_asset.day_ready {
        return;
    }
    for (_, mut skybox) in &mut skyboxes {
        if skybox.image.is_none() {
            skybox.image = Some(skybox_asset.day.clone());
        }
    }
}

/// Turns a vertically stacked six-face PNG into a cube texture once loaded.
fn prepare_cubemap(
    handle: &Handle<Image>,
    asset_server: &AssetServer,
    images: &mut Assets<Image>,
) -> bool {
    if !asset_server.load_state(handle).is_loaded() {
        return false;
    }
    let Some(mut image) = images.get_mut(handle) else {
        return false;
    };
    if image.texture_descriptor.array_layer_count() == 1 {
        let layers = image.height() / image.width();
        image
            .reinterpret_stacked_2d_as_array(layers)
            .expect("skybox PNG should contain six square faces");
        image.texture_view_descriptor = Some(TextureViewDescriptor {
            dimension: Some(TextureViewDimension::Cube),
            ..default()
        });
    }
    true
}

fn apply_sky(
    input: Res<RendererInput>,
    mut applied: Local<AppliedSky>,
    mut material_events: MessageReader<AssetEvent<PartMaterial>>,
    skybox_asset: Res<SkyboxAssets>,
    mut cameras: Query<(&mut Skybox, &mut AmbientLight)>,
    mut key_lights: Query<&mut DirectionalLight, With<SunKeyLight>>,
    mut world_materials: ResMut<Assets<PartMaterial>>,
) {
    let Some(frame) = input.frame() else {
        return;
    };
    if !skybox_asset.day_ready || !skybox_asset.night_ready {
        return;
    }
    let t = frame.sky_blend.clamp(0.0, 1.0);
    let dip = 1.0 - 0.94 * (core::f32::consts::PI * t).sin();
    let target = if t < 0.5 {
        &skybox_asset.day
    } else {
        &skybox_asset.night
    };
    let material_added = material_events
        .read()
        .any(|event| matches!(event, AssetEvent::Added { .. }));
    if applied.blend == Some(t) && applied.target == Some(target.id()) && !material_added {
        return;
    }
    applied.blend = Some(t);
    applied.target = Some(target.id());

    for (mut skybox, mut ambient) in &mut cameras {
        skybox.image = Some(target.clone());
        skybox.brightness = lerp(DAY_RIG.skybox_brightness, NIGHT_RIG.skybox_brightness, t) * dip;
        ambient.color = srgb3(lerp3(DAY_RIG.ambient_color, NIGHT_RIG.ambient_color, t));
        ambient.brightness = lerp(DAY_RIG.ambient_brightness, NIGHT_RIG.ambient_brightness, t);
    }
    for mut light in &mut key_lights {
        light.color = srgb3(lerp3(DAY_RIG.key_color, NIGHT_RIG.key_color, t));
        light.illuminance = lerp(DAY_RIG.key_illuminance, NIGHT_RIG.key_illuminance, t);
    }
    let sky_fill = lerp3(DAY_RIG.world_sky_fill, NIGHT_RIG.world_sky_fill, t);
    let ground_fill = lerp3(DAY_RIG.world_ground_fill, NIGHT_RIG.world_ground_fill, t);
    let shadow_floor = lerp(DAY_RIG.world_shadow_floor, NIGHT_RIG.world_shadow_floor, t);
    for (_, material) in world_materials.iter_mut() {
        material
            .extension
            .set_world_fill(sky_fill, ground_fill, shadow_floor);
    }
}

#[derive(Default)]
struct AppliedSky {
    blend: Option<f32>,
    target: Option<AssetId<Image>>,
}

fn srgb3(c: [f32; 3]) -> Color {
    Color::srgb(c[0], c[1], c[2])
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        lerp(a[0], b[0], t),
        lerp(a[1], b[1], t),
        lerp(a[2], b[2], t),
    ]
}

/// Marker for player entities whose visuals have been spawned.
#[derive(Component)]

pub(crate) struct PlayerVisual;

/// Added only after both avatar scenes and every driven animation joint are
/// present. The player root stays hidden until this marker exists, preventing
/// partially loaded models and visible T-poses.
#[derive(Component)]
pub(crate) struct AvatarReady;

/// Per-player animation state, driven by how the player actually moved.
#[derive(Component, Clone, Debug, PartialEq)]
pub(crate) struct AnimState {
    /// Walk cycle phase, advanced by horizontal distance traveled.
    phase: f32,
    /// Walk swing amplitude, eased in/out with speed. 0..1.
    amplitude: f32,
    facing: Vec3,
    facing_yaw: f32,
    /// Smoothed airborne factor. 0 grounded .. 1 airborne.
    air: f32,
    airborne: bool,
    /// Smoothed blend into the dedicated water pose.
    swim: f32,
    /// Smoothed distinction between treading idle and active strokes.
    swim_motion: f32,
    /// Independent source-clip clocks. Keeping both running makes the
    /// moving/idle crossfade continuous while preserving clip timing.
    swim_time: f32,
    swim_idle_time: f32,
    /// Local X rotation that aims the Avatar's long axis along its 3D swim path.
    swim_pitch: f32,
    climb: f32,
    climb_time: f32,
    sit: f32,
    sit_time: f32,
    /// Landing squash envelope, set to 1 on touchdown and decays to 0.
    squash: f32,
}

impl AnimState {
    /// Foot planting is a locomotion correction, not an idle pose. Once the
    /// walk blend reaches zero, IK must release both feet so the authored idle
    /// basis becomes authoritative again.
    pub(crate) fn has_walk_motion(&self) -> bool {
        self.amplitude > f32::EPSILON
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum PartKind {
    LegL,
    LegR,
    ArmL,
    ArmR,
    Hip,
    Torso,
    Head,
}

/// An animatable joint of the articulated avatar body. Joints live in a
/// hierarchy—arms under `chest`, legs under `hip`, both under `skl_root`—so
/// connected primitive geometry follows each driven limb.
///
/// Every driven joint's *parent* has an identity rest rotation, so a
/// body-space pose delta is applied by premultiplying it onto the joint's
/// rest rotation.
#[derive(Component)]
pub(crate) struct BodyPart {
    pub(crate) player: Entity,
    pub(crate) kind: PartKind,
    /// Rest-pose local transform, the basis all animation is applied to.
    base: Transform,
}

/// The avatar is assembled from separate head and body models. Together they
/// span about 21.8 model units with feet anchored at the root.
pub(crate) const BODY_SCALE: f32 = 1.68 / 21.8;
const HEAD_SCALE: f32 = 0.14 * BODY_SCALE;
const HEAD_MOUNT: Vec3 = Vec3::new(0.0, 10.7766 * BODY_SCALE, 0.0176 * BODY_SCALE);
/// Children are offset down by GROUND_Y so feet touch the ground when the
/// entity sits at GROUND_Y.
const FEET_Y: f32 = -GROUND_Y;

/// Attaches the Avatar model to player entities that should be visible:
/// - Predicted (our own player)
/// - Interpolated (other players)
/// - Replicated server-authoritative entities
///
/// Confirmed replica copies get no visual, so players aren't drawn twice.
/// Predicted entities also get the FrameInterpolate marker so their motion is
/// smooth at any frame rate.
#[allow(clippy::type_complexity)]
fn consume_renderer_input(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    input: Res<RendererInput>,
    mut clock: ResMut<SemanticClock>,
    current: Query<(Entity, &VisualPlayerId), With<PlayerVisual>>,
    mut values: Query<(
        &mut PlayerPosition,
        &mut PlayerVelocity,
        &mut PlayerAlive,
        &mut PlayerGrounded,
        &mut PlayerSwimming,
        &mut PlayerClimbing,
        &mut PlayerSeated,
        &mut AnimState,
        &mut PlayerColor,
    )>,
) {
    if input.revision() == clock.revision {
        clock.delta = 0.0;
        return;
    }
    let Some(frame) = input.frame() else {
        return;
    };
    let next_time = frame.time_seconds;
    clock.delta = if clock.revision == 0 {
        0.0
    } else {
        (next_time - clock.elapsed).max(0.0)
    };
    clock.elapsed = next_time;
    clock.revision = input.revision();

    let mut by_id = HashMap::new();
    for (entity, id) in &current {
        by_id.insert(id.0, entity);
    }
    let incoming: HashSet<u64> = frame.players.iter().map(|player| player.id).collect();
    for (&id, &entity) in &by_id {
        if !incoming.contains(&id) {
            commands.entity(entity).despawn();
        }
    }

    for player in &frame.players {
        if let Some(&entity) = by_id.get(&player.id) {
            if let Ok((
                mut position,
                mut velocity,
                mut alive,
                mut grounded,
                mut swimming,
                mut climbing,
                mut seated,
                mut animation,
                mut color,
            )) = values.get_mut(entity)
            {
                position.0 = Vec3::from_array(player.position);
                velocity.0 = Vec3::from_array(player.velocity);
                alive.0 = player.alive;
                grounded.0 = player.grounded;
                swimming.0 = player.swimming;
                climbing.0 = player.climbing;
                seated.0 = player.seated;
                animation.facing = Vec3::from_array(player.facing);
                if color.rgb != player.color_rgb {
                    color.rgb = player.color_rgb;
                    color.color = Color::srgb_u8(
                        player.color_rgb[0],
                        player.color_rgb[1],
                        player.color_rgb[2],
                    );
                }
            }
        } else {
            spawn_player(&mut commands, &asset_server, player);
        }
    }
}

fn update_avatar_colors(
    players: Query<(Entity, &PlayerColor), Changed<PlayerColor>>,
    children: Query<&Children>,
    mesh_materials: Query<&MeshMaterial3d<AvatarMaterial>>,
    mut materials: ResMut<Assets<AvatarMaterial>>,
) {
    for (player, color) in &players {
        let mut descendants = vec![player];
        while let Some(entity) = descendants.pop() {
            if let Ok(material) = mesh_materials.get(entity) {
                if let Some(mut material) = materials.get_mut(&material.0) {
                    material.set_player_color(color.color);
                }
            }
            if let Ok(entity_children) = children.get(entity) {
                descendants.extend(entity_children.iter());
            }
        }
    }
}

fn spawn_player(commands: &mut Commands, asset_server: &AssetServer, player: &RenderPlayer) {
    let position = Vec3::from_array(player.position);
    let facing = Vec3::from_array(player.facing);
    let head = asset_server.load(GltfAssetLabel::Scene(0).from_asset("avatar_head.glb"));
    let body = asset_server.load(GltfAssetLabel::Scene(0).from_asset("avatar_body.glb"));
    let head_transform = Transform::from_translation(HEAD_MOUNT + Vec3::Y * FEET_Y)
        .with_scale(Vec3::splat(HEAD_SCALE));

    let player_entity = commands
        .spawn((
            Name::new(format!("Visual Player {}", player.id)),
            PlayerVisual,
            VisualPlayerId(player.id),
            PlayerColor {
                color: Color::srgb_u8(
                    player.color_rgb[0],
                    player.color_rgb[1],
                    player.color_rgb[2],
                ),
                rgb: player.color_rgb,
            },
            PlayerPosition(position),
            PlayerVelocity(Vec3::from_array(player.velocity)),
            PlayerAlive(player.alive),
            PlayerGrounded(player.grounded),
            PlayerSwimming(player.swimming),
            PlayerClimbing(player.climbing),
            PlayerSeated(player.seated),
            AnimState {
                phase: 0.0,
                amplitude: 0.0,
                facing,
                facing_yaw: facing.x.atan2(facing.z),
                air: 0.0,
                airborne: false,
                swim: 0.0,
                swim_motion: 0.0,
                swim_time: 0.0,
                swim_idle_time: 0.0,
                swim_pitch: core::f32::consts::FRAC_PI_2,
                climb: 0.0,
                climb_time: 0.0,
                sit: 0.0,
                sit_time: 0.0,
                squash: 0.0,
            },
            Transform::from_translation(position),
            Visibility::Hidden,
        ))
        .id();

    commands.entity(player_entity).with_children(|parent| {
        parent.spawn((
            WorldAssetRoot(body),
            Transform::from_xyz(0.0, FEET_Y, 0.0).with_scale(Vec3::splat(BODY_SCALE)),
        ));
        parent.spawn((
            WorldAssetRoot(head),
            head_transform,
            BodyPart {
                player: player_entity,
                kind: PartKind::Head,
                base: head_transform,
            },
        ));
    });
}

fn style_avatar_materials(
    mut commands: Commands,
    standard_materials: Res<Assets<StandardMaterial>>,
    mut avatar_materials: ResMut<Assets<AvatarMaterial>>,
    meshes: Query<(Entity, &MeshMaterial3d<StandardMaterial>, &GltfMaterialName)>,
    parents: Query<&ChildOf>,
    players: Query<&PlayerColor, With<PlayerVisual>>,
) {
    for (entity, material, material_name) in &meshes {
        let mut current = entity;
        let mut player_color = None;
        while let Ok(child_of) = parents.get(current) {
            current = child_of.parent();
            if let Ok(color) = players.get(current) {
                player_color = Some(color.color);
                break;
            }
        }
        let Some(shirt_color) = player_color else {
            continue;
        };
        let Some(part) = AvatarPart::from_material_name(material_name) else {
            continue;
        };
        let Some(source) = standard_materials.get(&material.0) else {
            continue;
        };
        let avatar = AvatarMaterial::from_standard(source, part, shirt_color);

        let mut entity_commands = commands.entity(entity);
        entity_commands
            .remove::<MeshMaterial3d<StandardMaterial>>()
            .insert(MeshMaterial3d(avatar_materials.add(avatar)));

        // Textured cosmetic overlays should not contribute duplicate shadow
        // geometry outside their visible facial features.
        if matches!(
            part,
            AvatarPart::Mask | AvatarPart::NoseLine | AvatarPart::Glasses
        ) {
            entity_commands.insert(NotShadowCaster);
        }
    }
}

/// Finds animatable joints in freshly spawned body scenes (by their joint
/// names from the baked GLB) and links them to their owning player entity.
fn tag_body_parts(
    mut commands: Commands,
    added: Query<(Entity, &Name, &Transform), Added<Name>>,
    parents: Query<&ChildOf>,
    players: Query<(), With<PlayerVisual>>,
) {
    for (entity, name, transform) in &added {
        let kind = match name.as_str() {
            "foot_l1" => PartKind::LegL,
            "foot_r1" => PartKind::LegR,
            "arm_l1" => PartKind::ArmL,
            "arm_r1" => PartKind::ArmR,
            "hip" => PartKind::Hip,
            "chest" => PartKind::Torso,
            _ => continue,
        };
        let mut current = entity;
        while let Ok(child_of) = parents.get(current) {
            current = child_of.parent();
            if players.contains(current) {
                commands.entity(entity).insert(BodyPart {
                    player: current,
                    kind,
                    base: *transform,
                });
                break;
            }
        }
    }
}

fn mark_avatar_ready(
    mut commands: Commands,
    players: Query<Entity, (With<PlayerVisual>, Without<AvatarReady>)>,
    body_parts: Query<&BodyPart>,
    material_names: Query<(Entity, &GltfMaterialName)>,
    parents: Query<&ChildOf>,
) {
    for player in &players {
        let complete_rig = body_parts
            .iter()
            .filter(|part| part.player == player)
            .count()
            >= 7;
        if !complete_rig {
            continue;
        }

        let mut has_head = false;
        let mut has_body = false;
        for (mesh, material_name) in &material_names {
            let mut current = mesh;
            let mut belongs_to_player = false;
            while let Ok(child_of) = parents.get(current) {
                current = child_of.parent();
                if current == player {
                    belongs_to_player = true;
                    break;
                }
            }
            if !belongs_to_player {
                continue;
            }
            has_head |= matches!(material_name.0.as_str(), "face" | "forehead");
            has_body |= material_name.0.as_str() == "body";
        }

        if has_head && has_body {
            commands.entity(player).insert(AvatarReady);
            info!("Avatar rig ready for player entity {player:?}");
        }
    }
}

const FULL_STRIDE_SPEED: f32 = 4.8;
const FULL_SWIM_SPEED: f32 = 4.2;
/// Ignore tiny interpolation/reconciliation corrections when deciding whether
/// locomotion is active. Without a dead zone they can hold a walk pose after
/// the player has released movement.
const LOCOMOTION_STOP_SPEED: f32 = 0.08;
const WALK_BLEND_IN_SECONDS: f32 = 0.12;
const WALK_BLEND_OUT_SECONDS: f32 = 0.08;
/// Stride cadence, in radians of walk phase per world unit traveled.
const STRIDE_RATE: f32 = 2.2;
/// Peak leg swing in radians.
const LEG_SWING: f32 = 0.7;
/// Arms swing at this fraction of the leg amplitude, in antiphase.
const ARM_SWING: f32 = 0.55;
/// Airborne: how far the arms lift out sideways (radians).
const JUMP_ARM_RAISE: f32 = 1.4;
/// Airborne: how far the legs tuck backward (radians).
const JUMP_LEG_TUCK: f32 = 0.5;
/// Landing: peak vertical squash (fraction of height).
const SQUASH_AMOUNT: f32 = 0.15;
/// Idle: breathing/sway rate (radians/sec).
const IDLE_RATE: f32 = 2.0;
/// Lower the compact visual rig until its hip pivot rests on the bench while
/// the legs rotate forward.
const SIT_VISUAL_DROP: f32 = 0.36;

/// Locomotion rotations for the compact avatar. The avatar uses one driven
/// joint per limb, so the source torso chain is accumulated into one torso
/// rotation.
const AVATAR_ANIMATIONS: &str = include_str!("../../../../assets/avatar_animations.json");

#[derive(Deserialize)]
struct AvatarAnimationFile {
    clips: Vec<AnimationClip>,
}

#[derive(Deserialize)]
struct AnimationClip {
    name: String,
    length: f32,
    looped: bool,
    keyframes: Vec<AnimationKeyframe>,
}

#[derive(Deserialize)]
struct AnimationKeyframe {
    time: f32,
    poses: AnimationSourcePose,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AnimationSourcePose {
    root: [f32; 4],
    spine: [f32; 4],
    head: [f32; 4],
    arm_l: [f32; 4],
    arm_r: [f32; 4],
    leg_l: [f32; 4],
    leg_r: [f32; 4],
}

/// One sampled pose, split per driven joint. `lower` goes to the hip joint
/// and `torso` (lower * upper) to the chest joint; because arms hang under
/// the chest and legs under the hip in the skeleton, their samples stay raw
/// and the hierarchy composes the chain. The head is a separate model parented
/// to the player root, so it keeps the fully premultiplied rotation.
#[derive(Clone, Copy)]
struct AvatarAnimationPose {
    lower: Quat,
    torso: Quat,
    head: Quat,
    arm_l: Quat,
    arm_r: Quat,
    leg_l: Quat,
    leg_r: Quat,
}

fn animation_clips() -> &'static AvatarAnimationFile {
    static CLIPS: OnceLock<AvatarAnimationFile> = OnceLock::new();
    CLIPS.get_or_init(|| {
        serde_json::from_str(AVATAR_ANIMATIONS)
            .expect("generated avatar animation data must be valid")
    })
}

fn animation_clip(name: &str) -> &'static AnimationClip {
    animation_clips()
        .clips
        .iter()
        .find(|clip| clip.name == name)
        .unwrap_or_else(|| panic!("missing generated avatar animation clip {name}"))
}

fn avatar_basis_quat(value: [f32; 4]) -> Quat {
    // The source rig faces -Z while this avatar's authored forward is +Z.
    // Conjugating by a 180-degree Y rotation maps both directions correctly:
    // (x, y, z, w) -> (-x, y, -z, w).
    Quat::from_xyzw(-value[0], value[1], -value[2], value[3]).normalize()
}

fn interpolate_source_pose(
    a: &AnimationSourcePose,
    b: &AnimationSourcePose,
    alpha: f32,
) -> AvatarAnimationPose {
    let sample = |left: [f32; 4], right: [f32; 4]| {
        avatar_basis_quat(left).slerp(avatar_basis_quat(right), alpha)
    };
    let lower = sample(a.root, b.root);
    let upper = sample(a.spine, b.spine);
    let upper_world = lower * upper;
    AvatarAnimationPose {
        lower,
        torso: upper_world,
        head: upper_world * sample(a.head, b.head),
        arm_l: sample(a.arm_l, b.arm_l),
        arm_r: sample(a.arm_r, b.arm_r),
        leg_l: sample(a.leg_l, b.leg_l),
        leg_r: sample(a.leg_r, b.leg_r),
    }
}

fn sample_animation_clip(clip: &AnimationClip, time: f32) -> AvatarAnimationPose {
    let time = if clip.looped {
        time.rem_euclid(clip.length)
    } else {
        time.clamp(0.0, clip.length)
    };
    let after = clip.keyframes.partition_point(|frame| frame.time <= time);
    let a_index = after.saturating_sub(1).min(clip.keyframes.len() - 1);
    let b_index = after.min(clip.keyframes.len() - 1);
    let a = &clip.keyframes[a_index];
    let b = &clip.keyframes[b_index];
    let alpha = if b.time > a.time {
        (time - a.time) / (b.time - a.time)
    } else {
        0.0
    };
    interpolate_source_pose(&a.poses, &b.poses, alpha)
}

fn blend_animation_pose(
    idle: AvatarAnimationPose,
    active: AvatarAnimationPose,
    alpha: f32,
) -> AvatarAnimationPose {
    AvatarAnimationPose {
        lower: idle.lower.slerp(active.lower, alpha),
        torso: idle.torso.slerp(active.torso, alpha),
        head: idle.head.slerp(active.head, alpha),
        arm_l: idle.arm_l.slerp(active.arm_l, alpha),
        arm_r: idle.arm_r.slerp(active.arm_r, alpha),
        leg_l: idle.leg_l.slerp(active.leg_l, alpha),
        leg_r: idle.leg_r.slerp(active.leg_r, alpha),
    }
}

fn pose_rotation(pose: AvatarAnimationPose, kind: PartKind) -> Quat {
    match kind {
        PartKind::LegL => pose.leg_l,
        PartKind::LegR => pose.leg_r,
        PartKind::ArmL => pose.arm_l,
        PartKind::ArmR => pose.arm_r,
        PartKind::Hip => pose.lower,
        PartKind::Torso => pose.torso,
        PartKind::Head => pose.head,
    }
}

/// Keeps the procedural idle/walk/jump/land animation and blends in the
/// retargeted swim, climb, and sit clips.
fn animate_character(
    clock: Res<SemanticClock>,
    mut players: Query<(
        &PlayerVelocity,
        &PlayerGrounded,
        &PlayerSwimming,
        &PlayerClimbing,
        &PlayerSeated,
        &mut AnimState,
    )>,
    mut parts: Query<(&BodyPart, &mut Transform)>,
) {
    let dt = clock.delta as f32;
    let t = clock.elapsed as f32;
    if dt > 0.0 {
        for (velocity, grounded, swimming, climbing, seated, mut anim) in &mut players {
            let horizontal_speed = Vec2::new(velocity.0.x, velocity.0.z).length();
            let distance = horizontal_speed * dt;
            let swim_speed = velocity.0.length();
            let swimming = swimming.0;
            let climbing = climbing.0;
            let seated = seated.0;
            let airborne = !grounded.0 && !swimming && !climbing && !seated;
            if anim.airborne && !airborne {
                anim.squash = 1.0;
            }
            anim.airborne = airborne;
            anim.squash *= 1.0 - (dt * 10.0).min(1.0);
            let air_target = if airborne { 1.0 } else { 0.0 };
            anim.air += (air_target - anim.air) * (dt * 15.0).min(1.0);
            let swim_target = if swimming && !climbing && !seated {
                1.0
            } else {
                0.0
            };
            let swim_step = (dt / 0.24).min(1.0);
            anim.swim += (swim_target - anim.swim).clamp(-swim_step, swim_step);
            let swim_motion_target = if swimming {
                (swim_speed / FULL_SWIM_SPEED).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let swim_motion_step = (dt / 0.24).min(1.0);
            anim.swim_motion +=
                (swim_motion_target - anim.swim_motion).clamp(-swim_motion_step, swim_motion_step);
            if swimming {
                anim.swim_time += dt;
                anim.swim_idle_time += dt;
            } else {
                anim.swim_time = 0.0;
                anim.swim_idle_time = 0.0;
            }
            let climb_speed = velocity.0.y.abs();
            let climbing_in_motion = climbing && climb_speed > LOCOMOTION_STOP_SPEED;
            let climb_target = if climbing && !seated { 1.0 } else { 0.0 };
            let climb_step = (dt / 0.12).min(1.0);
            anim.climb += (climb_target - anim.climb).clamp(-climb_step, climb_step);
            if climbing_in_motion {
                // Advance the source clip in proportion to vertical climb speed.
                let source_vertical_speed = climb_speed / MAP_SCALE;
                anim.climb_time += dt * source_vertical_speed / 12.0;
            } else if !climbing && anim.climb <= f32::EPSILON {
                // Reset only after detaching. While attached but stationary, hold
                // the current climbing pose instead of falling back to idle.
                anim.climb_time = 0.0;
            }
            let sit_target = if seated { 1.0 } else { 0.0 };
            let sit_step = (dt / 0.08).min(1.0);
            anim.sit += (sit_target - anim.sit).clamp(-sit_step, sit_step);
            if seated {
                anim.sit_time += dt;
            } else {
                anim.sit_time = 0.0;
            }
            let walk_target = if airborne
                || swimming
                || climbing
                || seated
                || horizontal_speed <= LOCOMOTION_STOP_SPEED
            {
                0.0
            } else {
                (horizontal_speed / FULL_STRIDE_SPEED).clamp(0.0, 1.0)
            };
            let walk_blend_seconds = if walk_target > anim.amplitude {
                WALK_BLEND_IN_SECONDS
            } else {
                WALK_BLEND_OUT_SECONDS
            };
            let walk_step = (dt / walk_blend_seconds).min(1.0);
            anim.amplitude += (walk_target - anim.amplitude).clamp(-walk_step, walk_step);
            if walk_target > 0.0 {
                anim.phase = (anim.phase + distance * STRIDE_RATE) % core::f32::consts::TAU;
            } else if anim.amplitude <= f32::EPSILON {
                // A stopped character is exactly in the idle/rest basis. Resetting
                // the phase prevents the next walk from inheriting an old step.
                anim.amplitude = 0.0;
                anim.phase = 0.0;
            }
        }
    }

    for (part, mut transform) in &mut parts {
        let Ok((_, _, _, _, _, anim)) = players.get(part.player) else {
            continue;
        };
        let squash = 1.0 - SQUASH_AMOUNT * anim.squash;
        let idle =
            (1.0 - anim.air) * (1.0 - anim.swim) * (1.0 - anim.amplitude) * (t * IDLE_RATE).sin();
        let active_swim_pose = sample_animation_clip(animation_clip("swim"), anim.swim_time);
        let idle_swim_pose =
            sample_animation_clip(animation_clip("swim_idle"), anim.swim_idle_time);
        let swim_pose = blend_animation_pose(idle_swim_pose, active_swim_pose, anim.swim_motion);
        let climb_pose = sample_animation_clip(animation_clip("climb"), anim.climb_time);
        let sit_pose = sample_animation_clip(animation_clip("sit"), anim.sit_time);
        *transform = part.base;
        let procedural_rotation = match part.kind {
            PartKind::LegL | PartKind::LegR => {
                let sign = if part.kind == PartKind::LegL {
                    1.0
                } else {
                    -1.0
                };
                let walk = anim.amplitude * LEG_SWING * anim.phase.sin() * sign;
                let tuck = anim.air * (JUMP_LEG_TUCK + 0.1 * sign);
                Quat::from_rotation_x(walk + tuck)
            }
            PartKind::ArmL | PartKind::ArmR => {
                let sign = if part.kind == PartKind::ArmL {
                    1.0
                } else {
                    -1.0
                };
                let walk = anim.amplitude * LEG_SWING * ARM_SWING * anim.phase.sin() * -sign;
                let sway = 0.06 * idle;
                let raise = anim.air * JUMP_ARM_RAISE * sign;
                Quat::from_rotation_z(raise) * Quat::from_rotation_x(walk + sway)
            }
            PartKind::Hip => Quat::IDENTITY,
            PartKind::Torso => {
                // Squash/breathe scale on the chest carries the arms and the
                // skinned upper body with it through the joint hierarchy.
                let breathe = 1.0 + 0.012 * idle;
                let bulge = 1.0 + 0.4 * (1.0 - squash);
                transform.scale = part.base.scale * Vec3::new(bulge, squash * breathe, bulge);
                Quat::IDENTITY
            }
            PartKind::Head => {
                let above_feet = part.base.translation.y - FEET_Y;
                transform.translation.y = FEET_Y
                    + above_feet * squash
                    + 0.008 * idle
                    + anim.amplitude * 0.01 * (anim.phase * 2.0).sin();
                Quat::IDENTITY
            }
        };
        let rotation = procedural_rotation
            .slerp(pose_rotation(swim_pose, part.kind), anim.swim)
            .slerp(pose_rotation(climb_pose, part.kind), anim.climb)
            .slerp(pose_rotation(sit_pose, part.kind), anim.sit);
        // Pose deltas are body-space rotations; the driven joints' parents all
        // rest at identity, so premultiplying onto the rest rotation swings
        // the joint about the body axes regardless of its authored twist.
        transform.rotation = rotation * part.base.rotation;
        // Limbs inherit the sit drop through the hierarchy, so it is applied
        // only at the roots: hip + chest (body units) and the head model
        // (player units).
        let sit_drop = match part.kind {
            PartKind::Head => SIT_VISUAL_DROP,
            PartKind::Hip | PartKind::Torso => SIT_VISUAL_DROP / BODY_SCALE,
            _ => 0.0,
        };
        transform.translation.y -= sit_drop * anim.sit;
    }
}

#[allow(clippy::type_complexity)]
fn sync_player_transforms(
    mut players: Query<
        (
            &PlayerPosition,
            &PlayerAlive,
            &mut Transform,
            &mut Visibility,
            &mut AnimState,
            Has<AvatarReady>,
        ),
        With<PlayerVisual>,
    >,
) {
    for (position, alive, mut transform, mut visibility, mut anim, ready) in &mut players {
        *visibility = if alive.0 && ready {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        if !alive.0 {
            continue;
        }
        transform.translation = position.0;
        let facing = anim.facing.try_normalize().unwrap_or(Vec3::Z);
        anim.facing_yaw = facing.x.atan2(facing.z);
        anim.swim_pitch = core::f32::consts::FRAC_PI_2 - facing.y.clamp(-1.0, 1.0).asin();
        let swim_pitch = anim.swim_pitch * anim.swim * anim.swim_motion;
        transform.rotation =
            Quat::from_rotation_y(anim.facing_yaw) * Quat::from_rotation_x(swim_pitch);
    }
}

fn camera_follow(
    input: Res<RendererInput>,
    obstacles: Res<CameraObstacles>,
    mut cameras: Query<&mut Transform, With<Camera3d>>,
) {
    let (Some(frame), Ok(mut camera_transform)) = (input.frame(), cameras.single_mut()) else {
        return;
    };
    let focus = Vec3::from_array(frame.camera.focus);
    let orbit = &frame.camera;
    let offset = Vec3::new(
        orbit.radius * orbit.pitch.cos() * orbit.yaw.sin(),
        orbit.radius * orbit.pitch.sin(),
        orbit.radius * orbit.pitch.cos() * orbit.yaw.cos(),
    );
    let desired_position = focus + offset;
    let ray_length = offset.length();
    let Some(ray_direction) = offset.try_normalize() else {
        *camera_transform =
            Transform::from_translation(desired_position).looking_at(focus, Vec3::Y);
        return;
    };

    let mut resolved_distance = ray_length;
    for obstacle in &obstacles.0 {
        if let Some(hit_distance) = ray_aabb_distance(
            focus,
            ray_direction,
            ray_length,
            obstacle.center,
            obstacle.half_size + Vec3::splat(CAMERA_OCCLUSION_NEAR_PLANE),
        ) {
            resolved_distance = resolve_camera_hit_distance(resolved_distance, hit_distance);
        }
    }

    let resolved_position = focus + ray_direction * resolved_distance;
    *camera_transform = Transform::from_translation(resolved_position).looking_at(focus, Vec3::Y);
}

fn ray_aabb_distance(
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
    center: Vec3,
    half_size: Vec3,
) -> Option<f32> {
    let minimum = center - half_size;
    let maximum = center + half_size;
    let mut entry = 0.0_f32;
    let mut exit = max_distance;

    for axis in 0..3 {
        if direction[axis].abs() <= f32::EPSILON {
            if origin[axis] < minimum[axis] || origin[axis] > maximum[axis] {
                return None;
            }
            continue;
        }
        let inverse = direction[axis].recip();
        let first = (minimum[axis] - origin[axis]) * inverse;
        let second = (maximum[axis] - origin[axis]) * inverse;
        entry = entry.max(first.min(second));
        exit = exit.min(first.max(second));
        if entry > exit {
            return None;
        }
    }

    (entry <= max_distance).then_some(entry)
}

fn update_renderer_status(
    assets: Option<Res<SkyboxAssets>>,
    asset_server: Res<AssetServer>,
    part_assets: Res<PartRenderAssets>,
    mut status: ResMut<RendererStatus>,
    mut animation_status: ResMut<RendererAnimationStatus>,
    players: Query<(&VisualPlayerId, &AnimState, Has<AvatarReady>)>,
    world_assets: Query<&WorldAssetRoot>,
) {
    let mut player_status = BTreeMap::new();
    let mut player_animation_status = BTreeMap::new();
    for (id, animation, avatar_ready) in &players {
        player_animation_status.insert(
            id.0,
            PlayerAnimationStatus {
                walk_phase: animation.phase,
                walk_amount: animation.amplitude,
            },
        );
        player_status.insert(
            id.0,
            PlayerRenderStatus {
                avatar_ready,
                locomotion: animation
                    .amplitude
                    .max(animation.swim_motion)
                    .max(animation.climb),
            },
        );
    }
    animation_status.players = player_animation_status;
    let sky_ready = assets
        .as_ref()
        .is_some_and(|assets| assets.day_ready && assets.night_ready);
    status.ready = sky_ready && part_assets.is_ready(&asset_server);
    status.error = part_assets
        .load_failure(&asset_server)
        .or_else(|| {
            assets.as_ref().and_then(|assets| {
                [&assets.day, &assets.night].into_iter().find_map(|handle| {
                    match asset_server.load_state(handle) {
                        bevy::asset::LoadState::Failed(error) => Some(error.to_string()),
                        _ => None,
                    }
                })
            })
        })
        .or_else(|| {
            world_assets
                .iter()
                .find_map(|root| match asset_server.load_state(&root.0) {
                    bevy::asset::LoadState::Failed(error) => Some(error.to_string()),
                    _ => match asset_server.dependency_load_state(&root.0) {
                        bevy::asset::DependencyLoadState::Failed(error) => Some(error.to_string()),
                        _ => match asset_server.recursive_dependency_load_state(&root.0) {
                            bevy::asset::RecursiveDependencyLoadState::Failed(error) => {
                                Some(error.to_string())
                            }
                            _ => None,
                        },
                    },
                })
        });
    status.players = player_status;
}

fn resolve_camera_hit_distance(current_distance: f32, hit_distance: f32) -> f32 {
    current_distance
        .min((hit_distance - CAMERA_OCCLUSION_WALL_MARGIN).max(CAMERA_OCCLUSION_MIN_DISTANCE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::asset::{AssetMetaCheck, AssetPlugin};
    use render_api::{RenderCamera, RenderScene};

    fn anim_state(_position: Vec3, facing_yaw: f32) -> AnimState {
        AnimState {
            phase: 0.0,
            amplitude: 0.0,
            facing: Vec3::Z,
            facing_yaw,
            air: 0.0,
            airborne: false,
            swim: 0.0,
            swim_motion: 0.0,
            swim_time: 0.0,
            swim_idle_time: 0.0,
            swim_pitch: core::f32::consts::FRAC_PI_2,
            climb: 0.0,
            climb_time: 0.0,
            sit: 0.0,
            sit_time: 0.0,
            squash: 0.0,
        }
    }

    fn scene() -> RenderScene {
        RenderScene::from_world_json(
            r##"{
                "schemaVersion": 2,
                "worldScale": 0.3,
                "killPlane": -500.0,
                "spawnPoints": [[0.0, 0.0]],
                "parts": [{
                    "name": "floor",
                    "position": [0.0, 0.0, 0.0],
                    "size": [10.0, 1.0, 10.0],
                    "color": "#a3a3a3",
                    "material": "plastic"
                }]
            }"##,
        )
        .expect("valid test scene")
    }

    fn frame(time_seconds: f64, players: Vec<RenderPlayer>) -> render_api::RenderFrame {
        render_api::RenderFrame {
            time_seconds,
            players,
            camera: RenderCamera {
                focus: [0.0, 1.0, 0.0],
                yaw: 0.0,
                pitch: 0.45,
                radius: 9.0,
            },
            sky_blend: 0.0,
        }
    }

    fn player(id: u64, position: [f32; 3]) -> RenderPlayer {
        RenderPlayer {
            id,
            position,
            velocity: [0.0; 3],
            color_rgb: [12, 34, 56],
            alive: true,
            grounded: true,
            swimming: false,
            climbing: false,
            seated: false,
            facing: [0.0, 0.0, 1.0],
        }
    }

    #[test]
    fn facing_does_not_depend_on_observer_history() {
        let mut app = App::new();
        app.add_systems(Update, sync_player_transforms);
        let spawn_copy = |world: &mut World, prior_yaw| {
            world
                .spawn((
                    PlayerVisual,
                    PlayerPosition(Vec3::new(0.0, 1.5, -3.6)),
                    PlayerAlive(true),
                    Transform::default(),
                    Visibility::Inherited,
                    anim_state(Vec3::new(0.0, 1.5, -3.6), prior_yaw),
                ))
                .id()
        };
        let local = spawn_copy(app.world_mut(), -1.2);
        let remote = spawn_copy(app.world_mut(), 0.8);

        app.update();

        let local_yaw = app.world().get::<AnimState>(local).unwrap().facing_yaw;
        let remote_yaw = app.world().get::<AnimState>(remote).unwrap().facing_yaw;
        assert!((local_yaw - remote_yaw).abs() < 1e-6);
        assert!(local_yaw.abs() < 1e-6);
        assert!(app
            .world()
            .get::<Transform>(local)
            .unwrap()
            .rotation
            .abs_diff_eq(app.world().get::<Transform>(remote).unwrap().rotation, 1e-6));
    }

    #[test]
    fn camera_hit_distance_stays_in_front_of_walls_and_respects_minimum() {
        let wall = resolve_camera_hit_distance(9.0, 4.0);
        assert!((wall - (4.0 - CAMERA_OCCLUSION_WALL_MARGIN)).abs() < 1e-6);
        let near = resolve_camera_hit_distance(9.0, 0.1);
        assert!((near - CAMERA_OCCLUSION_MIN_DISTANCE).abs() < 1e-6);
        assert!((resolve_camera_hit_distance(9.0, 20.0) - 9.0).abs() < 1e-6);
    }

    #[test]
    fn camera_obstacle_ray_finds_only_boxes_on_the_segment() {
        let hit = ray_aabb_distance(
            Vec3::ZERO,
            Vec3::Z,
            10.0,
            Vec3::new(0.0, 0.0, 5.0),
            Vec3::ONE,
        );
        assert_eq!(hit, Some(4.0));
        assert_eq!(
            ray_aabb_distance(
                Vec3::ZERO,
                Vec3::Z,
                10.0,
                Vec3::new(3.0, 0.0, 5.0),
                Vec3::ONE,
            ),
            None
        );
        assert_eq!(
            ray_aabb_distance(
                Vec3::ZERO,
                Vec3::Z,
                3.0,
                Vec3::new(0.0, 0.0, 5.0),
                Vec3::ONE,
            ),
            None
        );
    }

    #[test]
    fn imported_locomotion_clips_are_complete_and_sampleable() {
        for name in ["swim", "swim_idle", "climb", "sit"] {
            let clip = animation_clip(name);
            assert!(clip.length > 0.0 && !clip.keyframes.is_empty());
            let pose = sample_animation_clip(clip, clip.length * 0.37);
            for rotation in [
                pose.torso, pose.head, pose.arm_l, pose.arm_r, pose.leg_l, pose.leg_r,
            ] {
                assert!(rotation.is_finite());
                assert!((rotation.length() - 1.0).abs() < 1e-4);
            }
        }
        assert!(animation_clips().clips.iter().all(|clip| clip.looped));
    }

    #[test]
    fn every_movement_state_drives_its_animation() {
        let mut app = App::new();
        app.insert_resource(SemanticClock {
            revision: 1,
            elapsed: 0.1,
            delta: 0.1,
        });
        app.add_systems(Update, animate_character);

        let spawn = |world: &mut World,
                     velocity: Vec3,
                     grounded: bool,
                     swimming: bool,
                     climbing: bool,
                     seated: bool| {
            world
                .spawn((
                    PlayerVelocity(velocity),
                    PlayerGrounded(grounded),
                    PlayerSwimming(swimming),
                    PlayerClimbing(climbing),
                    PlayerSeated(seated),
                    anim_state(Vec3::ZERO, 0.0),
                ))
                .id()
        };
        let walking = spawn(
            app.world_mut(),
            Vec3::X * FULL_STRIDE_SPEED,
            true,
            false,
            false,
            false,
        );
        let jumping = spawn(app.world_mut(), Vec3::Y * 4.0, false, false, false, false);
        let swimming = spawn(
            app.world_mut(),
            Vec3::Z * FULL_SWIM_SPEED,
            false,
            true,
            false,
            false,
        );
        let climbing = spawn(app.world_mut(), Vec3::Y * 2.0, false, false, true, false);
        let seated = spawn(app.world_mut(), Vec3::ZERO, true, false, false, true);

        app.update();

        let walk = app.world().get::<AnimState>(walking).unwrap();
        assert!(walk.amplitude > 0.0 && walk.phase > 0.0);

        let jump = app.world().get::<AnimState>(jumping).unwrap();
        assert!(jump.airborne && jump.air > 0.0);

        let swim = app.world().get::<AnimState>(swimming).unwrap();
        assert!(swim.swim > 0.0 && swim.swim_motion > 0.0 && swim.swim_time > 0.0);

        let climb = app.world().get::<AnimState>(climbing).unwrap();
        assert!(climb.climb > 0.0 && climb.climb_time > 0.0);

        let sit = app.world().get::<AnimState>(seated).unwrap();
        assert!(sit.sit > 0.0 && sit.sit_time > 0.0);
    }

    #[test]
    fn avatar_basis_and_horizontal_swim_pitch_match_the_authored_axes() {
        let half = core::f32::consts::FRAC_PI_4;
        let converted = avatar_basis_quat([half.sin(), 0.0, 0.0, half.cos()]);
        assert!(converted.abs_diff_eq(Quat::from_rotation_x(-core::f32::consts::FRAC_PI_2), 1e-5,));
        let horizontal = Quat::from_rotation_x(core::f32::consts::FRAC_PI_2);
        assert!((horizontal * Vec3::Y).abs_diff_eq(Vec3::Z, 1e-5));
        assert!((horizontal * Vec3::Z).abs_diff_eq(Vec3::NEG_Y, 1e-5));
    }

    #[test]
    fn authoritative_removal_despawns_and_reappearance_resets_animation() {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin {
                meta_check: AssetMetaCheck::Never,
                ..default()
            },
        ));
        app.init_asset::<WorldAsset>();
        app.insert_resource(RendererInput::new(scene()));
        app.init_resource::<SemanticClock>();
        app.add_systems(Update, consume_renderer_input);

        app.world_mut()
            .resource_mut::<RendererInput>()
            .submit(frame(0.0, vec![player(7, [1.0, 2.0, 3.0])]));
        app.update();
        let first = {
            let mut query = app.world_mut().query::<(Entity, &VisualPlayerId)>();
            query
                .iter(app.world())
                .find_map(|(entity, id)| (id.0 == 7).then_some(entity))
                .expect("first visual")
        };
        app.world_mut().get_mut::<AnimState>(first).unwrap().phase = 2.0;

        app.world_mut()
            .resource_mut::<RendererInput>()
            .submit(frame(1.0, Vec::new()));
        app.update();
        assert!(app.world().get_entity(first).is_err());

        app.world_mut()
            .resource_mut::<RendererInput>()
            .submit(frame(2.0, vec![player(7, [4.0, 2.0, 3.0])]));
        app.update();
        let second = {
            let mut query = app.world_mut().query::<(Entity, &VisualPlayerId)>();
            query
                .iter(app.world())
                .find_map(|(entity, id)| (id.0 == 7).then_some(entity))
                .expect("reappeared visual")
        };
        assert_ne!(first, second);
        let animation = app.world().get::<AnimState>(second).unwrap();
        assert_eq!(animation.phase, 0.0);
        assert_eq!(
            app.world().get::<PlayerPosition>(second).unwrap().0,
            Vec3::new(4.0, 2.0, 3.0)
        );
    }

    #[test]
    fn same_id_color_change_preserves_visual_and_animation_history() {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin {
                meta_check: AssetMetaCheck::Never,
                ..default()
            },
        ));
        app.init_asset::<WorldAsset>();
        app.insert_resource(RendererInput::new(scene()));
        app.init_resource::<SemanticClock>();
        app.add_systems(Update, consume_renderer_input);

        app.world_mut()
            .resource_mut::<RendererInput>()
            .submit(frame(0.0, vec![player(7, [1.0, 2.0, 3.0])]));
        app.update();
        let visual = {
            let mut query = app.world_mut().query::<(Entity, &VisualPlayerId)>();
            query
                .iter(app.world())
                .find_map(|(entity, id)| (id.0 == 7).then_some(entity))
                .expect("visual")
        };
        app.world_mut().get_mut::<AnimState>(visual).unwrap().phase = 2.0;

        let mut recolored = player(7, [4.0, 2.0, 3.0]);
        recolored.color_rgb = [200, 100, 50];
        app.world_mut()
            .resource_mut::<RendererInput>()
            .submit(frame(1.0, vec![recolored]));
        app.update();

        let retained = {
            let mut query = app.world_mut().query::<(Entity, &VisualPlayerId)>();
            query
                .iter(app.world())
                .find_map(|(entity, id)| (id.0 == 7).then_some(entity))
                .expect("retained visual")
        };
        assert_eq!(retained, visual);
        assert_eq!(app.world().get::<AnimState>(retained).unwrap().phase, 2.0);
        assert_eq!(
            app.world().get::<PlayerColor>(retained).unwrap().rgb,
            [200, 100, 50]
        );
    }

    #[test]
    fn zero_delta_pumps_apply_pose_without_advancing_animation_state() {
        let mut app = App::new();
        app.insert_resource(SemanticClock {
            revision: 1,
            elapsed: 0.25,
            delta: 0.25,
        });
        app.add_systems(Update, animate_character);
        let position = Vec3::new(0.5, GROUND_Y, 0.0);
        let visual = app
            .world_mut()
            .spawn((
                PlayerVelocity(position),
                PlayerGrounded(true),
                PlayerSwimming(false),
                PlayerClimbing(false),
                PlayerSeated(false),
                anim_state(Vec3::ZERO, 0.0),
            ))
            .id();
        app.world_mut().spawn((
            BodyPart {
                player: visual,
                kind: PartKind::LegL,
                base: Transform::default(),
            },
            Transform::default(),
        ));

        app.update();
        let advanced = app.world().get::<AnimState>(visual).unwrap().clone();
        app.world_mut().resource_mut::<SemanticClock>().delta = 0.0;
        app.update();
        app.update();
        assert_eq!(*app.world().get::<AnimState>(visual).unwrap(), advanced);
    }
}
