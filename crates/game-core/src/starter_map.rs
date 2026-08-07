//! Default movement-playground map.
//!
//! These definitions are the single source of truth for rendering, solid
//! collision, and swimmable volumes.

use bevy::prelude::*;

pub const MAP_SCALE: f32 = 0.3;
pub const KILL_PLANE_WORLD_Y: f32 = -500.0 * MAP_SCALE;
pub const PLAYGROUND_SPAWNS: [Vec2; 8] = [
    Vec2::new(-0.9, 0.0),
    Vec2::new(-0.3, 0.0),
    Vec2::new(0.3, 0.0),
    Vec2::new(0.9, 0.0),
    Vec2::new(-0.9, 0.7),
    Vec2::new(-0.3, 0.7),
    Vec2::new(0.3, 0.7),
    Vec2::new(0.9, 0.7),
];

#[derive(Clone, Copy, Debug)]
pub struct StarterPart {
    pub name: &'static str,
    pub source_position: Vec3,
    pub source_size: Vec3,
    pub color: u32,
    pub alpha: u8,
    pub material_id: u8,
    pub collidable: bool,
    pub swimmable: bool,
    pub climbable: bool,
    pub seat: bool,
}

impl StarterPart {
    pub const fn position(self) -> Vec3 {
        Vec3::new(
            self.source_position.x * MAP_SCALE,
            self.source_position.y * MAP_SCALE,
            self.source_position.z * MAP_SCALE,
        )
    }

    pub const fn size(self) -> Vec3 {
        Vec3::new(
            self.source_size.x * MAP_SCALE,
            self.source_size.y * MAP_SCALE,
            self.source_size.z * MAP_SCALE,
        )
    }
}

const fn part(
    name: &'static str,
    position: Vec3,
    size: Vec3,
    color: u32,
    material_id: u8,
) -> StarterPart {
    StarterPart {
        name,
        source_position: position,
        source_size: size,
        color,
        alpha: 255,
        material_id,
        collidable: true,
        swimmable: false,
        climbable: false,
        seat: false,
    }
}

const fn climbable_part(
    name: &'static str,
    position: Vec3,
    size: Vec3,
    color: u32,
    material_id: u8,
) -> StarterPart {
    let mut value = part(name, position, size, color, material_id);
    value.climbable = true;
    value
}

const fn seat_part(
    name: &'static str,
    position: Vec3,
    size: Vec3,
    color: u32,
    material_id: u8,
) -> StarterPart {
    let mut value = part(name, position, size, color, material_id);
    value.seat = true;
    value
}

pub const STARTER_PARTS: [StarterPart; 13] = [
    part(
        "Baseplate",
        Vec3::new(0.0, -0.5, 0.0),
        Vec3::new(80.0, 1.0, 80.0),
        0x67a84b,
        14,
    ),
    part(
        "JumpPlatformLow",
        Vec3::new(-14.0, 1.0, 0.0),
        Vec3::new(6.0, 2.0, 6.0),
        0xf8d820,
        1,
    ),
    part(
        "JumpPlatformMid",
        Vec3::new(-22.0, 2.0, 0.0),
        Vec3::new(6.0, 4.0, 6.0),
        0xf07828,
        1,
    ),
    part(
        "JumpPlatformHigh",
        Vec3::new(-30.0, 3.0, 0.0),
        Vec3::new(6.0, 6.0, 6.0),
        0xc95648,
        1,
    ),
    part(
        "StoneSoundPad",
        Vec3::new(10.0, 0.15, -12.0),
        Vec3::new(8.0, 0.3, 8.0),
        0xb4b8bc,
        6,
    ),
    part(
        "MetalSoundPad",
        Vec3::new(-10.0, 0.15, -12.0),
        Vec3::new(8.0, 0.3, 8.0),
        0x8495a8,
        12,
    ),
    part(
        "WoodSoundPad",
        Vec3::new(0.0, 0.15, -24.0),
        Vec3::new(8.0, 0.3, 8.0),
        0xa87342,
        4,
    ),
    StarterPart {
        name: "Pool",
        source_position: Vec3::new(24.0, 4.0, 0.0),
        source_size: Vec3::new(24.0, 8.0, 24.0),
        color: 0x338ce6,
        alpha: 128,
        material_id: 0,
        collidable: false,
        swimmable: true,
        climbable: false,
        seat: false,
    },
    StarterPart {
        name: "Shallows",
        source_position: Vec3::new(24.0, 0.6, 22.0),
        source_size: Vec3::new(14.0, 1.2, 10.0),
        color: 0x55a8e8,
        alpha: 115,
        material_id: 0,
        collidable: false,
        swimmable: true,
        climbable: false,
        seat: false,
    },
    climbable_part(
        "ClimbColumn",
        Vec3::new(0.0, 6.0, 24.0),
        Vec3::new(6.0, 12.0, 1.0),
        0xd2d5da,
        1,
    ),
    part(
        "ClimbTopPlatform",
        Vec3::new(0.0, 12.25, 28.5),
        Vec3::new(10.0, 0.5, 8.0),
        0x8798a8,
        1,
    ),
    part(
        "BenchBase",
        Vec3::new(0.0, 1.0, -12.0),
        Vec3::new(7.0, 2.0, 2.0),
        0x775033,
        3,
    ),
    seat_part(
        "BenchSeat",
        Vec3::new(0.0, 2.25, -12.0),
        Vec3::new(8.0, 0.5, 2.5),
        0xb77a43,
        3,
    ),
];
