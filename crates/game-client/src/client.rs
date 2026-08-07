//! Client-side input collection for the authoritative game server.

use avian3d::prelude::RigidBody;
use bevy::prelude::*;
use game_core::physics::PlayerPhysicsBundle;
use game_core::protocol::*;
use lightyear::prelude::client::input::*;
use lightyear::prelude::input::native::*;
use lightyear::prelude::*;

pub struct GameClientPlugin;

impl Plugin for GameClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PreUpdate,
            (
                attach_predicted_player_physics,
                attach_input_to_controlled_players,
            )
                .chain(),
        );
        app.add_systems(
            FixedPreUpdate,
            buffer_input.in_set(InputSystems::WriteClientInputs),
        );
        app.add_systems(
            PostUpdate,
            discard_input_buffers_ahead_of_timeline.before(InputSystems::PrepareInputMessage),
        );
        app.add_observer(handle_controlled_spawn);
    }
}

#[allow(clippy::type_complexity)]
fn attach_predicted_player_physics(
    mut commands: Commands,
    players: Query<Entity, (With<Predicted>, With<Player>, Without<RigidBody>)>,
) {
    for entity in &players {
        commands
            .entity(entity)
            .insert(PlayerPhysicsBundle::default());
    }
}

fn attach_input_to_controlled_players(
    mut commands: Commands,
    players: Query<Entity, (With<Player>, With<Controlled>, Without<InputMarker<Inputs>>)>,
) {
    for entity in &players {
        commands
            .entity(entity)
            .insert(InputMarker::<Inputs>::default());
    }
}

/// A tick snap can move a newly connected client backward after its first
/// inputs were buffered. Lightyear cannot encode a buffer that starts after
/// the message tick, so discard that stale fragment before message creation.
fn discard_input_buffers_ahead_of_timeline(
    timeline: Res<LocalTimeline>,
    input_timeline: Single<
        &InputTimeline,
        (
            With<Client>,
            With<IsSynced<InputTimeline>>,
            Without<Rollback>,
        ),
    >,
    mut input_buffers: Query<&mut NativeBuffer<Inputs>, With<InputMarker<Inputs>>>,
) {
    let message_tick = timeline.tick() + input_timeline.input_delay() as i32;
    for mut input_buffer in &mut input_buffers {
        discard_input_buffer_if_ahead(&mut input_buffer, message_tick);
    }
}

fn discard_input_buffer_if_ahead(
    input_buffer: &mut NativeBuffer<Inputs>,
    message_tick: Tick,
) -> bool {
    if input_buffer
        .start_tick
        .is_some_and(|start_tick| start_tick > message_tick)
    {
        *input_buffer = NativeBuffer::default();
        return true;
    }
    false
}

/// Reads the keyboard and writes the input for the current tick.
/// The direction is rotated by the camera yaw so W always means
/// "away from the camera".
fn buffer_input(
    mut query: Query<(&mut ActionState<Inputs>, &Player), With<InputMarker<Inputs>>>,
    keypress: Option<Res<ButtonInput<KeyCode>>>,
    camera: Option<Res<crate::render::OrbitCamera>>,
) {
    let Ok((mut action_state, swimming)) = query.single_mut() else {
        return;
    };
    let mut forward = 0.0f32;
    let mut strafe = 0.0f32;
    if let Some(keypress) = &keypress {
        if keypress.pressed(KeyCode::KeyW) || keypress.pressed(KeyCode::ArrowUp) {
            forward += 1.0;
        }
        if keypress.pressed(KeyCode::KeyS) || keypress.pressed(KeyCode::ArrowDown) {
            forward -= 1.0;
        }
        // ArrowLeft/ArrowRight are camera-turn keys (see render.rs), so only
        // A/D strafe.
        if keypress.pressed(KeyCode::KeyA) {
            strafe -= 1.0;
        }
        if keypress.pressed(KeyCode::KeyD) {
            strafe += 1.0;
        }
    }
    let jump = keypress.as_ref().is_some_and(|k| k.pressed(KeyCode::Space));
    let descend = keypress
        .as_ref()
        .is_some_and(|k| k.pressed(KeyCode::ShiftLeft) || k.pressed(KeyCode::ShiftRight));
    let yaw = camera.as_ref().map(|c| c.yaw).unwrap_or(0.0);
    // Ground controls stay planar, then switch to the camera's
    // full 3D basis in water so looking down and pressing W dives naturally.
    let forward_dir = if swimming.swimming {
        Vec3::new(
            -yaw.sin() * camera.as_ref().map(|c| c.pitch.cos()).unwrap_or(1.0),
            -camera.as_ref().map(|c| c.pitch.sin()).unwrap_or(0.0),
            -yaw.cos() * camera.as_ref().map(|c| c.pitch.cos()).unwrap_or(1.0),
        )
    } else {
        Vec3::new(-yaw.sin(), 0.0, -yaw.cos())
    };
    let right_dir = Vec3::new(yaw.cos(), 0.0, -yaw.sin());
    let dir = (forward_dir * forward + right_dir * strafe).clamp_length_max(1.0);
    action_state.0 = Inputs::Move {
        dir,
        jump,
        descend,
        climb: forward,
    };
}

/// When the entity we control is spawned, attach the input marker to it.
fn handle_controlled_spawn(
    trigger: On<Add, Controlled>,
    mut commands: Commands,
    players: Query<(), (With<Player>, Without<InputMarker<Inputs>>)>,
) {
    let entity = trigger.entity;
    if players.contains(entity) {
        commands
            .entity(entity)
            .insert(InputMarker::<Inputs>::default());
    }
}
