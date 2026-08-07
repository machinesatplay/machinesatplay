//! Shared movement-playground collision geometry and character shape.

#[cfg(any(feature = "client", feature = "server"))]
use crate::starter_map::STARTER_PARTS;
use avian3d::prelude::*;
use bevy::prelude::*;

use crate::protocol::SeatId;
use lightyear::avian3d::plugin::{AvianReplicationMode, LightyearAvianPlugin};
use lightyear::prelude::input::native::ActionState;

pub const CHARACTER_RADIUS: f32 = 0.28;
pub const CHARACTER_HEIGHT: f32 = 1.5;
pub const CHARACTER_GROUND_PROBE_HEIGHT: f32 = 0.04;
const CLIMB_INTERACTION_RADIUS: f32 = 0.45;

pub const SOLID_LAYER: u32 = 1 << 0;
pub const PLAYER_LAYER: u32 = 1 << 1;

pub fn solid_query_filter() -> SpatialQueryFilter {
    SpatialQueryFilter::from_mask(SOLID_LAYER)
}

#[derive(Resource)]
pub struct CharacterCollisionShape {
    pub body: Collider,
    pub ground_probe: Collider,
}

impl Default for CharacterCollisionShape {
    fn default() -> Self {
        Self {
            body: Collider::capsule(CHARACTER_RADIUS, CHARACTER_HEIGHT - CHARACTER_RADIUS * 2.0),
            ground_probe: Collider::cylinder(CHARACTER_RADIUS * 0.9, CHARACTER_GROUND_PROBE_HEIGHT),
        }
    }
}

#[derive(Bundle)]
pub struct PlayerPhysicsBundle {
    rigid_body: RigidBody,
    collider: Collider,
    layers: CollisionLayers,
    custom_position_integration: CustomPositionIntegration,
    speculative_margin: SpeculativeMargin,
    mass: Mass,
    collisions: crate::shared::CharacterCollisions,
}

impl Default for PlayerPhysicsBundle {
    fn default() -> Self {
        Self {
            rigid_body: RigidBody::Kinematic,
            collider: Collider::capsule(
                CHARACTER_RADIUS,
                CHARACTER_HEIGHT - CHARACTER_RADIUS * 2.0,
            ),
            layers: CollisionLayers::new(PLAYER_LAYER, SOLID_LAYER),
            custom_position_integration: CustomPositionIntegration,
            speculative_margin: SpeculativeMargin(0.0),
            mass: Mass(1.0),
            collisions: crate::shared::CharacterCollisions::default(),
        }
    }
}

#[derive(Bundle)]
pub struct WorldObjectPhysicsBundle {
    rigid_body: RigidBody,
    collider: Collider,
    layers: CollisionLayers,
    swept_ccd: SweptCcd,
    sleeping_disabled: SleepingDisabled,
    mass: Mass,
    friction: Friction,
    linear_damping: LinearDamping,
    angular_damping: AngularDamping,
}

impl WorldObjectPhysicsBundle {
    pub fn new(shape: &crate::protocol::WorldObjectShape) -> Self {
        Self {
            rigid_body: RigidBody::Dynamic,
            collider: world_object_collider(shape),
            layers: CollisionLayers::new(SOLID_LAYER, SOLID_LAYER | PLAYER_LAYER),
            swept_ccd: SweptCcd::default(),
            sleeping_disabled: SleepingDisabled,
            mass: Mass(2.5),
            friction: Friction::new(0.55),
            linear_damping: LinearDamping(0.6),
            angular_damping: AngularDamping(1.25),
        }
    }
}

fn world_object_collider(shape: &crate::protocol::WorldObjectShape) -> Collider {
    match shape {
        crate::protocol::WorldObjectShape::Sphere(radius) => Collider::sphere(*radius),
        crate::protocol::WorldObjectShape::Capsule {
            radius,
            half_height,
        } => Collider::capsule(*radius, half_height * 2.0),
        crate::protocol::WorldObjectShape::Cuboid(half) => {
            Collider::cuboid(half.x * 2.0, half.y * 2.0, half.z * 2.0)
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct WaterVolume {
    position: Vec3,
    size: Vec3,
}

#[derive(Resource, Default)]
pub struct WaterVolumes(Vec<WaterVolume>);

impl WaterVolumes {
    pub fn surface_for_character(&self, character_position: Vec3) -> Option<f32> {
        self.0
            .iter()
            .filter_map(|volume| {
                let local = character_position - volume.position;
                let half = volume.size * 0.5;
                let overlaps = local.x.abs() <= half.x
                    && local.z.abs() <= half.z
                    && local.y.abs() <= half.y + CHARACTER_HEIGHT * 0.5;
                overlaps.then_some(volume.position.y + half.y)
            })
            .max_by(f32::total_cmp)
    }
}

#[derive(Clone, Copy, Debug)]
struct InteractionVolume {
    position: Vec3,
    size: Vec3,
}

#[derive(Resource, Default)]
pub struct ClimbableVolumes(Vec<InteractionVolume>);

impl ClimbableVolumes {
    pub fn contains_character(&self, character_position: Vec3) -> bool {
        self.0
            .iter()
            .any(|volume| volume.overlaps_character(character_position))
    }

    /// Horizontal direction the character must face to look into the actual
    /// contacted surface. Characters can acquire a climb from a part's wide
    /// front/back or its narrow sides, so this uses the closest point on the
    /// horizontal AABB rather than assuming one authored climb axis.
    pub fn facing_direction_for_character(&self, character_position: Vec3) -> Option<Vec3> {
        self.0
            .iter()
            .filter(|volume| volume.overlaps_character(character_position))
            .min_by(|left, right| {
                left.horizontal_distance_squared(character_position)
                    .total_cmp(&right.horizontal_distance_squared(character_position))
            })
            .map(|volume| volume.facing_direction(character_position))
    }
}

impl InteractionVolume {
    fn overlaps_character(self, character_position: Vec3) -> bool {
        let local = character_position - self.position;
        let half = self.size * 0.5;
        local.x.abs() <= half.x + CLIMB_INTERACTION_RADIUS
            && local.z.abs() <= half.z + CLIMB_INTERACTION_RADIUS
            && local.y.abs() <= half.y + CHARACTER_HEIGHT * 0.5
    }

    fn horizontal_distance_squared(self, character_position: Vec3) -> f32 {
        let local = character_position - self.position;
        local.x * local.x + local.z * local.z
    }

    fn facing_direction(self, character_position: Vec3) -> Vec3 {
        let local = character_position - self.position;
        let half = self.size * 0.5;
        let closest = Vec3::new(
            local.x.clamp(-half.x, half.x),
            0.0,
            local.z.clamp(-half.z, half.z),
        );
        let toward_surface = closest - Vec3::new(local.x, 0.0, local.z);
        if let Some(direction) = toward_surface.try_normalize() {
            return direction;
        }

        // A center inside the AABB is only possible during a correction or
        // spawn overlap. Face its nearest boundary deterministically.
        let x_clearance = half.x - local.x.abs();
        let z_clearance = half.z - local.z.abs();
        if x_clearance <= z_clearance {
            Vec3::new(nonzero_sign(local.x), 0.0, 0.0)
        } else {
            Vec3::new(0.0, 0.0, nonzero_sign(local.z))
        }
    }
}

fn nonzero_sign(value: f32) -> f32 {
    if value < 0.0 {
        -1.0
    } else {
        1.0
    }
}

#[derive(Resource, Default)]
pub struct SeatVolumes(Vec<InteractionVolume>);

impl SeatVolumes {
    /// Seat identity and avatar root position for a character touching a seat.
    pub fn target_for_character(&self, character_position: Vec3) -> Option<(SeatId, Vec3)> {
        self.0.iter().enumerate().find_map(|(index, volume)| {
            let local = character_position - volume.position;
            let half = volume.size * 0.5;
            (local.x.abs() <= half.x + CHARACTER_RADIUS
                && local.z.abs() <= half.z + CHARACTER_RADIUS
                && local.y.abs() <= half.y + CHARACTER_HEIGHT * 0.5)
                .then_some((
                    SeatId(index as u16),
                    Vec3::new(
                        volume.position.x,
                        volume.position.y + half.y + crate::shared::GROUND_Y,
                        volume.position.z,
                    ),
                ))
        })
    }

    /// Root position of a known seat. Used to keep its owner attached without
    /// accidentally switching to a neighboring overlapping seat volume.
    pub fn target(&self, seat: SeatId) -> Option<Vec3> {
        self.0.get(seat.0 as usize).map(|volume| {
            let half = volume.size * 0.5;
            Vec3::new(
                volume.position.x,
                volume.position.y + half.y + crate::shared::GROUND_Y,
                volume.position.z,
            )
        })
    }

    /// Canonical facing for a known seat. Every observer derives this from the
    /// replicated seat identity, so local prediction and remote interpolation
    /// cannot leave the same seated avatar facing different directions.
    ///
    /// Parts do not yet carry an authored rotation, so a seat faces along its
    /// shorter horizontal axis. The positive axis is the deterministic default.
    pub fn facing_yaw(&self, seat: SeatId) -> Option<f32> {
        self.0.get(seat.0 as usize).map(|volume| {
            if volume.size.x >= volume.size.z {
                0.0
            } else {
                core::f32::consts::FRAC_PI_2
            }
        })
    }
}

pub struct GamePhysicsPlugin;

#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GamePhysicsSet {
    CharacterMovement,
    PlayerLifecycle,
    PushDynamicBodies,
}

impl Plugin for GamePhysicsPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Gravity(Vec3::NEG_Y * crate::shared::WORLD_GRAVITY));
        app.add_plugins(LightyearAvianPlugin {
            replication_mode: AvianReplicationMode::Position,
            ..default()
        });
        app.add_plugins(
            PhysicsPlugins::default()
                .build()
                .disable::<PhysicsTransformPlugin>()
                .disable::<PhysicsInterpolationPlugin>()
                .disable::<IslandPlugin>()
                .disable::<IslandSleepingPlugin>(),
        );
        app.init_resource::<CharacterCollisionShape>();
        app.init_resource::<WaterVolumes>();
        app.init_resource::<ClimbableVolumes>();
        app.init_resource::<SeatVolumes>();
        #[cfg(any(feature = "client", feature = "server"))]
        app.add_systems(Startup, spawn_starter_map_colliders);
        app.add_systems(
            FixedUpdate,
            (
                simulate_player_movement.in_set(GamePhysicsSet::CharacterMovement),
                crate::shared::update_player_lifecycle
                    .in_set(GamePhysicsSet::PlayerLifecycle)
                    .after(GamePhysicsSet::CharacterMovement),
                apply_character_pushes
                    .in_set(GamePhysicsSet::PushDynamicBodies)
                    .after(GamePhysicsSet::PlayerLifecycle),
            )
                .chain(),
        );
    }
}

#[doc(hidden)]
#[allow(clippy::type_complexity)]
pub fn simulate_player_movement(
    collision_shape: Res<CharacterCollisionShape>,
    water_volumes: Res<WaterVolumes>,
    climbable_volumes: Res<ClimbableVolumes>,
    seat_volumes: Res<SeatVolumes>,
    mut params: ParamSet<(
        Query<(
            Entity,
            &mut crate::protocol::Player,
            &ActionState<crate::protocol::Inputs>,
            &mut Position,
            &mut LinearVelocity,
            &mut crate::shared::CharacterCollisions,
        )>,
        MoveAndSlide,
    )>,
) {
    let (occupied, pending) = {
        let mut players = params.p0();
        let occupied = players
            .iter()
            .filter_map(|(entity, player, ..)| player.seated.map(|seat| (entity, seat)))
            .collect::<Vec<_>>();
        let pending = players
            .iter_mut()
            .map(|(entity, player, input, position, velocity, _)| {
                (
                    entity,
                    player.clone(),
                    input.0.clone(),
                    *position,
                    *velocity,
                )
            })
            .collect::<Vec<_>>();
        (occupied, pending)
    };

    for (entity, mut player, input, mut position, mut velocity) in pending {
        let occupied_by_other = occupied
            .iter()
            .filter_map(|(owner, seat)| (*owner != entity).then_some(*seat))
            .collect::<Vec<_>>();
        let mut hits = Vec::new();
        {
            let move_and_slide = params.p1();
            crate::shared::apply_player_movement(
                entity,
                &mut player,
                &mut position,
                &mut velocity,
                &input,
                &collision_shape,
                &water_volumes,
                &climbable_volumes,
                &seat_volumes,
                &occupied_by_other,
                &move_and_slide,
                |collision| hits.push(collision),
            );
        }
        if let Ok((_, mut live_player, _, mut live_position, mut live_velocity, mut collisions)) =
            params.p0().get_mut(entity)
        {
            *live_player = player;
            *live_position = position;
            *live_velocity = velocity;
            collisions.extend(hits);
        }
    }
}

#[doc(hidden)]
pub fn apply_character_pushes(
    mut characters: Query<(&ComputedMass, &mut crate::shared::CharacterCollisions)>,
    colliders: Query<&ColliderOf>,
    mut rigid_bodies: Query<(&RigidBody, Forces)>,
) {
    for (mass, mut collisions) in &mut characters {
        let mass = mass.value();
        let mut pushed_bodies = std::collections::HashSet::new();
        for collision in collisions.iter() {
            let Ok(collider_of) = colliders.get(collision.collider) else {
                continue;
            };
            if !pushed_bodies.insert(collider_of.body) {
                continue;
            }
            let Ok((rigid_body, mut forces)) = rigid_bodies.get_mut(collider_of.body) else {
                continue;
            };
            if !rigid_body.is_dynamic() {
                continue;
            }

            let touch_direction = -collision.normal;
            let relative_velocity = collision.character_velocity - forces.linear_velocity();
            let touch_velocity = touch_direction.dot(relative_velocity) * touch_direction;
            forces.apply_linear_impulse_at_point(touch_velocity * mass, collision.point);
        }
        collisions.clear();
    }
}

#[cfg(any(feature = "client", feature = "server"))]
fn spawn_starter_map_colliders(mut commands: Commands) {
    let mut water = Vec::new();
    let mut climbable = Vec::new();
    let mut seats = Vec::new();
    let mut solid_count = 0;
    for part in STARTER_PARTS {
        let position = part.position();
        let size = part.size();
        if part.swimmable {
            water.push(WaterVolume { position, size });
        }
        if part.climbable {
            climbable.push(InteractionVolume { position, size });
        }
        if part.seat {
            seats.push(InteractionVolume { position, size });
        }
        if part.collidable {
            commands.spawn((
                Name::new(format!("{} Collider", part.name)),
                RigidBody::Static,
                Collider::cuboid(size.x, size.y, size.z),
                CollisionLayers::new(SOLID_LAYER, SOLID_LAYER | PLAYER_LAYER),
                Position(position),
                Rotation::default(),
            ));
            solid_count += 1;
        }
    }
    let water_count = water.len();
    commands.insert_resource(WaterVolumes(water));
    commands.insert_resource(ClimbableVolumes(climbable));
    commands.insert_resource(SeatVolumes(seats));
    info!("spawned movement playground with {solid_count} solids and {water_count} water volumes");
}
