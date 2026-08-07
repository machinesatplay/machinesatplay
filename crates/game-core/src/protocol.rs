//! The networked protocol: which components and inputs are replicated.

#[cfg(any(feature = "client", feature = "server"))]
use avian3d::prelude::{AngularVelocity, LinearVelocity, Position, Rotation};
use bevy::ecs::entity::MapEntities;
use bevy::math::Curve;
use bevy::prelude::*;
use lightyear::prelude::*;
use serde::{Deserialize, Serialize};
#[cfg(any(feature = "client", feature = "server"))]
use std::time::Duration;

pub struct GameCommandChannel;

#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorldSkyState {
    pub night: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetWorldSky {
    pub night: bool,
}

/// Stable index of a seat in the authored map.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Reflect)]
pub struct SeatId(pub u16);

/// Complete replicated gameplay state for one player.
#[derive(Component, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Player {
    pub id: PeerId,
    pub facing: Vec3,
    pub alive: bool,
    pub grounded: bool,
    pub swimming: bool,
    pub climbing: bool,
    pub seated: Option<SeatId>,
    pub seat_cooldown: u8,
    pub jump_held: bool,
    pub jump_release_ticks: u8,
    pub respawn_ticks_remaining: u8,
    pub color: Color,
}

impl Player {
    pub fn new(id: PeerId) -> Self {
        // A compact set of authored team colors. A restricted palette keeps
        // every player vivid under the same lighting instead of generating
        // arbitrary HSL hues that can become neon, muddy, or over-saturated.
        const WII_PLAYER_COLORS: [u32; 12] = [
            0x204898, 0xf07828, 0xf8d820, 0x80c828, 0x007428, 0xb84030, 0x40a0d8, 0xe86078,
            0x702ca8, 0x483818, 0xe0e0e0, 0x181814,
        ];
        let rgb = WII_PLAYER_COLORS[id.to_bits() as usize % WII_PLAYER_COLORS.len()];
        let red = ((rgb >> 16) & 0xff) as u8;
        let green = ((rgb >> 8) & 0xff) as u8;
        let blue = (rgb & 0xff) as u8;
        Self {
            id,
            facing: Vec3::Z,
            alive: true,
            grounded: false,
            swimming: false,
            climbing: false,
            seated: None,
            seat_cooldown: 0,
            jump_held: false,
            jump_release_ticks: 0,
            respawn_ticks_remaining: 0,
            color: Color::srgb_u8(red, green, blue),
        }
    }
}

impl Ease for Player {
    fn interpolating_curve_unbounded(start: Self, end: Self) -> impl Curve<Self> {
        FunctionCurve::new(Interval::UNIT, move |t| {
            let mut player = if t < 1.0 { start.clone() } else { end.clone() };
            let facing = start.facing.lerp(end.facing, t);
            player.facing = facing.try_normalize().unwrap_or(end.facing);
            player
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum WorldObjectShape {
    Sphere(f32),
    Capsule { radius: f32, half_height: f32 },
    Cuboid(Vec3),
}

#[derive(Component, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WorldObject {
    pub shape: WorldObjectShape,
    pub tint: u32,
}

/// Camera-relative movement direction, clamped to unit length. Ground movement
/// ignores Y; swimming uses all three axes.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Reflect)]
pub enum Inputs {
    Move {
        dir: Vec3,
        jump: bool,
        descend: bool,
        /// Raw forward input: W/Up = 1, S/Down = -1.
        climb: f32,
    },
}

impl Default for Inputs {
    fn default() -> Self {
        Self::Move {
            dir: Vec3::ZERO,
            jump: false,
            descend: false,
            climb: 0.0,
        }
    }
}

impl MapEntities for Inputs {
    fn map_entities<M: EntityMapper>(&mut self, _entity_mapper: &mut M) {}
}

#[derive(Clone)]
pub struct ProtocolPlugin;

#[cfg(any(feature = "client", feature = "server"))]
impl Plugin for ProtocolPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(input::native::InputPlugin::<Inputs> {
            config: input::InputConfig::<Inputs>::default(),
        });
        app.add_channel::<GameCommandChannel>(ChannelSettings {
            mode: ChannelMode::OrderedReliable(ReliableSettings::default()),
            send_frequency: Duration::ZERO,
            priority: 2.0,
        })
        .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<SetWorldSky>()
            .add_direction(NetworkDirection::ClientToServer);
        app.component::<WorldSkyState>().replicate();

        app.component::<Player>()
            .replicate()
            .predict()
            .add_linear_interpolation();

        app.component::<Position>()
            .replicate()
            .predict()
            .with_rollback_condition(position_should_rollback)
            .add_linear_interpolation();
        app.component::<Rotation>()
            .replicate()
            .predict()
            .with_rollback_condition(rotation_should_rollback)
            .add_linear_interpolation();
        app.component::<LinearVelocity>()
            .replicate()
            .predict()
            .with_rollback_condition(linear_velocity_should_rollback)
            .add_interpolation_with(|start, end, t| {
                lightyear::avian3d::types::linear_velocity::lerp(&start, &end, t)
            });
        app.component::<AngularVelocity>()
            .replicate()
            .predict()
            .with_rollback_condition(angular_velocity_should_rollback)
            .add_interpolation_with(|start, end, t| {
                lightyear::avian3d::types::angular_velocity::lerp(&start, &end, t)
            });
        app.component::<WorldObject>().replicate();
    }
}

#[cfg(any(feature = "client", feature = "server"))]
fn position_should_rollback(confirmed: &Position, predicted: &Position) -> bool {
    !confirmed.0.is_finite()
        || !predicted.0.is_finite()
        || (confirmed.0 - predicted.0).length() >= 0.01
}

#[cfg(any(feature = "client", feature = "server"))]
fn rotation_should_rollback(confirmed: &Rotation, predicted: &Rotation) -> bool {
    confirmed.angle_between(*predicted) >= 0.01
}

#[cfg(any(feature = "client", feature = "server"))]
fn linear_velocity_should_rollback(confirmed: &LinearVelocity, predicted: &LinearVelocity) -> bool {
    !confirmed.0.is_finite()
        || !predicted.0.is_finite()
        || (confirmed.0 - predicted.0).length() >= 0.01
}

#[cfg(any(feature = "client", feature = "server"))]
fn angular_velocity_should_rollback(
    confirmed: &AngularVelocity,
    predicted: &AngularVelocity,
) -> bool {
    (confirmed.0 - predicted.0).length() >= 0.01
}

#[cfg(not(any(feature = "client", feature = "server")))]
impl Plugin for ProtocolPlugin {
    fn build(&self, _app: &mut App) {}
}
