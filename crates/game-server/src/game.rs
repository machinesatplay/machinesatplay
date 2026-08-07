//! Source-owned authoritative game server.

use avian3d::prelude::{LinearVelocity, PhysicsSystems, Position, Rotation};
use bevy::prelude::*;
use game_core::physics::PlayerPhysicsBundle;
use game_core::protocol::*;
use game_core::shared;
use game_core::shared::SEND_INTERVAL;
use lightyear::connection::client::Connected;
use lightyear::netcode::TokenUserData;
use lightyear::prelude::input::native::*;
use lightyear::prelude::server::*;
use lightyear::prelude::*;
pub struct GameServerPlugin;

impl Plugin for GameServerPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ReplicationMetadata::new(SEND_INTERVAL));
        app.add_systems(Startup, spawn_world_sky_state);
        app.add_systems(FixedUpdate, apply_world_sky_requests);
        app.add_systems(
            FixedPostUpdate,
            destroy_fallen_world_objects.after(PhysicsSystems::Writeback),
        );
        app.add_observer(handle_new_client);
        app.add_observer(handle_connected);
    }
}

fn spawn_world_sky_state(mut commands: Commands) {
    commands.spawn((
        Name::new("World Sky State"),
        WorldSkyState::default(),
        Replicate::to_clients(NetworkTarget::All),
    ));
}

fn apply_world_sky_requests(
    mut sky: Query<&mut WorldSkyState>,
    mut clients: Query<&mut MessageReceiver<SetWorldSky>, With<ClientOf>>,
) {
    let mut requested = None;
    for mut receiver in &mut clients {
        for message in receiver.receive() {
            requested = Some(message.night);
        }
    }
    let Some(night) = requested else {
        return;
    };
    let Ok(mut sky) = sky.single_mut() else {
        return;
    };
    sky.night = night;
}

fn destroy_fallen_world_objects(
    mut commands: Commands,
    objects: Query<(Entity, &Position), With<WorldObject>>,
) {
    for (entity, position) in &objects {
        if shared::fell_out_of_world(position.y) {
            commands.entity(entity).try_despawn();
        }
    }
}

fn handle_new_client(trigger: On<Add, LinkOf>, mut commands: Commands) {
    commands
        .entity(trigger.entity)
        .insert((ReplicationSender, Name::from("ClientLink")));
}

fn handle_connected(
    trigger: On<Add, Connected>,
    clients: Query<(&RemoteId, Option<&TokenUserData>), With<ClientOf>>,
    existing_players: Query<(Entity, &Player, &ControlledBy)>,
    mut commands: Commands,
) {
    let Ok((session_id, token_user_data)) = clients.get(trigger.entity) else {
        return;
    };
    let session_id = session_id.0;
    let logical_id = token_user_data
        .and_then(|data| shared::decode_player_identity(&data.0))
        .map(PeerId::Netcode)
        .unwrap_or(session_id);
    for (entity, player, controlled_by) in &existing_players {
        if player.id == logical_id && controlled_by.owner != trigger.entity {
            commands.entity(entity).try_despawn();
        }
    }
    let entity = spawn_player(&mut commands, trigger.entity, session_id, logical_id);
    info!("spawned player {entity:?} for {logical_id:?} on {session_id:?}");
}

fn player_network_targets(owner: PeerId) -> (NetworkTarget, NetworkTarget) {
    (
        NetworkTarget::Single(owner),
        NetworkTarget::AllExceptSingle(owner),
    )
}

fn spawn_player(
    commands: &mut Commands,
    owner: Entity,
    owner_id: PeerId,
    logical_id: PeerId,
) -> Entity {
    let (prediction_target, interpolation_target) = player_network_targets(owner_id);
    commands
        .spawn((
            Player::new(logical_id),
            ActionState::<Inputs>::default(),
            PlayerPhysicsBundle::default(),
            Position(shared::playground_spawn(logical_id)),
            Rotation::default(),
            LinearVelocity::ZERO,
            Replicate::to_clients(NetworkTarget::All),
            PredictionTarget::to_clients(prediction_target),
            InterpolationTarget::to_clients(interpolation_target),
            ControlledBy {
                owner,
                lifetime: Default::default(),
            },
        ))
        .id()
}
