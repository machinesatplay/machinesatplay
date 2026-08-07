//! Character movement sounds projected from predicted/interpolated player
//! state.

use crate::render::{PlayerPresentation, PresentedLocalPlayer, PresentedPlayer};
use avian3d::prelude::{LinearVelocity, Position};
use bevy::audio::{
    AudioPlayer, AudioSink, PlaybackSettings, SpatialListener, SpatialScale, Volume,
};
use bevy::prelude::*;
use game_core::physics::{WaterVolumes, CHARACTER_RADIUS};
use game_core::protocol::*;
use game_core::shared::GROUND_Y;
use game_core::starter_map::STARTER_PARTS;
use lightyear::frame_interpolation::FrameInterpolationSystems;
use render_fn::{RendererAnimationStatus, RendererSystems};

const RUN_AFTER_SECONDS: f32 = 0.7;
const HARDFALL_SPEED: f32 = 58.0 * game_core::starter_map::MAP_SCALE;
const UNDERWATER_DEPTH: f32 = 2.1 * game_core::starter_map::MAP_SCALE;
const FOOTFALL_INTERVAL: f32 = (0.666_672 / 2.0) / (16.0 / 14.5);
const FOOTFALL_DEBOUNCE_SECONDS: f32 = 0.12;
const MOVING_SPEED: f32 = 0.08;
const MOVEMENT_GRACE_SECONDS: f32 = 0.05;
const BACKGROUND_MUSIC_VOLUME: f32 = 0.083;
// Rodio applies inverse-square attenuation after one scaled unit. Keeping the
// default 1:1 scale makes a cue near the followed player almost silent because
// the third-person camera itself sits about nine world units away.
const CHARACTER_SPATIAL_SCALE: f32 = 0.1;
const FIRST_FOOT_CONTACT_PHASE: f32 = 0.35;

#[derive(Resource)]
struct BackgroundMusic {
    entity: Option<Entity>,
}

#[derive(Component)]
struct MusicToggleButton;

#[derive(Component)]
struct MusicToggleLabel;

pub struct CharacterAudioPlugin;

impl Plugin for CharacterAudioPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup_background_music);
        app.add_systems(PreUpdate, despawn_pending_audio);
        app.add_systems(
            Update,
            (
                music_toggle_interaction,
                attach_listener,
                attach_audio_state,
                cleanup_detached_audio_state,
                cleanup_orphaned_swim_loops,
            )
                .chain(),
        );
        app.add_systems(
            PostUpdate,
            update_character_audio
                .after(FrameInterpolationSystems::Interpolate)
                .after(RendererSystems::Render),
        );
    }
}

fn spawn_background_music(commands: &mut Commands, asset_server: &AssetServer) -> Entity {
    commands
        .spawn((
            Name::new("Background music: town theme"),
            AudioPlayer::new(asset_server.load("music/town-theme.mp3")),
            PlaybackSettings::LOOP.with_volume(Volume::Linear(BACKGROUND_MUSIC_VOLUME)),
        ))
        .id()
}

fn setup_background_music(mut commands: Commands, asset_server: Res<AssetServer>) {
    let entity = spawn_background_music(&mut commands, &asset_server);
    commands.insert_resource(BackgroundMusic {
        entity: Some(entity),
    });
    commands.spawn((
        Name::new("Music Toggle Button"),
        Button,
        MusicToggleButton,
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(84.0),
            right: Val::Px(12.0),
            padding: UiRect::axes(Val::Px(10.0), Val::Px(6.0)),
            border_radius: BorderRadius::all(Val::Px(8.0)),
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55)),
        children![(
            MusicToggleLabel,
            Text::new("Music: On"),
            TextFont {
                font_size: FontSize::Px(14.0),
                ..default()
            },
            TextColor(Color::WHITE),
        )],
    ));
}

fn music_toggle_interaction(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut music: ResMut<BackgroundMusic>,
    mut buttons: Query<
        (&Interaction, &mut BackgroundColor),
        (Changed<Interaction>, With<MusicToggleButton>),
    >,
    mut labels: Query<&mut Text, With<MusicToggleLabel>>,
) {
    for (interaction, mut background) in &mut buttons {
        match interaction {
            Interaction::Pressed => {
                if let Some(entity) = music.entity.take() {
                    commands.entity(entity).despawn();
                } else {
                    music.entity = Some(spawn_background_music(&mut commands, &asset_server));
                }
                if let Ok(mut label) = labels.single_mut() {
                    label.0 = if music.entity.is_some() {
                        "Music: On".to_string()
                    } else {
                        "Music: Off".to_string()
                    };
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GroundMaterial {
    Plastic,
    Grass,
    Sand,
    Stone,
    Wood,
    Metal,
}

impl GroundMaterial {
    const fn directory(self) -> &'static str {
        match self {
            Self::Plastic => "plastic",
            Self::Grass => "grass",
            Self::Sand => "sand",
            Self::Stone => "stone",
            Self::Wood => "wood",
            Self::Metal => "metal",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SwimLoop {
    Surface,
    Underwater,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FootfallMode {
    Ground,
    Shallow,
}

#[derive(Component)]
struct CharacterAudioState {
    was_grounded: bool,
    was_swimming: bool,
    was_underwater: bool,
    ground_material: GroundMaterial,
    max_downward_speed: f32,
    movement_grace_seconds: f32,
    footfall_mode: Option<FootfallMode>,
    footfall_elapsed: f32,
    footfall_ordinal: Option<usize>,
    footfall_cooldown: f32,
    render_footfall_phase: Option<f32>,
    cue_ordinal: usize,
    swim_loop: Option<(SwimLoop, Entity)>,
}

#[derive(Component)]
struct PendingAudioDespawn;

fn despawn_pending_audio(
    mut commands: Commands,
    pending: Query<(Entity, Option<&AudioSink>), With<PendingAudioDespawn>>,
) {
    for (entity, sink) in &pending {
        if let Some(sink) = sink {
            sink.stop();
        }
        commands.entity(entity).try_despawn();
    }
}

fn defer_audio_despawn(commands: &mut Commands, entity: Entity) {
    commands.entity(entity).try_insert(PendingAudioDespawn);
}

#[derive(Component)]
struct SwimLoopOwner(Entity);

fn attach_listener(
    mut commands: Commands,
    cameras: Query<Entity, (With<Camera3d>, Without<SpatialListener>)>,
) {
    for entity in &cameras {
        commands.entity(entity).insert(SpatialListener::new(0.18));
    }
}

fn attach_audio_state(
    mut commands: Commands,
    players: Query<
        (Entity, &Player, &Position, Option<&PlayerPresentation>),
        (
            With<PresentedPlayer>,
            Or<(Without<CharacterAudioState>, Added<PresentedPlayer>)>,
        ),
    >,
) {
    for (entity, player, physics_position, _) in &players {
        let position = physics_position.0;
        commands.entity(entity).insert(CharacterAudioState {
            was_grounded: player.grounded && !player.swimming,
            was_swimming: player.swimming,
            was_underwater: false,
            ground_material: ground_material_at(position).unwrap_or(GroundMaterial::Plastic),
            max_downward_speed: 0.0,
            movement_grace_seconds: 0.0,
            footfall_mode: None,
            footfall_elapsed: 0.0,
            footfall_ordinal: None,
            footfall_cooldown: 0.0,
            render_footfall_phase: None,
            cue_ordinal: 0,
            swim_loop: None,
        });
    }
}

fn cleanup_detached_audio_state(
    mut commands: Commands,
    mut removed: RemovedComponents<PresentedPlayer>,
    mut states: Query<&mut CharacterAudioState>,
) {
    for player in removed.read() {
        let Ok(mut state) = states.get_mut(player) else {
            continue;
        };
        if let Some((_, loop_entity)) = state.swim_loop.take() {
            defer_audio_despawn(&mut commands, loop_entity);
        }
        commands.entity(player).remove::<CharacterAudioState>();
    }
}

fn cleanup_orphaned_swim_loops(
    mut commands: Commands,
    players: Query<(), With<PresentedPlayer>>,
    loops: Query<(Entity, &SwimLoopOwner)>,
) {
    for (loop_entity, owner) in &loops {
        if players.get(owner.0).is_err() {
            defer_audio_despawn(&mut commands, loop_entity);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn update_character_audio(
    mut commands: Commands,
    time: Res<Time>,
    asset_server: Res<AssetServer>,
    water_volumes: Res<WaterVolumes>,
    animation_status: Option<Res<RendererAnimationStatus>>,
    mut loop_transforms: Query<&mut Transform, Without<PresentedPlayer>>,
    mut players: Query<
        (
            Entity,
            &Player,
            &Position,
            &LinearVelocity,
            Option<&PlayerPresentation>,
            Has<PresentedLocalPlayer>,
            &mut CharacterAudioState,
        ),
        With<PresentedPlayer>,
    >,
) {
    let dt = time.delta_secs().max(1e-6);
    for (entity, player, physics_position, velocity, _, owned, mut state) in &mut players {
        let position = physics_position.0;
        let horizontal_speed = Vec2::new(velocity.x, velocity.z).length();
        let travel_speed = velocity.length();
        state.footfall_cooldown = (state.footfall_cooldown - dt).max(0.0);
        let physically_grounded =
            player.grounded && !player.swimming && !player.climbing && player.seated.is_none();
        let surface_supported = player.grounded && !player.swimming && !player.climbing;
        let water_surface = water_volumes.surface_for_character(position);
        let shallow = physically_grounded && water_surface.is_some();
        let underwater = player.swimming
            && water_surface.is_some_and(|surface| surface - position.y > UNDERWATER_DEPTH);
        let measured_moving = if player.swimming {
            travel_speed > MOVING_SPEED
        } else {
            horizontal_speed > MOVING_SPEED
        };
        if measured_moving {
            state.movement_grace_seconds = MOVEMENT_GRACE_SECONDS;
        } else {
            state.movement_grace_seconds = (state.movement_grace_seconds - dt).max(0.0);
        }
        let moving = measured_moving || state.movement_grace_seconds > 0.0;
        let rendered_walk = animation_status
            .as_ref()
            .and_then(|status| status.players.get(&player.id.to_bits()));
        let uses_renderer_timing = animation_status.is_some();
        let walking = if uses_renderer_timing {
            rendered_walk.is_some_and(|status| status.walk_amount >= 0.05)
        } else {
            moving
        };

        let material = ground_material_at(position).unwrap_or(state.ground_material);
        if physically_grounded {
            state.ground_material = material;
        }

        if !state.was_swimming && player.swimming {
            play_numbered(
                &mut commands,
                &asset_server,
                "water/transitions/enter",
                4,
                0.42,
                next_ordinal(&mut state),
                owned,
                position,
            );
        } else if state.was_swimming && !player.swimming {
            play_numbered(
                &mut commands,
                &asset_server,
                "water/transitions/exit",
                4,
                0.38,
                next_ordinal(&mut state),
                owned,
                position,
            );
        }
        if state.was_swimming && player.swimming && state.was_underwater != underwater {
            let (cue, volume) = if underwater {
                ("water/transitions/submerge", 0.36)
            } else {
                ("water/transitions/surface", 0.36)
            };
            play_numbered(
                &mut commands,
                &asset_server,
                cue,
                4,
                volume,
                next_ordinal(&mut state),
                owned,
                position,
            );
        }

        if state.was_grounded && !surface_supported && !player.swimming && velocity.y > 0.0 {
            let cue = format!("ground/{}/jump", state.ground_material.directory());
            play_numbered(
                &mut commands,
                &asset_server,
                &cue,
                4,
                0.32,
                next_ordinal(&mut state),
                owned,
                position,
            );
        }

        if !physically_grounded && !player.swimming && !player.climbing && player.seated.is_none() {
            state.max_downward_speed = state.max_downward_speed.max(-velocity.y);
        }
        if !state.was_grounded && surface_supported {
            let hard = state.max_downward_speed >= HARDFALL_SPEED;
            let (kind, count, volume) = if hard {
                ("hard-land", 3, 0.46)
            } else {
                ("land", 4, 0.34)
            };
            let cue = format!("ground/{}/{kind}", material.directory());
            play_numbered(
                &mut commands,
                &asset_server,
                &cue,
                count,
                volume,
                next_ordinal(&mut state),
                owned,
                position,
            );
            state.max_downward_speed = 0.0;
        }

        let footfall_mode = (physically_grounded && walking).then_some(if shallow {
            FootfallMode::Shallow
        } else {
            FootfallMode::Ground
        });
        if state.footfall_mode != footfall_mode {
            state.footfall_mode = footfall_mode;
            state.footfall_elapsed = 0.0;
            state.footfall_ordinal = None;
            state.render_footfall_phase = None;
        } else if footfall_mode.is_some() {
            state.footfall_elapsed += dt;
        }
        if let Some(mode) = footfall_mode {
            let ordinal = if let Some(rendered_walk) = rendered_walk {
                crossed_foot_contact(&mut state.render_footfall_phase, rendered_walk.walk_phase)
                    .then(|| {
                        state
                            .footfall_ordinal
                            .map_or(0, |ordinal| ordinal.wrapping_add(1))
                    })
            } else if !uses_renderer_timing {
                let ordinal =
                    ((state.footfall_elapsed + 0.001) / FOOTFALL_INTERVAL).floor() as usize;
                state
                    .footfall_ordinal
                    .is_none_or(|previous| ordinal > previous)
                    .then_some(ordinal)
            } else {
                None
            };
            if let Some(ordinal) = ordinal {
                state.footfall_ordinal = Some(ordinal);
                if state.footfall_cooldown <= 0.0 {
                    state.footfall_cooldown = FOOTFALL_DEBOUNCE_SECONDS;
                    let running = state.footfall_elapsed >= RUN_AFTER_SECONDS;
                    let (cue, count, volume) = match (mode, running) {
                        (FootfallMode::Shallow, true) => ("water/shallow/run".to_string(), 8, 0.38),
                        (FootfallMode::Shallow, false) => {
                            ("water/shallow/walk".to_string(), 8, 0.32)
                        }
                        (FootfallMode::Ground, true) => {
                            (format!("ground/{}/run", material.directory()), 6, 0.34)
                        }
                        (FootfallMode::Ground, false) => {
                            (format!("ground/{}/walk", material.directory()), 6, 0.28)
                        }
                    };
                    play_numbered(
                        &mut commands,
                        &asset_server,
                        &cue,
                        count,
                        volume,
                        ordinal,
                        owned,
                        position,
                    );
                }
            }
        } else {
            state.footfall_elapsed = 0.0;
            state.footfall_ordinal = None;
            state.render_footfall_phase = None;
        }

        let desired_loop = (player.swimming && moving).then_some(if underwater {
            SwimLoop::Underwater
        } else {
            SwimLoop::Surface
        });
        if state.swim_loop.map(|(kind, _)| kind) != desired_loop {
            if let Some((_, entity)) = state.swim_loop.take() {
                defer_audio_despawn(&mut commands, entity);
            }
            if let Some(kind) = desired_loop {
                let (asset, volume) = match kind {
                    SwimLoop::Surface => ("water/surface-swim-loop.mp3", 0.30),
                    SwimLoop::Underwater => ("water/underwater-swim-loop.mp3", 0.24),
                };
                let loop_entity = spawn_audio(
                    &mut commands,
                    &asset_server,
                    asset,
                    volume,
                    true,
                    owned,
                    position,
                );
                commands.entity(loop_entity).insert(SwimLoopOwner(entity));
                state.swim_loop = Some((kind, loop_entity));
            }
        }
        if let Some((_, loop_entity)) = state.swim_loop {
            if let Ok(mut transform) = loop_transforms.get_mut(loop_entity) {
                transform.translation = position;
            }
        }

        state.was_grounded = surface_supported;
        state.was_swimming = player.swimming;
        state.was_underwater = underwater;
    }
}

fn next_ordinal(state: &mut CharacterAudioState) -> usize {
    let value = state.cue_ordinal;
    state.cue_ordinal = state.cue_ordinal.wrapping_add(1);
    value
}

fn crossed_foot_contact(previous: &mut Option<f32>, phase: f32) -> bool {
    let phase = phase.rem_euclid(core::f32::consts::TAU);
    let crossed = match *previous {
        None => phase <= FIRST_FOOT_CONTACT_PHASE,
        Some(previous) => {
            phase < previous
                || (phase / core::f32::consts::PI).floor()
                    > (previous / core::f32::consts::PI).floor()
        }
    };
    *previous = Some(phase);
    crossed
}

#[allow(clippy::too_many_arguments)]
fn play_numbered(
    commands: &mut Commands,
    asset_server: &AssetServer,
    base: &str,
    count: usize,
    volume: f32,
    ordinal: usize,
    owned: bool,
    position: Vec3,
) {
    let variant = ordinal % count + 1;
    let asset = format!("{base}-{variant:02}.mp3");
    spawn_audio(
        commands,
        asset_server,
        &asset,
        volume,
        false,
        owned,
        position,
    );
}

fn spawn_audio(
    commands: &mut Commands,
    asset_server: &AssetServer,
    asset: &str,
    volume: f32,
    looped: bool,
    owned: bool,
    position: Vec3,
) -> Entity {
    let path = format!("character_sfx/{asset}");
    let settings = if looped {
        PlaybackSettings::LOOP
    } else {
        PlaybackSettings::DESPAWN
    }
    .with_volume(Volume::Linear(volume))
    .with_spatial(!owned);
    let settings = if owned {
        settings
    } else {
        settings.with_spatial_scale(SpatialScale::new(CHARACTER_SPATIAL_SCALE))
    };
    commands
        .spawn((
            Name::new(format!("Character sound: {asset}")),
            AudioPlayer::new(asset_server.load(path)),
            settings,
            Transform::from_translation(position),
        ))
        .id()
}

fn ground_material_at(position: Vec3) -> Option<GroundMaterial> {
    let foot_y = position.y - GROUND_Y;
    STARTER_PARTS
        .iter()
        .filter(|part| part.collidable)
        .filter_map(|part| {
            let center = part.position();
            let half = part.size() * 0.5;
            let top = center.y + half.y;
            // Grounded is replicated as a discrete value while remote position
            // is interpolated. On the landing frame that position can still be
            // above the actual support, so resolve the highest surface below
            // the character instead of requiring an exact foot-height match.
            let supports = (position.x - center.x).abs() <= half.x + CHARACTER_RADIUS
                && (position.z - center.z).abs() <= half.z + CHARACTER_RADIUS
                && top <= foot_y + 0.22;
            supports.then_some((top, material_for_part(part.name, part.material_id)))
        })
        .max_by(|left, right| left.0.total_cmp(&right.0))
        .map(|(_, material)| material)
}

fn material_for_part(name: &str, material_id: u8) -> GroundMaterial {
    match material_id {
        14 => return GroundMaterial::Grass,
        12 => return GroundMaterial::Metal,
        6 => return GroundMaterial::Stone,
        3 | 4 => return GroundMaterial::Wood,
        _ => {}
    }
    let lower = name.to_ascii_lowercase();
    if lower.contains("grass") || lower == "baseplate" {
        GroundMaterial::Grass
    } else if lower.contains("sand") {
        GroundMaterial::Sand
    } else if lower.contains("wood") || lower.contains("bench") {
        GroundMaterial::Wood
    } else if lower.contains("metal") {
        GroundMaterial::Metal
    } else if lower.contains("stone") {
        GroundMaterial::Stone
    } else {
        GroundMaterial::Plastic
    }
}
