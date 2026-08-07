//! State and simulation shared by the authoritative server and predicted clients.

use crate::physics::{
    solid_query_filter, CharacterCollisionShape, ClimbableVolumes, SeatVolumes, WaterVolumes,
    CHARACTER_GROUND_PROBE_HEIGHT, CHARACTER_HEIGHT,
};
use crate::protocol::*;
use avian3d::prelude::*;
use bevy::prelude::*;
use core::time::Duration;
use lightyear::prelude::PeerId;

pub const FIXED_TIMESTEP_HZ: f64 = 64.0;
#[cfg(not(target_family = "wasm"))]
pub const SERVER_PORT: u16 = 5888;
#[cfg(not(target_family = "wasm"))]
pub const SEND_INTERVAL: Duration = Duration::from_millis(32);
pub const PROTOCOL_ID: u64 = 0;
pub const PRIVATE_KEY: [u8; 32] = [0; 32];
pub const GROUND_Y: f32 = 0.75;
pub const RESPAWN_DELAY_TICKS: u8 = FIXED_TIMESTEP_HZ as u8;

const MOVE_SPEED: f32 = 16.0 * crate::starter_map::MAP_SCALE;
const JUMP_HEIGHT: f32 = 7.0 * crate::starter_map::MAP_SCALE;
pub const WORLD_GRAVITY: f32 = 196.2 * crate::starter_map::MAP_SCALE;
const MAX_FALL_SPEED: f32 = 400.0 * crate::starter_map::MAP_SCALE;
const GROUND_PROBE_DISTANCE: f32 = 0.10;
const MAX_STEP_HEIGHT: f32 = 1.0 * crate::starter_map::MAP_SCALE;
const STEP_ASCENT_DISTANCE: f32 = 1.5 * crate::starter_map::MAP_SCALE;
const STEP_EPSILON: f32 = 0.005;
const STEP_SURFACE_CLEARANCE: f32 = 0.01;
const MAX_WALKABLE_SLOPE: f32 = 50.0_f32.to_radians();
const SWIM_SPEED: f32 = 4.2;
const SWIM_MINIMUM_DEPTH: f32 = 0.435;
const AVATAR_SWIM_ROOT_ABOVE_SURFACE: f32 = 0.12;
const SWIM_SURFACE_TOLERANCE: f32 = 0.03;
const SWIM_RESPONSE: f32 = 8.0;
const CLIMB_SPEED: f32 = 16.0 * crate::starter_map::MAP_SCALE;
const SEAT_REENTRY_COOLDOWN_TICKS: u8 = 48;
const JUMP_RELEASE_DEBOUNCE_TICKS: u8 = 4;

#[cfg(any(feature = "client", feature = "server"))]
const PLAYER_ID_MAGIC: [u8; 4] = *b"SFID";

#[cfg(feature = "client")]
pub fn encode_player_identity(player_id: u64) -> [u8; lightyear::netcode::USER_DATA_BYTES] {
    let mut user_data = [0; lightyear::netcode::USER_DATA_BYTES];
    user_data[..4].copy_from_slice(&PLAYER_ID_MAGIC);
    user_data[4..12].copy_from_slice(&player_id.to_le_bytes());
    user_data
}

#[cfg(feature = "server")]
pub fn decode_player_identity(
    user_data: &[u8; lightyear::netcode::USER_DATA_BYTES],
) -> Option<u64> {
    (user_data[..4] == PLAYER_ID_MAGIC).then(|| {
        u64::from_le_bytes(
            user_data[4..12]
                .try_into()
                .expect("player identity occupies exactly eight bytes"),
        )
    })
}

pub struct SharedPlugin;

impl Plugin for SharedPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((ProtocolPlugin, crate::physics::GamePhysicsPlugin));
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CharacterCollision {
    pub collider: Entity,
    pub point: Vec3,
    pub normal: Vec3,
    pub character_velocity: Vec3,
}

#[derive(Component, Default, Deref, DerefMut)]
pub struct CharacterCollisions(Vec<CharacterCollision>);

/// Updates death and respawn as part of the predicted simulation.
#[allow(clippy::type_complexity)]
pub fn update_player_lifecycle(
    mut commands: Commands,
    mut players: Query<
        (
            Entity,
            &mut Player,
            &mut Position,
            &mut LinearVelocity,
            Has<ColliderDisabled>,
        ),
        With<CharacterCollisions>,
    >,
) {
    for (entity, mut player, mut position, mut velocity, collider_disabled) in &mut players {
        if player.alive && fell_out_of_world(position.y) {
            player.alive = false;
            player.respawn_ticks_remaining = RESPAWN_DELAY_TICKS;
            velocity.0 = Vec3::ZERO;
            if !collider_disabled {
                commands.entity(entity).insert(ColliderDisabled);
            }
            continue;
        }

        if player.alive {
            if collider_disabled {
                commands.entity(entity).remove::<ColliderDisabled>();
            }
            continue;
        }

        velocity.0 = Vec3::ZERO;
        if !collider_disabled {
            commands.entity(entity).insert(ColliderDisabled);
        }
        player.respawn_ticks_remaining = player.respawn_ticks_remaining.saturating_sub(1);
        if player.respawn_ticks_remaining > 0 {
            continue;
        }

        position.0 = playground_spawn(player.id);
        player.alive = true;
        player.grounded = false;
        player.swimming = false;
        player.climbing = false;
        player.seated = None;
        player.seat_cooldown = 0;
        player.jump_held = false;
        player.jump_release_ticks = 0;
        commands.entity(entity).remove::<ColliderDisabled>();
    }
}

pub fn fell_out_of_world(height: f32) -> bool {
    !height.is_finite() || height < crate::starter_map::KILL_PLANE_WORLD_Y
}

pub fn playground_spawn(player: PeerId) -> Vec3 {
    let spawn = crate::starter_map::PLAYGROUND_SPAWNS
        [player.to_bits() as usize % crate::starter_map::PLAYGROUND_SPAWNS.len()];
    Vec3::new(spawn.x, GROUND_Y, spawn.y)
}

/// Applies one fixed tick of movement to the player's single Avian body.
#[allow(clippy::too_many_arguments)]
pub fn apply_player_movement(
    entity: Entity,
    player: &mut Player,
    position: &mut Position,
    velocity: &mut LinearVelocity,
    input: &Inputs,
    collision_shape: &CharacterCollisionShape,
    water_volumes: &WaterVolumes,
    climbable_volumes: &ClimbableVolumes,
    seat_volumes: &SeatVolumes,
    occupied_seats: &[SeatId],
    move_and_slide: &MoveAndSlide,
    mut on_hit: impl FnMut(CharacterCollision),
) {
    if !player.alive {
        velocity.0 = Vec3::ZERO;
        return;
    }

    let Inputs::Move {
        dir,
        jump,
        descend,
        climb,
    } = input;
    let jump_pressed =
        jump_pressed_this_tick(*jump, &mut player.jump_held, &mut player.jump_release_ticks);
    let dir = dir.clamp_length_max(1.0);
    let filter = solid_query_filter().with_excluded_entities([entity]);
    let ground_hit = move_and_slide.spatial_query.cast_shape(
        &collision_shape.ground_probe,
        position.0 - Vec3::Y * (CHARACTER_HEIGHT * 0.5 - CHARACTER_GROUND_PROBE_HEIGHT * 0.5),
        Quat::IDENTITY,
        Dir3::NEG_Y,
        &ShapeCastConfig::from_max_distance(GROUND_PROBE_DISTANCE),
        &filter,
    );
    player.grounded = ground_hit
        .as_ref()
        .is_some_and(|hit| hit.normal1.angle_between(Vec3::Y) <= MAX_WALKABLE_SLOPE);
    if player.grounded && velocity.y <= 0.0 {
        position.y -= ground_hit
            .expect("grounded requires a walkable probe hit")
            .distance;
    }

    tick_seat_cooldown(&mut player.seat_cooldown);
    if exit_seat_on_jump(&mut player.seated, &mut player.seat_cooldown, jump_pressed) {
        player.grounded = true;
    } else if let Some(seat) = player.seated {
        if let Some(target) = seat_volumes.target(seat) {
            position.0 = target;
            velocity.0 = Vec3::ZERO;
            player.grounded = true;
            player.swimming = false;
            player.climbing = false;
            if let Some(yaw) = seat_volumes.facing_yaw(seat) {
                player.facing = Vec3::new(yaw.sin(), 0.0, yaw.cos());
            }
            return;
        }
        player.seated = None;
    }
    if player.grounded && !*jump && player.seat_cooldown == 0 {
        if let Some((seat, target)) = seat_volumes
            .target_for_character(position.0)
            .filter(|(seat, _)| !occupied_seats.contains(seat))
        {
            position.0 = target;
            velocity.0 = Vec3::ZERO;
            player.seated = Some(seat);
            player.swimming = false;
            player.climbing = false;
            if let Some(yaw) = seat_volumes.facing_yaw(seat) {
                player.facing = Vec3::new(yaw.sin(), 0.0, yaw.cos());
            }
            return;
        }
    }

    let water_surface = water_volumes.surface_for_character(position.0);
    player.swimming = water_surface
        .is_some_and(|surface| !player.grounded || surface - position.y >= SWIM_MINIMUM_DEPTH);
    let touches_climbable = climbable_volumes.contains_character(position.0);
    let climb_jump = player.climbing && jump_pressed;
    player.climbing = next_climbing_state(
        player.climbing,
        player.swimming,
        player.grounded,
        touches_climbable,
        Vec2::new(dir.x, dir.z).length(),
        *climb,
        *jump,
    );
    let ground_jump = ground_jump_requested(*jump, player.grounded, player.swimming);

    if player.climbing {
        if let Some(direction) = climbable_volumes.facing_direction_for_character(position.0) {
            player.facing = direction;
        }
    } else if let Some(direction) = dir.try_normalize() {
        player.facing = direction;
    }

    let mut vertical_velocity = velocity.y;
    if player.swimming {
        player.grounded = false;
        let surface = water_surface.expect("swimming requires a water surface");
        let vertical_intent = dir.y + (*jump as i8 - *descend as i8) as f32;
        vertical_velocity = swimming_vertical_velocity(
            vertical_velocity,
            vertical_intent,
            jump_pressed,
            position.y,
            surface,
        );
    } else if climb_jump || ground_jump {
        vertical_velocity = (2.0 * WORLD_GRAVITY * JUMP_HEIGHT).sqrt();
        player.grounded = false;
    } else if player.climbing {
        player.grounded = false;
        vertical_velocity = climb.clamp(-1.0, 1.0) * CLIMB_SPEED;
    } else if player.grounded && vertical_velocity <= 0.0 {
        vertical_velocity = 0.0;
    } else {
        vertical_velocity =
            (vertical_velocity - WORLD_GRAVITY / FIXED_TIMESTEP_HZ as f32).max(-MAX_FALL_SPEED);
    }

    let horizontal_speed = if player.swimming {
        SWIM_SPEED
    } else {
        MOVE_SPEED
    };
    let horizontal_dir = if player.climbing { Vec3::ZERO } else { dir };
    let desired_velocity = Vec3::new(
        horizontal_dir.x * horizontal_speed,
        vertical_velocity,
        horizontal_dir.z * horizontal_speed,
    );
    let step_velocity = Vec3::new(desired_velocity.x, 0.0, desired_velocity.z);
    let can_step = !player.swimming
        && !player.climbing
        && !*jump
        && vertical_velocity <= 0.0
        && step_velocity.length_squared() > f32::EPSILON;
    let start_position = position.0;
    let mut hit_walkable_ground = false;
    let mut hit_ceiling = false;
    let output = move_and_slide.move_and_slide(
        &collision_shape.body,
        position.0,
        Quat::IDENTITY,
        desired_velocity,
        Duration::from_secs_f64(1.0 / FIXED_TIMESTEP_HZ),
        &MoveAndSlideConfig {
            move_and_slide_iterations: 6,
            ..default()
        },
        &filter,
        |hit| {
            let normal = **hit.normal;
            on_hit(CharacterCollision {
                collider: hit.entity,
                point: hit.point,
                normal,
                character_velocity: *hit.velocity,
            });
            if normal.angle_between(Vec3::Y) <= MAX_WALKABLE_SLOPE {
                hit_walkable_ground = true;
            } else if normal.angle_between(Vec3::NEG_Y) <= MAX_WALKABLE_SLOPE {
                hit_ceiling = true;
            }
            MoveAndSlideHitResponse::Accept
        },
    );
    if can_step {
        if let Some(step_position) = try_smooth_step_up(
            move_and_slide,
            &collision_shape.body,
            start_position,
            step_velocity,
            &filter,
        ) {
            position.0 = step_position;
            velocity.0 = step_velocity;
            player.grounded = true;
            return;
        }
    }
    position.0 = output.position;
    velocity.0 = Vec3::new(
        output.projected_velocity.x,
        vertical_velocity,
        output.projected_velocity.z,
    );

    if hit_ceiling && velocity.y > 0.0 {
        velocity.y = 0.0;
    }
    if hit_walkable_ground && velocity.y <= 0.0 {
        player.grounded = true;
        velocity.y = 0.0;
    } else if velocity.y <= 0.0 {
        if let Some(hit) = move_and_slide.spatial_query.cast_shape(
            &collision_shape.ground_probe,
            position.0 - Vec3::Y * (CHARACTER_HEIGHT * 0.5 - CHARACTER_GROUND_PROBE_HEIGHT * 0.5),
            Quat::IDENTITY,
            Dir3::NEG_Y,
            &ShapeCastConfig::from_max_distance(GROUND_PROBE_DISTANCE),
            &filter,
        ) {
            if hit.normal1.angle_between(Vec3::Y) <= MAX_WALKABLE_SLOPE {
                position.y -= hit.distance;
                player.grounded = true;
                velocity.y = 0.0;
            }
        }
    }
}

fn jump_pressed_this_tick(jump: bool, was_held: &mut bool, release_ticks: &mut u8) -> bool {
    if jump {
        *release_ticks = 0;
        if *was_held {
            return false;
        }
        *was_held = true;
        return true;
    }

    if *was_held {
        *release_ticks = release_ticks.saturating_add(1);
        if *release_ticks >= JUMP_RELEASE_DEBOUNCE_TICKS {
            *was_held = false;
            *release_ticks = 0;
        }
    } else {
        *release_ticks = 0;
    }
    false
}

fn ground_jump_requested(jump: bool, grounded: bool, swimming: bool) -> bool {
    jump && grounded && !swimming
}

fn tick_seat_cooldown(cooldown: &mut u8) {
    *cooldown = cooldown.saturating_sub(1);
}

fn exit_seat_on_jump(seated: &mut Option<SeatId>, cooldown: &mut u8, jump_pressed: bool) -> bool {
    if seated.is_none() || !jump_pressed {
        return false;
    }
    *seated = None;
    *cooldown = SEAT_REENTRY_COOLDOWN_TICKS;
    true
}

fn try_smooth_step_up(
    move_and_slide: &MoveAndSlide,
    collision_shape: &Collider,
    start_position: Vec3,
    planar_velocity: Vec3,
    filter: &SpatialQueryFilter,
) -> Option<Vec3> {
    let direction = planar_velocity.try_normalize()?;
    let tick_distance = planar_velocity.length() / FIXED_TIMESTEP_HZ as f32;
    let support = move_and_slide.spatial_query.cast_shape(
        collision_shape,
        start_position,
        Quat::IDENTITY,
        Dir3::NEG_Y,
        &ShapeCastConfig::from_max_distance(MAX_STEP_HEIGHT + GROUND_PROBE_DISTANCE),
        filter,
    )?;
    if support.normal1.angle_between(Vec3::Y) > MAX_WALKABLE_SLOPE {
        return continue_across_elevated_step(
            move_and_slide,
            collision_shape,
            start_position,
            support.entity,
            direction,
            planar_velocity,
            filter,
        );
    }
    let support_position = start_position - Vec3::Y * support.distance;
    let mut obstacle_filter = filter.clone();
    obstacle_filter.excluded_entities.insert(support.entity);

    let obstacle = move_and_slide.spatial_query.cast_shape(
        collision_shape,
        support_position,
        Quat::IDENTITY,
        Dir3::new(direction).ok()?,
        &ShapeCastConfig::from_max_distance(STEP_ASCENT_DISTANCE + tick_distance),
        &obstacle_filter,
    );
    let Some(obstacle) = obstacle else {
        return continue_across_elevated_step(
            move_and_slide,
            collision_shape,
            start_position,
            support.entity,
            direction,
            planar_velocity,
            filter,
        );
    };
    if obstacle.normal1.angle_between(Vec3::Y) <= MAX_WALKABLE_SLOPE {
        return continue_across_elevated_step(
            move_and_slide,
            collision_shape,
            start_position,
            support.entity,
            direction,
            planar_velocity,
            filter,
        );
    }

    let landing_probe = support_position
        + Vec3::Y * MAX_STEP_HEIGHT
        + direction
            * (obstacle.distance
                + tick_distance
                + crate::physics::CHARACTER_RADIUS * 2.0
                + STEP_EPSILON);
    let landing = move_and_slide.spatial_query.cast_shape(
        collision_shape,
        landing_probe,
        Quat::IDENTITY,
        Dir3::NEG_Y,
        &ShapeCastConfig::from_max_distance(MAX_STEP_HEIGHT + GROUND_PROBE_DISTANCE),
        filter,
    )?;
    if landing.normal1.angle_between(Vec3::Y) > MAX_WALKABLE_SLOPE {
        return None;
    }
    let landing_position = landing_probe - Vec3::Y * landing.distance;
    let rise = landing_position.y + STEP_SURFACE_CLEARANCE - support_position.y;
    if rise <= STEP_EPSILON || rise > MAX_STEP_HEIGHT + STEP_EPSILON {
        return None;
    }

    let remaining_after_tick = (obstacle.distance - tick_distance).max(0.0);
    let ascent = 1.0 - (remaining_after_tick / STEP_ASCENT_DISTANCE).clamp(0.0, 1.0);
    let target_y = support_position.y + rise * ascent;
    let lift = target_y - start_position.y;
    if lift > STEP_EPSILON
        && move_and_slide
            .spatial_query
            .cast_shape(
                collision_shape,
                start_position,
                Quat::IDENTITY,
                Dir3::Y,
                &ShapeCastConfig::from_max_distance(lift),
                filter,
            )
            .is_some_and(|hit| hit.distance < lift - STEP_EPSILON)
    {
        return None;
    }

    let ramp_start = Vec3::new(start_position.x, target_y, start_position.z);
    let output = move_and_slide.move_and_slide(
        collision_shape,
        ramp_start,
        Quat::IDENTITY,
        planar_velocity,
        Duration::from_secs_f64(1.0 / FIXED_TIMESTEP_HZ),
        &MoveAndSlideConfig {
            move_and_slide_iterations: 6,
            ..default()
        },
        filter,
        |_| MoveAndSlideHitResponse::Accept,
    );
    Some(output.position)
}

fn continue_across_elevated_step(
    move_and_slide: &MoveAndSlide,
    collision_shape: &Collider,
    start_position: Vec3,
    support_entity: Entity,
    direction: Vec3,
    planar_velocity: Vec3,
    filter: &SpatialQueryFilter,
) -> Option<Vec3> {
    let mut lower_filter = filter.clone();
    lower_filter.excluded_entities.insert(support_entity);
    let lower_support = move_and_slide.spatial_query.cast_shape(
        collision_shape,
        start_position,
        Quat::IDENTITY,
        Dir3::NEG_Y,
        &ShapeCastConfig::from_max_distance(MAX_STEP_HEIGHT + GROUND_PROBE_DISTANCE),
        &lower_filter,
    )?;
    if lower_support.normal1.angle_between(Vec3::Y) > MAX_WALKABLE_SLOPE {
        return None;
    }
    let lower_y = start_position.y - lower_support.distance;

    let top_probe = start_position
        + Vec3::Y * MAX_STEP_HEIGHT
        + direction * (crate::physics::CHARACTER_RADIUS * 2.0 + STEP_EPSILON);
    let top = move_and_slide.spatial_query.cast_shape(
        collision_shape,
        top_probe,
        Quat::IDENTITY,
        Dir3::NEG_Y,
        &ShapeCastConfig::from_max_distance(MAX_STEP_HEIGHT + GROUND_PROBE_DISTANCE),
        filter,
    )?;
    if top.entity != support_entity || top.normal1.angle_between(Vec3::Y) > MAX_WALKABLE_SLOPE {
        return None;
    }
    let top_y = top_probe.y - top.distance;
    let rise = top_y - lower_y;
    if rise <= STEP_EPSILON || rise > MAX_STEP_HEIGHT + STEP_EPSILON {
        return None;
    }

    let step_start = Vec3::new(start_position.x, top_y, start_position.z);
    let output = move_and_slide.move_and_slide(
        collision_shape,
        step_start,
        Quat::IDENTITY,
        planar_velocity,
        Duration::from_secs_f64(1.0 / FIXED_TIMESTEP_HZ),
        &MoveAndSlideConfig {
            move_and_slide_iterations: 6,
            ..default()
        },
        filter,
        |_| MoveAndSlideHitResponse::Accept,
    );
    Some(output.position)
}

fn next_climbing_state(
    was_climbing: bool,
    swimming: bool,
    grounded: bool,
    touches_climbable: bool,
    planar_input: f32,
    climb_input: f32,
    jump: bool,
) -> bool {
    let wants_to_acquire = planar_input > 0.01;
    let descending_at_bottom = grounded && climb_input < -0.01;
    !swimming
        && !jump
        && !descending_at_bottom
        && touches_climbable
        && (was_climbing || wants_to_acquire)
}

fn swimming_vertical_velocity(
    current_velocity: f32,
    vertical_intent: f32,
    exit_jump: bool,
    root_y: f32,
    surface_y: f32,
) -> f32 {
    let target_root_y = surface_y + AVATAR_SWIM_ROOT_ABOVE_SURFACE;
    let at_surface = root_y >= target_root_y - SWIM_SURFACE_TOLERANCE;
    if at_surface && (exit_jump || current_velocity > SWIM_SPEED) {
        if current_velocity > SWIM_SPEED {
            return current_velocity - WORLD_GRAVITY / FIXED_TIMESTEP_HZ as f32;
        }
        return (2.0 * WORLD_GRAVITY * JUMP_HEIGHT).sqrt();
    }
    let target_velocity = if vertical_intent.abs() > 0.01 {
        let requested = vertical_intent.clamp(-1.0, 1.0) * SWIM_SPEED;
        if requested > 0.0 && root_y >= target_root_y {
            0.0
        } else {
            requested
        }
    } else {
        let surface_delta = target_root_y - root_y;
        if surface_delta.abs() <= SWIM_SURFACE_TOLERANCE {
            0.0
        } else {
            (surface_delta * 2.0).clamp(-SWIM_SPEED * 0.35, SWIM_SPEED * 0.35)
        }
    };
    let velocity = current_velocity
        + (target_velocity - current_velocity)
            * (SWIM_RESPONSE / FIXED_TIMESTEP_HZ as f32).min(1.0);
    if velocity > 0.0 {
        velocity.min((target_root_y - root_y).max(0.0) * FIXED_TIMESTEP_HZ as f32)
    } else {
        velocity
    }
}
