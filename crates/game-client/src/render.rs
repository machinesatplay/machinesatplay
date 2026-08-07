//! Live-game presentation bridge for the reusable renderer.
//!
//! Networking and simulation stay in the host. Each visual frame is reduced
//! to the stable `render-fn` input contract after Lightyear interpolation.

use avian3d::prelude::{LinearVelocity, Position};
use bevy::color::ColorToPacked;
use bevy::diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin};
use bevy::input::mouse::{AccumulatedMouseMotion, AccumulatedMouseScroll};
use bevy::prelude::*;
use bevy::window::{CursorIcon, CursorOptions, CustomCursor, CustomCursorImage, PrimaryWindow};
use game_core::physics::{CHARACTER_HEIGHT, CHARACTER_RADIUS};
use game_core::protocol::*;
use lightyear::frame_interpolation::{
    FrameInterpolate, FrameInterpolationPlugin, FrameInterpolationSystems,
};
use lightyear::prelude::input::native::InputMarker;
use lightyear::prelude::*;
use render_fn::{
    RenderCamera, RenderFrame, RenderPlayer, RenderScene, RendererInput, RendererPlugin,
    RendererStatus, RendererSystems,
};
use std::collections::{BTreeMap, HashMap, HashSet};

pub struct GameRenderPlugin;

impl Plugin for GameRenderPlugin {
    fn build(&self, app: &mut App) {
        let scene = RenderScene {
            world: starter_render_world(),
        };

        app.add_plugins(FrameTimeDiagnosticsPlugin {
            max_history_length: 120,
            smoothing_factor: 0.5,
        });
        app.world_mut()
            .resource_mut::<InterpolationRegistry>()
            .set_interpolation::<PlayerPresentation>(interpolate_player_presentation);
        app.add_plugins(FrameInterpolationPlugin::<PlayerPresentation>::default());
        app.add_plugins(RendererPlugin::new(scene));
        app.add_plugins(crate::character_audio::CharacterAudioPlugin);
        app.init_resource::<OrbitCamera>();
        app.init_resource::<SubmittedRenderCamera>();
        app.init_resource::<HostRenderClock>();
        app.init_resource::<SkyTransition>();
        app.init_resource::<PhysicsDebugOverlay>();
        app.add_systems(Startup, setup_host_presentation);
        app.add_systems(
            Update,
            (
                sky_toggle_interaction,
                (apply_replicated_sky_state, animate_sky_transition).chain(),
                physics_debug_interaction,
                toggle_physics_debug_keyboard,
                draw_physics_debug_overlay,
                update_fps_counter,
                maintain_joining_overlay,
                apply_custom_cursor,
                orbit_camera_input,
                enable_player_presentation,
                add_world_object_visuals,
            ),
        );
        app.add_systems(
            FixedPostUpdate,
            update_player_presentation
                .before(FrameInterpolationSystems::Update)
                .run_if(not(is_in_rollback)),
        );
        app.add_systems(
            PostUpdate,
            submit_render_frame
                .after(FrameInterpolationSystems::Interpolate)
                .after(RollbackSystems::VisualCorrection)
                .before(RendererSystems::Input),
        );
    }
}

fn starter_render_world() -> render_fn::GameWorld {
    use game_core::starter_map::{KILL_PLANE_WORLD_Y, MAP_SCALE, PLAYGROUND_SPAWNS, STARTER_PARTS};

    render_fn::GameWorld {
        schema: None,
        schema_version: game_format::WORLD_SCHEMA_VERSION,
        world_scale: MAP_SCALE,
        kill_plane: KILL_PLANE_WORLD_Y / MAP_SCALE,
        spawn_points: PLAYGROUND_SPAWNS
            .iter()
            .map(|spawn| [spawn.x / MAP_SCALE, spawn.y / MAP_SCALE])
            .collect(),
        parts: STARTER_PARTS
            .iter()
            .map(|part| render_fn::GamePart {
                name: part.name.to_owned(),
                position: part.source_position.to_array(),
                size: part.source_size.to_array(),
                color: format!("#{:06x}", part.color),
                material: starter_material_name(part.material_id).to_owned(),
                alpha: part.alpha,
                collidable: part.collidable,
                swimmable: part.swimmable,
                climbable: part.climbable,
                seat: part.seat,
            })
            .collect(),
    }
}

fn starter_material_name(id: u8) -> &'static str {
    match id {
        0 => "water",
        1 => "plastic",
        3 => "wood",
        4 => "planks",
        6 => "stone",
        12 => "metal",
        14 => "grass",
        _ => "plastic",
    }
}

#[derive(Component)]
struct WorldObjectVisual {
    shape: WorldObjectShape,
    tint: u32,
}

fn add_world_object_visuals(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    objects: Query<
        (Entity, &WorldObject, Option<&WorldObjectVisual>),
        Or<(Without<WorldObjectVisual>, Changed<WorldObject>)>,
    >,
) {
    for (entity, object, visual) in &objects {
        if visual.is_some_and(|visual| visual.shape == object.shape && visual.tint == object.tint) {
            continue;
        }
        commands.entity(entity).insert((
            WorldObjectVisual {
                shape: object.shape.clone(),
                tint: object.tint,
            },
            Mesh3d(meshes.add(world_object_mesh(&object.shape))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: world_object_color(object.tint),
                perceptual_roughness: 0.65,
                ..default()
            })),
            Visibility::default(),
        ));
    }
}

fn world_object_mesh(shape: &WorldObjectShape) -> Mesh {
    match shape {
        WorldObjectShape::Sphere(radius) => Mesh::from(Sphere::new(*radius)),
        WorldObjectShape::Capsule {
            radius,
            half_height,
        } => Mesh::from(Capsule3d::new(*radius, half_height * 2.0)),
        WorldObjectShape::Cuboid(half) => {
            Mesh::from(Cuboid::new(half.x * 2.0, half.y * 2.0, half.z * 2.0))
        }
    }
}

fn world_object_color(rgba: u32) -> Color {
    Color::srgba_u8(
        (rgba >> 24) as u8,
        (rgba >> 16) as u8,
        (rgba >> 8) as u8,
        rgba as u8,
    )
}

/// Render-only pose for a player entity. This is not replicated and never
/// replaces the fixed-tick gameplay state in `Player`.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub(crate) struct PlayerPresentation {
    pub(crate) facing: Vec3,
}

impl From<&Player> for PlayerPresentation {
    fn from(player: &Player) -> Self {
        Self {
            facing: player.facing,
        }
    }
}

fn interpolate_player_presentation(
    start: PlayerPresentation,
    end: PlayerPresentation,
    t: f32,
) -> PlayerPresentation {
    let facing = start.facing.lerp(end.facing, t);
    PlayerPresentation {
        facing: facing.try_normalize().unwrap_or(end.facing),
    }
}

/// Predicted and server-authoritative entities advance in FixedUpdate.
/// Lightyear's interpolated replicas already arrive with a smoothed pose.
fn enable_player_presentation(
    mut commands: Commands,
    players: Query<(Entity, &Player, Has<Predicted>, Has<Replicate>), Without<PlayerPresentation>>,
) {
    for (entity, player, predicted, replicate) in &players {
        if predicted || replicate {
            commands.entity(entity).insert((
                PlayerPresentation::from(player),
                FrameInterpolate::<PlayerPresentation>::default(),
            ));
        }
    }
}

fn update_player_presentation(
    mut players: Query<
        (&Player, &mut PlayerPresentation),
        With<FrameInterpolate<PlayerPresentation>>,
    >,
) {
    for (player, mut presentation) in &mut players {
        *presentation = PlayerPresentation::from(player);
    }
}

/// Simulation entity selected as the source for one rendered player ID.
///
/// This is host state rather than a renderer marker. Audio, debug drawing,
/// and reliability probes use it to avoid producing duplicate effects when a
/// host world contains server, predicted, and interpolated copies together.
#[derive(Component)]
pub(crate) struct PresentedPlayer;

/// The selected presentation entity for an ID owned by this client.
///
/// Lightyear may place the input marker on a different replica copy than the
/// one selected for presentation, so ownership is aggregated by logical ID.
#[derive(Component)]
pub(crate) struct PresentedLocalPlayer;

/// Default third-person orbit camera. Input and movement direction both read
/// this host resource; `render-fn` receives a value copy each frame.
#[derive(Resource)]
pub struct OrbitCamera {
    pub focus: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub radius: f32,
    target_radius: f32,
}

impl Default for OrbitCamera {
    fn default() -> Self {
        Self {
            focus: Vec3::Y * 0.5,
            yaw: 0.0,
            pitch: 0.45,
            radius: 9.0,
            target_radius: 9.0,
        }
    }
}

#[derive(Resource, Clone)]
struct SubmittedRenderCamera(RenderCamera);

impl Default for SubmittedRenderCamera {
    fn default() -> Self {
        let offset = Vec3::new(0.0, 6.0, 12.0);
        Self(RenderCamera {
            focus: Vec3::Y.to_array(),
            yaw: 0.0,
            pitch: (offset.y / offset.length()).asin(),
            radius: offset.length(),
        })
    }
}

#[derive(Resource, Default)]
struct HostRenderClock {
    started_at: Option<f64>,
    last_frame: Option<f64>,
}

impl HostRenderClock {
    fn sample(&mut self, elapsed: f64) -> f64 {
        let started_at = *self.started_at.get_or_insert(elapsed);
        let mut relative = (elapsed - started_at).max(0.0);
        if let Some(last) = self.last_frame {
            if relative <= last {
                relative = last + f64::EPSILON.max(last.abs() * f64::EPSILON);
            }
        }
        self.last_frame = Some(relative);
        relative
    }
}

#[derive(Resource, Default)]
struct SkyTransition {
    blend: f32,
    start_blend: f32,
    target_blend: f32,
    progress: Option<f32>,
    synced: bool,
}

impl SkyTransition {
    fn apply_authoritative_state(&mut self, night: bool) {
        let target = if night { 1.0 } else { 0.0 };
        if !self.synced {
            self.blend = target;
            self.start_blend = target;
            self.target_blend = target;
            self.progress = None;
            self.synced = true;
        } else if self.target_blend != target {
            self.start_blend = self.blend;
            self.target_blend = target;
            self.progress = Some(0.0);
        }
    }
}

const SKY_TRANSITION_SECONDS: f32 = 2.5;

#[derive(Component)]
struct SkyToggleButton;

#[derive(Component)]
struct SkyToggleLabel;

#[derive(Resource)]
pub(crate) struct PhysicsDebugOverlay {
    pub(crate) enabled: bool,
}

impl Default for PhysicsDebugOverlay {
    fn default() -> Self {
        Self {
            enabled: std::env::var("MACH_DEBUG_OVERLAY").is_ok(),
        }
    }
}

#[derive(Component)]
struct PhysicsDebugButton;

#[derive(Component)]
struct PhysicsDebugLabel;

#[derive(Component)]
struct FpsCounter;

#[derive(Component)]
struct JoiningOverlay;

fn setup_host_presentation(mut commands: Commands) {
    spawn_sky_toggle_ui(&mut commands);
    spawn_physics_debug_ui(&mut commands);
    spawn_fps_counter(&mut commands);
    spawn_joining_overlay(&mut commands);
}

fn spawn_joining_overlay(commands: &mut Commands) {
    commands.spawn((
        Name::new("Joining World Overlay"),
        JoiningOverlay,
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(0.0),
            right: Val::Px(0.0),
            top: Val::Px(0.0),
            bottom: Val::Px(0.0),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            ..default()
        },
        BackgroundColor(Color::srgba(0.03, 0.05, 0.08, 0.72)),
        ZIndex(100),
        children![(
            Text::new("Joining world…"),
            TextFont {
                font_size: FontSize::Px(28.0),
                ..default()
            },
            TextColor(Color::WHITE),
        )],
    ));
}

fn maintain_joining_overlay(
    mut commands: Commands,
    synced: Query<(), (With<Client>, With<IsSynced<InputTimeline>>)>,
    local_players: Query<&Player, With<InputMarker<Inputs>>>,
    renderer: Res<RendererStatus>,
    overlays: Query<Entity, With<JoiningOverlay>>,
) {
    let playable = renderer_playable(
        !synced.is_empty(),
        local_players.iter().map(|player| player.id.to_bits()),
        &renderer,
    );
    if playable {
        for overlay in &overlays {
            commands.entity(overlay).despawn();
        }
    } else if overlays.is_empty() {
        spawn_joining_overlay(&mut commands);
    }
}

fn renderer_playable(
    synced: bool,
    local_player_ids: impl IntoIterator<Item = u64>,
    renderer: &RendererStatus,
) -> bool {
    synced
        && local_player_ids.into_iter().any(|player_id| {
            renderer
                .players
                .get(&player_id)
                .is_some_and(|status| status.avatar_ready)
        })
}

fn spawn_sky_toggle_ui(commands: &mut Commands) {
    commands.spawn((
        Name::new("Sky Toggle Button"),
        Button,
        SkyToggleButton,
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(48.0),
            right: Val::Px(12.0),
            padding: UiRect::axes(Val::Px(10.0), Val::Px(6.0)),
            border_radius: BorderRadius::all(Val::Px(8.0)),
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55)),
        children![(
            SkyToggleLabel,
            Text::new("Night"),
            TextFont {
                font_size: FontSize::Px(14.0),
                ..default()
            },
            TextColor(Color::WHITE),
        )],
    ));
}

fn sky_toggle_interaction(
    transition: Res<SkyTransition>,
    renderer: Res<RendererStatus>,
    sky: Query<&WorldSkyState>,
    mut clients: Query<&mut MessageSender<SetWorldSky>, With<Client>>,
    mut buttons: Query<
        (&Interaction, &mut BackgroundColor),
        (Changed<Interaction>, With<SkyToggleButton>),
    >,
) {
    for (interaction, mut background) in &mut buttons {
        match interaction {
            Interaction::Pressed => {
                if !renderer.ready || transition.progress.is_some() {
                    continue;
                }
                let Ok(sky) = sky.single() else {
                    continue;
                };
                for mut sender in &mut clients {
                    sender.send::<GameCommandChannel>(SetWorldSky { night: !sky.night });
                }
                *background = BackgroundColor(Color::srgba(0.25, 0.25, 0.25, 0.75));
            }
            Interaction::Hovered => {
                *background = BackgroundColor(Color::srgba(0.12, 0.12, 0.12, 0.65));
            }
            Interaction::None => {
                *background = BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55));
            }
        }
    }
}

fn apply_replicated_sky_state(
    sky: Query<&WorldSkyState, Changed<WorldSkyState>>,
    mut transition: ResMut<SkyTransition>,
    mut labels: Query<&mut Text, With<SkyToggleLabel>>,
) {
    let Ok(sky) = sky.single() else {
        return;
    };
    transition.apply_authoritative_state(sky.night);
    if let Ok(mut label) = labels.single_mut() {
        label.0 = if sky.night {
            "Day".to_string()
        } else {
            "Night".to_string()
        };
    }
}

fn animate_sky_transition(time: Res<Time>, mut transition: ResMut<SkyTransition>) {
    let Some(progress) = transition.progress else {
        return;
    };
    let progress = (progress + time.delta_secs() / SKY_TRANSITION_SECONDS).min(1.0);
    let smooth = progress * progress * (3.0 - 2.0 * progress);
    transition.blend =
        transition.start_blend + (transition.target_blend - transition.start_blend) * smooth;
    transition.progress = (progress < 1.0).then_some(progress);
}

fn spawn_fps_counter(commands: &mut Commands) {
    commands.spawn((
        Name::new("FPS Counter"),
        FpsCounter,
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(12.0),
            right: Val::Px(12.0),
            padding: UiRect::axes(Val::Px(10.0), Val::Px(6.0)),
            border_radius: BorderRadius::all(Val::Px(8.0)),
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55)),
        Text::new("fps: --"),
        TextFont {
            font_size: FontSize::Px(14.0),
            ..default()
        },
        TextColor(Color::WHITE),
    ));
}

fn update_fps_counter(
    time: Res<Time>,
    diagnostics: Res<DiagnosticsStore>,
    mut counters: Query<&mut Text, With<FpsCounter>>,
    mut refresh_elapsed: Local<f32>,
) {
    *refresh_elapsed += time.delta_secs();
    if *refresh_elapsed < 0.25 {
        return;
    }
    *refresh_elapsed %= 0.25;

    let Some(fps) = diagnostics
        .get(&FrameTimeDiagnosticsPlugin::FPS)
        .and_then(|diagnostic| diagnostic.smoothed())
    else {
        return;
    };
    for mut counter in &mut counters {
        counter.0 = format!("fps: {fps:.0}");
    }
}

fn spawn_physics_debug_ui(commands: &mut Commands) {
    commands.spawn((
        Name::new("Debug Button"),
        Button,
        PhysicsDebugButton,
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(120.0),
            right: Val::Px(12.0),
            padding: UiRect::axes(Val::Px(10.0), Val::Px(6.0)),
            border_radius: BorderRadius::all(Val::Px(8.0)),
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55)),
        children![(
            PhysicsDebugLabel,
            Text::new("Debug: Off (F9)"),
            TextFont {
                font_size: FontSize::Px(14.0),
                ..default()
            },
            TextColor(Color::WHITE),
        )],
    ));
}

fn set_physics_debug_enabled(
    debug: &mut PhysicsDebugOverlay,
    enabled: bool,
    labels: &mut Query<&mut Text, With<PhysicsDebugLabel>>,
) {
    debug.enabled = enabled;
    if let Ok(mut label) = labels.single_mut() {
        label.0 = if enabled {
            "Debug: On (F9)".to_string()
        } else {
            "Debug: Off (F9)".to_string()
        };
    }
}

fn toggle_physics_debug_keyboard(
    keys: Res<ButtonInput<KeyCode>>,
    mut debug: ResMut<PhysicsDebugOverlay>,
    mut labels: Query<&mut Text, With<PhysicsDebugLabel>>,
) {
    if keys.just_pressed(KeyCode::F9) {
        let enabled = !debug.enabled;
        set_physics_debug_enabled(&mut debug, enabled, &mut labels);
    }
}

fn physics_debug_interaction(
    mut debug: ResMut<PhysicsDebugOverlay>,
    mut buttons: Query<
        (&Interaction, &mut BackgroundColor),
        (Changed<Interaction>, With<PhysicsDebugButton>),
    >,
    mut labels: Query<&mut Text, With<PhysicsDebugLabel>>,
) {
    for (interaction, mut background) in &mut buttons {
        match interaction {
            Interaction::Pressed => {
                let enabled = !debug.enabled;
                set_physics_debug_enabled(&mut debug, enabled, &mut labels);
                *background = BackgroundColor(Color::srgba(0.25, 0.25, 0.25, 0.75));
            }
            Interaction::Hovered => {
                *background = BackgroundColor(Color::srgba(0.12, 0.12, 0.12, 0.65));
            }
            Interaction::None => {
                *background = BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55));
            }
        }
    }
}

fn draw_physics_debug_overlay(
    debug: Res<PhysicsDebugOverlay>,
    mut gizmos: Gizmos,
    players: Query<(&Player, &Position), With<PresentedPlayer>>,
) {
    if !debug.enabled {
        return;
    }

    for part in game_core::starter_map::STARTER_PARTS {
        let transform = Transform::from_translation(part.position()).with_scale(part.size());
        if part.collidable {
            gizmos.cube(transform, Color::srgb(1.0, 0.82, 0.08));
        }
        if part.swimmable {
            gizmos.cube(transform, Color::srgb(0.0, 0.85, 1.0));
        }
        if part.climbable {
            gizmos.cube(transform, Color::srgb(1.0, 0.1, 0.85));
        }
        if part.seat {
            gizmos.cube(transform, Color::srgb(1.0, 0.42, 0.05));
        }
    }

    let capsule = Capsule3d::new(CHARACTER_RADIUS, CHARACTER_HEIGHT - CHARACTER_RADIUS * 2.0);
    for (player, position) in &players {
        if player.alive {
            gizmos
                .primitive_3d(
                    &capsule,
                    Isometry3d::new(position.0, Quat::IDENTITY),
                    Color::srgb(1.0, 0.08, 0.08),
                )
                .resolution(20);
        }
    }
}

#[derive(Debug)]
struct RenderCandidate {
    entity: Entity,
    priority: u8,
    local: bool,
    player: RenderPlayer,
}

fn presentation_priority(predicted: bool, interpolated: bool, replicate: bool) -> Option<u8> {
    if predicted {
        Some(3)
    } else if interpolated {
        Some(2)
    } else if replicate {
        Some(1)
    } else {
        None
    }
}

fn canonical_candidates(
    candidates: impl IntoIterator<Item = RenderCandidate>,
) -> BTreeMap<u64, RenderCandidate> {
    let mut canonical = BTreeMap::<u64, RenderCandidate>::new();
    for candidate in candidates {
        match canonical.entry(candidate.player.id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(candidate);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let current = entry.get();
                if candidate.priority > current.priority
                    || (candidate.priority == current.priority
                        && candidate.entity.index_u32() < current.entity.index_u32())
                {
                    entry.insert(candidate);
                }
            }
        }
    }
    canonical
}

fn render_player(
    player: &Player,
    position: &Position,
    velocity: &LinearVelocity,
    presentation: Option<&PlayerPresentation>,
) -> RenderPlayer {
    let facing = presentation.map_or(player.facing, |presentation| presentation.facing);
    RenderPlayer {
        id: player.id.to_bits(),
        position: position.0.to_array(),
        velocity: velocity.0.to_array(),
        color_rgb: player.color.to_srgba().to_u8_array_no_alpha(),
        alive: player.alive,
        grounded: player.grounded,
        swimming: player.swimming,
        climbing: player.climbing,
        seated: player.seated.is_some(),
        facing: facing.to_array(),
    }
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn submit_render_frame(
    mut commands: Commands,
    time: Res<Time>,
    mut clock: ResMut<HostRenderClock>,
    mut renderer: ResMut<RendererInput>,
    renderer_status: Res<RendererStatus>,
    mut orbit: ResMut<OrbitCamera>,
    mut submitted_camera: ResMut<SubmittedRenderCamera>,
    sky: Res<SkyTransition>,
    players: Query<(
        Entity,
        &Player,
        &Position,
        &LinearVelocity,
        Option<&PlayerPresentation>,
        Has<Predicted>,
        Has<Interpolated>,
        Has<Replicate>,
        Has<InputMarker<Inputs>>,
        Has<PresentedPlayer>,
        Has<PresentedLocalPlayer>,
    )>,
) {
    let local_player_ids: HashSet<u64> = players
        .iter()
        .filter_map(|(_, player, _, _, _, _, _, _, local, _, _)| {
            local.then_some(player.id.to_bits())
        })
        .collect();
    let candidates = players.iter().filter_map(
        |(
            entity,
            player,
            position,
            velocity,
            presentation,
            predicted,
            interpolated,
            replicate,
            _,
            _,
            _,
        )| {
            let priority = presentation_priority(predicted, interpolated, replicate)?;
            Some(RenderCandidate {
                entity,
                priority,
                local: false,
                player: render_player(player, position, velocity, presentation),
            })
        },
    );
    let mut canonical = canonical_candidates(candidates);
    for (&id, candidate) in &mut canonical {
        candidate.local = local_player_ids.contains(&id);
    }
    let selected_entities: HashMap<Entity, bool> = canonical
        .values()
        .map(|candidate| (candidate.entity, candidate.local))
        .collect();

    for (entity, _, _, _, _, _, _, _, _, presented, presented_local) in &players {
        let selected = selected_entities.get(&entity).copied();
        let local = selected == Some(true);
        let selected = selected.is_some();
        if selected && !presented {
            commands.entity(entity).insert(PresentedPlayer);
        } else if !selected && presented {
            commands.entity(entity).remove::<PresentedPlayer>();
        }
        if local && !presented_local {
            commands.entity(entity).insert(PresentedLocalPlayer);
        } else if !local && presented_local {
            commands.entity(entity).remove::<PresentedLocalPlayer>();
        }
    }

    if let Some(local) = canonical.values().find(|candidate| {
        candidate.local
            && candidate.player.alive
            && renderer_status
                .players
                .get(&candidate.player.id)
                .is_some_and(|status| status.avatar_ready)
    }) {
        orbit.focus = Vec3::from_array(local.player.position) + Vec3::Y * 0.5;
        submitted_camera.0 = RenderCamera {
            focus: orbit.focus.to_array(),
            yaw: orbit.yaw,
            pitch: orbit.pitch,
            radius: orbit.radius,
        };
    }

    let frame = RenderFrame {
        time_seconds: clock.sample(time.elapsed_secs_f64()),
        players: canonical
            .into_values()
            .map(|candidate| candidate.player)
            .collect(),
        camera: submitted_camera.0.clone(),
        sky_blend: sky.blend,
    };
    renderer.submit(frame);
}

/// The sprite standing in for the pointer during a camera drag.
#[derive(Component)]
struct DragCursor;

const DRAG_CURSOR_SIZE: f32 = 64.0;
const DRAG_CURSOR_HOTSPOT: (u16, u16) = (32, 32);
const PIN_DRAG_CURSOR: bool = cfg!(not(target_family = "wasm"));
const KEYBOARD_TURN_SPEED: f32 = 2.5;
const CAMERA_ZOOM_RESPONSE: f32 = 18.0;

fn apply_custom_cursor(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    windows: Query<Entity, With<PrimaryWindow>>,
    mut cursor_image: Local<Option<Handle<Image>>>,
    mut applied: Local<bool>,
) {
    if *applied {
        return;
    }
    let handle = cursor_image
        .get_or_insert_with(|| asset_server.load("ui/cursor.png"))
        .clone();
    if !asset_server.is_loaded_with_dependencies(&handle) {
        return;
    }
    for window in &windows {
        commands
            .entity(window)
            .insert(CursorIcon::Custom(CustomCursor::Image(CustomCursorImage {
                handle: handle.clone(),
                texture_atlas: None,
                flip_x: false,
                flip_y: false,
                rect: None,
                hotspot: DRAG_CURSOR_HOTSPOT,
            })));
    }
    *applied = true;
}

#[allow(clippy::too_many_arguments)]
fn orbit_camera_input(
    mut commands: Commands,
    asset_server: Option<Res<AssetServer>>,
    mut orbit: ResMut<OrbitCamera>,
    time: Res<Time>,
    buttons: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    motion: Res<AccumulatedMouseMotion>,
    scroll: Res<AccumulatedMouseScroll>,
    mut windows: Query<(&mut Window, &mut CursorOptions), With<PrimaryWindow>>,
    drag_cursors: Query<Entity, With<DragCursor>>,
    mut drag_anchor: Local<Option<Vec2>>,
) {
    if PIN_DRAG_CURSOR {
        if let Ok((mut window, mut cursor)) = windows.single_mut() {
            if buttons.just_pressed(MouseButton::Right) {
                *drag_anchor = window.cursor_position();
                cursor.visible = false;
                if let (Some(anchor), Some(asset_server)) = (*drag_anchor, &asset_server) {
                    commands.spawn((
                        Name::new("Drag Cursor"),
                        DragCursor,
                        ImageNode::new(asset_server.load("ui/cursor.png")),
                        Node {
                            position_type: PositionType::Absolute,
                            left: Val::Px(anchor.x - DRAG_CURSOR_SIZE / 2.0),
                            top: Val::Px(anchor.y - DRAG_CURSOR_SIZE / 2.0),
                            width: Val::Px(DRAG_CURSOR_SIZE),
                            height: Val::Px(DRAG_CURSOR_SIZE),
                            ..default()
                        },
                        GlobalZIndex(i32::MAX),
                    ));
                }
            }
            if !buttons.pressed(MouseButton::Right) {
                if let Some(anchor) = drag_anchor.take() {
                    window.set_cursor_position(Some(anchor));
                }
                if !cursor.visible {
                    cursor.visible = true;
                }
                for entity in &drag_cursors {
                    commands.entity(entity).despawn();
                }
            }
        }
    }
    if buttons.pressed(MouseButton::Right) {
        orbit.yaw -= motion.delta.x * 0.005;
        orbit.pitch = (orbit.pitch + motion.delta.y * 0.005).clamp(-1.5, 1.4);
    }
    let keyboard_turn =
        f32::from(keys.pressed(KeyCode::ArrowLeft)) - f32::from(keys.pressed(KeyCode::ArrowRight));
    if keyboard_turn != 0.0 {
        orbit.yaw += keyboard_turn * KEYBOARD_TURN_SPEED * time.delta_secs();
    }
    if scroll.delta.y != 0.0 {
        let fraction = (scroll.delta.y.abs() * 0.1).min(0.6);
        let scale = if scroll.delta.y > 0.0 {
            1.0 / (1.0 + fraction)
        } else {
            1.0 + fraction
        };
        orbit.target_radius = (orbit.target_radius * scale).clamp(3.0, 45.0);
    }
    let zoom_smoothing = 1.0 - (-time.delta_secs() * CAMERA_ZOOM_RESPONSE).exp();
    orbit.radius += (orbit.target_radius - orbit.radius) * zoom_smoothing;
}
