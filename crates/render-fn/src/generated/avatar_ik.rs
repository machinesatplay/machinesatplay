//! Two-bone foot-planting IK for the articulated avatar body.
//!
//! Each leg is a hip → knee → ankle chain. While the character is supported, a
//! foot that comes close enough to the ground locks to a world-space target;
//! the solver bends the knee to keep the planted foot still until it drifts too
//! far or the animation lifts it, then blends back to the animated pose. Flexion
//! limits keep the knee slightly bent through the straight-leg singularity and
//! on its anatomical side.
//!
//! Runs after the generated animator and before Bevy propagates
//! transforms, so joint world positions are composed manually from local
//! transforms along the (shallow) chain.

use bevy::prelude::*;

use super::{
    AnimState, PlayerClimbing, PlayerGrounded, PlayerPosition, PlayerSeated, PlayerSwimming,
    PlayerVisual, SemanticClock, BODY_SCALE, GROUND_Y,
};

pub(crate) struct AvatarIkPlugin;

impl Plugin for AvatarIkPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, tag_foot_chains);
    }
}

/// Ankle rest height above the feet, in world units (the baked skeleton's
/// `ankle_*` joints sit at y=1.0 body units).
const ANKLE_REST: f32 = 1.0 * BODY_SCALE;
/// A foot at most this high locks to the ground.
const CONTACT_HEIGHT: f32 = ANKLE_REST + 0.02;
/// A locked foot this high has been lifted by the animation and releases.
const RELEASE_HEIGHT: f32 = ANKLE_REST + 0.055;
/// A locked foot dragged this far horizontally from its target releases and
/// stays free until the animation lifts it again (prevents re-lock jitter).
const MAX_LOCK_DISTANCE: f32 = 0.12;
/// Flexion limits, measured from the straight limb.
const MIN_BEND: f32 = 3.0 * core::f32::consts::PI / 180.0;
const MAX_BEND: f32 = 150.0 * core::f32::consts::PI / 180.0;

/// Foot-planting state, carried by each ankle joint entity.
#[derive(Component)]
pub(crate) struct FootIk {
    player: Entity,
    upper: Entity,
    lower: Entity,
    /// Rest local rotation of the knee. The animator resets the hip every
    /// frame but never writes the knee, so the solver must restore it itself
    /// or its own adjustments accumulate across frames.
    lower_rest: Quat,
    /// Preferred knee direction in player-root space, from the rest pose
    /// (the knees point toward authored forward, +Z).
    bend_local: Vec3,
    weight: f32,
    locked: bool,
    blocked_until_lift: bool,
    target: Vec3,
}

/// Finds ankle joints in freshly spawned skinned bodies and records their
/// hip → knee → ankle chain plus the rest-pose bend direction.
fn tag_foot_chains(
    mut commands: Commands,
    added: Query<(Entity, &Name), Added<Name>>,
    parents: Query<&ChildOf>,
    transforms: Query<&Transform>,
    players: Query<(), With<PlayerVisual>>,
) {
    for (entity, name) in &added {
        if !matches!(name.as_str(), "ankle_l" | "ankle_r") {
            continue;
        }
        let Ok(lower) = parents.get(entity).map(ChildOf::parent) else {
            continue;
        };
        let Ok(upper) = parents.get(lower).map(ChildOf::parent) else {
            continue;
        };
        let mut player = None;
        let mut current = upper;
        while let Ok(child_of) = parents.get(current) {
            current = child_of.parent();
            if players.contains(current) {
                player = Some(current);
                break;
            }
        }
        let Some(player) = player else {
            continue;
        };

        // Rest positions relative to the player root (transforms are still
        // the spawned rest pose here).
        let position_of = |e: Entity| local_to_player(e, player, &transforms, &parents);
        let (Some(hip), Some(knee), Some(ankle)) =
            (position_of(upper), position_of(lower), position_of(entity))
        else {
            continue;
        };
        // Preferred bend: the knee's rest offset from the hip → ankle axis.
        let axis = (ankle - hip).normalize_or_zero();
        let mut bend = knee - hip;
        bend -= axis * bend.dot(axis);
        let bend_local = bend.try_normalize().unwrap_or(Vec3::Z);

        let Ok(lower_rest) = transforms.get(lower).map(|t| t.rotation) else {
            continue;
        };
        commands.entity(entity).insert(FootIk {
            player,
            upper,
            lower,
            lower_rest,
            bend_local,
            weight: 0.0,
            locked: false,
            blocked_until_lift: false,
            target: Vec3::ZERO,
        });
    }
}

/// The state machine + solve pass. Registered by the render plugin directly
/// after `animate_character` in `PostUpdate`.
pub(crate) fn apply_foot_ik(
    clock: Res<SemanticClock>,
    players: Query<(
        &PlayerPosition,
        &PlayerGrounded,
        &PlayerSwimming,
        &PlayerClimbing,
        &PlayerSeated,
        &AnimState,
        &Transform,
    )>,
    mut feet: Query<(Entity, &mut FootIk)>,
    mut transforms: Query<&mut Transform, Without<PlayerPosition>>,
    parents: Query<&ChildOf>,
) {
    let dt = (clock.delta as f32).min(1.0 / 20.0);
    for (end, mut foot) in &mut feet {
        let Ok((position, grounded, swimming, climbing, seated, anim, player_transform)) =
            players.get(foot.player)
        else {
            continue;
        };
        let supported =
            grounded.0 && !swimming.0 && !climbing.0 && !seated.0 && anim.has_walk_motion();
        let ground_y = supported.then_some(position.0.y - GROUND_Y);

        if dt > 0.0 && !supported {
            // A planted world-space target can otherwise hold the ankles at
            // the last stride locations forever, leaving an idle character
            // visibly crossed mid-step. Idle owns the exact rest pose.
            foot.locked = false;
            foot.blocked_until_lift = false;
            foot.weight = 0.0;
        }

        // Restore the knee to its animated (rest) pose before anything else:
        // this frame's solve must start from the animation, not from last
        // frame's IK result.
        if let Ok(mut lower_transform) = transforms.get_mut(foot.lower) {
            lower_transform.rotation = foot.lower_rest;
        }

        let chain = [foot.upper, foot.lower, end];
        let Some(end_world) = world_of(end, foot.player, player_transform, &transforms, &parents)
        else {
            continue;
        };
        let height = match ground_y {
            Some(ground) => end_world.translation.y - ground,
            None => f32::INFINITY,
        };

        if dt > 0.0 && height > RELEASE_HEIGHT {
            foot.locked = false;
            foot.blocked_until_lift = false;
        }
        if dt > 0.0 && foot.locked {
            let drift = (end_world.translation.xz() - foot.target.xz()).length();
            if ground_y.is_none() || drift > MAX_LOCK_DISTANCE {
                foot.locked = false;
                foot.blocked_until_lift = true;
            }
        }
        if dt > 0.0 {
            if let Some(ground) = ground_y {
                if !foot.locked && !foot.blocked_until_lift && height <= CONTACT_HEIGHT {
                    foot.locked = true;
                    foot.target = Vec3::new(
                        end_world.translation.x,
                        ground + ANKLE_REST,
                        end_world.translation.z,
                    );
                }
                if foot.locked {
                    foot.target.y = ground + ANKLE_REST;
                }
            }
        }

        if dt > 0.0 {
            foot.weight = advance_ik_weight(foot.weight, foot.locked, dt);
        }

        let bend_world = player_transform.rotation * foot.bend_local;

        // With no planted foot the animation passes through untouched; the
        // solver only bends the leg while a contact is blending in or out.
        if foot.weight > 0.001 {
            let target = foot.target;
            let weight = foot.weight;
            solve_two_bone(
                &chain,
                target,
                weight,
                bend_world,
                foot.player,
                player_transform,
                &mut transforms,
                &parents,
            );
        }
    }
}

fn advance_ik_weight(current: f32, locked: bool, dt: f32) -> f32 {
    let target = if locked { 1.0 } else { 0.0 };
    let speed = if locked { 18.0 } else { 12.0 };
    current + (target - current) * (1.0 - (-speed * dt).exp())
}

/// Clamp the target onto the reachable shell, place the knee on the preferred
/// side, then aim the hip at the knee target and the knee at the effective end
/// target.
#[allow(clippy::too_many_arguments)]
fn solve_two_bone(
    chain: &[Entity; 3],
    target: Vec3,
    weight: f32,
    bend_world: Vec3,
    player: Entity,
    player_transform: &Transform,
    transforms: &mut Query<&mut Transform, Without<PlayerPosition>>,
    parents: &Query<&ChildOf>,
) {
    let [upper, lower, end] = *chain;
    let world = |e: Entity, transforms: &Query<&mut Transform, Without<PlayerPosition>>| {
        world_of(e, player, player_transform, transforms, parents)
    };
    let (Some(upper_world), Some(lower_world), Some(end_world)) = (
        world(upper, transforms),
        world(lower, transforms),
        world(end, transforms),
    ) else {
        return;
    };
    let upper_position = upper_world.translation;
    let upper_length = upper_position.distance(lower_world.translation);
    let lower_length = lower_world.translation.distance(end_world.translation);
    if upper_length <= 1e-5 || lower_length <= 1e-5 {
        return;
    }

    let mut direction = target - upper_position;
    let requested = direction.length();
    if requested <= 1e-5 {
        return;
    }
    direction /= requested;
    // Full stretch keeps a slight bend so the knee never hyperextends through
    // the straight-limb singularity; close targets stop at the flexion cap.
    // Unreachable targets clamp onto this shell instead of flipping.
    let distance = requested.clamp(
        bent_chain_reach(upper_length, lower_length, MAX_BEND)
            .max((upper_length - lower_length).abs() + 1e-5),
        bent_chain_reach(upper_length, lower_length, MIN_BEND)
            .min(upper_length + lower_length - 1e-5),
    );
    let along = (upper_length * upper_length - lower_length * lower_length + distance * distance)
        / (2.0 * distance);
    let bend_height = (upper_length * upper_length - along * along)
        .max(0.0)
        .sqrt();

    let mut bend = bend_world - direction * bend_world.dot(direction);
    if bend.length_squared() <= 1e-4 {
        // No stable preference survives projection: keep the current side.
        bend = lower_world.translation - upper_position;
        bend -= direction * bend.dot(direction);
        if bend.length_squared() <= 1e-8 {
            bend = Vec3::Z;
            if bend.dot(direction).abs() > 0.95 {
                bend = Vec3::X;
            }
            bend -= direction * bend.dot(direction);
        }
    }
    let bend = bend.normalize();
    let desired_lower = upper_position + direction * along + bend * bend_height;
    let effective_target = upper_position + direction * distance;

    rotate_bone_toward(
        upper,
        lower,
        desired_lower,
        weight,
        player,
        player_transform,
        transforms,
        parents,
    );
    rotate_bone_toward(
        lower,
        end,
        effective_target,
        weight,
        player,
        player_transform,
        transforms,
        parents,
    );
}

/// Law of cosines, with flexion measured from the straight limb.
fn bent_chain_reach(upper_length: f32, lower_length: f32, bend: f32) -> f32 {
    (upper_length * upper_length
        + lower_length * lower_length
        + 2.0 * upper_length * lower_length * bend.cos())
    .max(0.0)
    .sqrt()
}

/// Rotates `bone` (in local space) so its child points at `target`, blended
/// by `weight`.
#[allow(clippy::too_many_arguments)]
fn rotate_bone_toward(
    bone: Entity,
    child: Entity,
    target: Vec3,
    weight: f32,
    player: Entity,
    player_transform: &Transform,
    transforms: &mut Query<&mut Transform, Without<PlayerPosition>>,
    parents: &Query<&ChildOf>,
) {
    let (Some(bone_world), Some(child_world)) = (
        world_of(bone, player, player_transform, transforms, parents),
        world_of(child, player, player_transform, transforms, parents),
    ) else {
        return;
    };
    let current = (child_world.translation - bone_world.translation).normalize_or_zero();
    let desired = (target - bone_world.translation).normalize_or_zero();
    if current == Vec3::ZERO || desired == Vec3::ZERO {
        return;
    }
    let delta = Quat::from_rotation_arc(current, desired);
    let desired_world = delta * bone_world.rotation;
    let parent_rotation = parents
        .get(bone)
        .ok()
        .and_then(|child_of| {
            world_of(
                child_of.parent(),
                player,
                player_transform,
                transforms,
                parents,
            )
        })
        .map_or(Quat::IDENTITY, |t| t.rotation);
    let desired_local = parent_rotation.inverse() * desired_world;
    let Ok(mut transform) = transforms.get_mut(bone) else {
        return;
    };
    transform.rotation = transform
        .rotation
        .slerp(desired_local, weight.clamp(0.0, 1.0));
}

/// World transform of a joint, composed manually up to (and including) the
/// player root; transform propagation has not run yet this frame.
fn world_of(
    entity: Entity,
    player: Entity,
    player_transform: &Transform,
    transforms: &Query<&mut Transform, Without<PlayerPosition>>,
    parents: &Query<&ChildOf>,
) -> Option<Transform> {
    let local = local_to_player_transform(entity, player, transforms, parents)?;
    Some(player_transform.mul_transform(local))
}

/// Position of a joint relative to the player root (rest-pose helper).
fn local_to_player(
    entity: Entity,
    player: Entity,
    transforms: &Query<&Transform>,
    parents: &Query<&ChildOf>,
) -> Option<Vec3> {
    let mut chain = Vec::new();
    let mut current = entity;
    while current != player {
        chain.push(current);
        current = parents.get(current).ok()?.parent();
    }
    let mut acc = Transform::IDENTITY;
    for &e in chain.iter().rev() {
        acc = acc.mul_transform(*transforms.get(e).ok()?);
    }
    Some(acc.translation)
}

fn local_to_player_transform(
    entity: Entity,
    player: Entity,
    transforms: &Query<&mut Transform, Without<PlayerPosition>>,
    parents: &Query<&ChildOf>,
) -> Option<Transform> {
    let mut chain = Vec::new();
    let mut current = entity;
    while current != player {
        chain.push(current);
        current = parents.get(current).ok()?.parent();
    }
    let mut acc = Transform::IDENTITY;
    for &e in chain.iter().rev() {
        acc = acc.mul_transform(*transforms.get(e).ok()?);
    }
    Some(acc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_semantic_delta_does_not_advance_ik_weight() {
        assert_eq!(advance_ik_weight(0.37, true, 0.0), 0.37);
        assert_eq!(advance_ik_weight(0.37, false, 0.0), 0.37);
        assert!(advance_ik_weight(0.37, true, 1.0 / 60.0) > 0.37);
        assert!(advance_ik_weight(0.37, false, 1.0 / 60.0) < 0.37);
    }

    #[test]
    fn two_bone_reach_shrinks_as_flexion_increases() {
        let nearly_straight = bent_chain_reach(1.0, 1.0, MIN_BEND);
        let deeply_bent = bent_chain_reach(1.0, 1.0, MAX_BEND);
        assert!(nearly_straight > deeply_bent);
        assert!(nearly_straight <= 2.0);
        assert!(deeply_bent > 0.0);
    }
}
