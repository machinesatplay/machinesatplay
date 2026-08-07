//! capture types shared by mach rendering clients.
//!
//! this capture contract is separate from creator gameplay. creators write a
//! full Bevy browser client and native authoritative server.

use std::{collections::HashSet, error::Error, fmt};

use game_format::ValidationIssue as WorldValidationIssue;
pub use game_format::{GamePart, GameWorld};
use serde::{Deserialize, Serialize};

/// Largest supported output width.
///
/// This matches wgpu's downlevel WebGL2 default for 2D textures, keeping a
/// validated request within the baseline shared by WebGL2, WebGPU, and native
/// capture backends.
pub const MAX_RENDER_WIDTH: u32 = 2_048;

/// Largest supported output height. See [`MAX_RENDER_WIDTH`].
pub const MAX_RENDER_HEIGHT: u32 = 2_048;

/// Largest supported number of pixels in one captured image.
pub const MAX_RENDER_PIXELS: u64 = (MAX_RENDER_WIDTH as u64) * (MAX_RENDER_HEIGHT as u64);

/// Largest supported tightly packed RGBA8 output allocation, in bytes.
pub const MAX_RENDER_RGBA8_BYTES: u64 = MAX_RENDER_PIXELS * 4;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenderRequest {
    pub scene: RenderScene,
    pub frames: Vec<RenderFrame>,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenderScene {
    pub world: GameWorld,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenderFrame {
    pub time_seconds: f64,
    /// The complete set of visible players for this frame.
    ///
    /// Consumers must remove visual state for IDs omitted from a later frame.
    pub players: Vec<RenderPlayer>,
    pub camera: RenderCamera,
    pub sky_blend: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenderPlayer {
    pub id: u64,
    pub position: [f32; 3],
    pub velocity: [f32; 3],
    pub color_rgb: [u8; 3],
    pub alive: bool,
    pub grounded: bool,
    pub swimming: bool,
    pub climbing: bool,
    pub seated: bool,
    pub facing: [f32; 3],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenderCamera {
    pub focus: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub radius: f32,
}

/// A tightly packed, top-left-origin RGBA8 sRGB image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapturedImage {
    pub width: u32,
    pub height: u32,
    pub rgba8_srgb: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ValidationIssue {
    pub path: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ValidationErrors {
    pub issues: Vec<ValidationIssue>,
}

#[derive(Debug)]
pub enum WorldJsonError {
    Parse(serde_json::Error),
    Validation(ValidationErrors),
}

impl RenderRequest {
    /// Checks the complete request without changing player ordering.
    pub fn validate(&self) -> Result<(), ValidationErrors> {
        let mut issues = Vec::new();

        append_world_issues(&mut issues, "scene.world", &self.scene.world.validate());

        if self.width == 0 {
            issue(&mut issues, "width", "must be greater than zero");
        }
        if self.height == 0 {
            issue(&mut issues, "height", "must be greater than zero");
        }
        if self.width > MAX_RENDER_WIDTH {
            issue(
                &mut issues,
                "width",
                format!("must not exceed {MAX_RENDER_WIDTH} pixels"),
            );
        }
        if self.height > MAX_RENDER_HEIGHT {
            issue(
                &mut issues,
                "height",
                format!("must not exceed {MAX_RENDER_HEIGHT} pixels"),
            );
        }
        if self.width != 0
            && self.height != 0
            && checked_rgba8_len(self.width, self.height).is_none()
        {
            issue(
                &mut issues,
                "dimensions",
                "RGBA8 byte length overflows the addressable buffer size",
            );
        }
        if self.width != 0
            && self.height != 0
            && u64::from(self.width) * u64::from(self.height) > MAX_RENDER_PIXELS
        {
            issue(
                &mut issues,
                "dimensions",
                format!(
                    "must not exceed {MAX_RENDER_PIXELS} pixels ({MAX_RENDER_RGBA8_BYTES} RGBA8 bytes)"
                ),
            );
        }

        if self.frames.is_empty() {
            issue(&mut issues, "frames", "must contain at least one frame");
        }

        let mut previous_time = None;
        for (frame_index, frame) in self.frames.iter().enumerate() {
            let frame_path = format!("frames[{frame_index}]");
            let time_path = format!("{frame_path}.timeSeconds");

            if frame_index == 0 {
                if !frame.time_seconds.is_finite() || frame.time_seconds != 0.0 {
                    issue(&mut issues, time_path, "must be exactly 0");
                }
            } else if !frame.time_seconds.is_finite() {
                issue(&mut issues, time_path, "must be a finite number");
            } else if previous_time.is_some_and(|previous| frame.time_seconds <= previous) {
                issue(
                    &mut issues,
                    time_path,
                    "must be strictly greater than the previous frame time",
                );
            }
            if frame.time_seconds.is_finite() {
                previous_time = Some(frame.time_seconds);
            }

            validate_frame(frame, frame_index, &mut issues);
        }

        finish_validation(issues)
    }

    /// Sorts every authoritative player set by ID.
    pub fn normalize(&mut self) {
        for frame in &mut self.frames {
            frame.players.sort_by_key(|player| player.id);
        }
    }

    /// Validates first, then canonicalizes player ordering in place.
    pub fn validate_and_normalize(&mut self) -> Result<(), ValidationErrors> {
        self.validate()?;
        self.normalize();
        Ok(())
    }

    /// Returns a validated request with canonical player ordering.
    pub fn into_validated(mut self) -> Result<Self, ValidationErrors> {
        self.validate_and_normalize()?;
        Ok(self)
    }
}

impl RenderScene {
    pub fn validate(&self) -> Result<(), ValidationErrors> {
        let mut issues = Vec::new();
        append_world_issues(&mut issues, "world", &self.world.validate());
        finish_validation(issues)
    }

    pub fn from_world_json(json: &str) -> Result<Self, WorldJsonError> {
        let scene = Self {
            world: serde_json::from_str(json).map_err(WorldJsonError::Parse)?,
        };
        scene.validate().map_err(WorldJsonError::Validation)?;
        Ok(scene)
    }
}

/// Returns the required byte length for tightly packed RGBA8 output.
pub fn checked_rgba8_len(width: u32, height: u32) -> Option<usize> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))?
        .checked_mul(4)?;
    usize::try_from(bytes).ok()
}

impl PartialEq for RenderScene {
    fn eq(&self, other: &Self) -> bool {
        worlds_equal(&self.world, &other.world)
    }
}

impl fmt::Display for ValidationIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path, self.message)
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "render request validation failed")?;
        for issue in &self.issues {
            write!(formatter, "; {issue}")?;
        }
        Ok(())
    }
}

impl Error for ValidationErrors {}

impl fmt::Display for WorldJsonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => write!(formatter, "could not parse world JSON: {error}"),
            Self::Validation(error) => error.fmt(formatter),
        }
    }
}

impl Error for WorldJsonError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::Validation(error) => Some(error),
        }
    }
}

fn validate_frame(frame: &RenderFrame, frame_index: usize, issues: &mut Vec<ValidationIssue>) {
    let frame_path = format!("frames[{frame_index}]");

    validate_finite_vector(
        &frame.camera.focus,
        format!("{frame_path}.camera.focus"),
        issues,
    );
    validate_finite(frame.camera.yaw, format!("{frame_path}.camera.yaw"), issues);
    validate_finite(
        frame.camera.pitch,
        format!("{frame_path}.camera.pitch"),
        issues,
    );
    if !frame.camera.radius.is_finite() || frame.camera.radius <= 0.0 {
        issue(
            issues,
            format!("{frame_path}.camera.radius"),
            "must be a positive finite number",
        );
    }
    if !frame.sky_blend.is_finite() || !(0.0..=1.0).contains(&frame.sky_blend) {
        issue(
            issues,
            format!("{frame_path}.skyBlend"),
            "must be a finite number from 0 through 1",
        );
    }

    let mut player_ids = HashSet::with_capacity(frame.players.len());
    for (player_index, player) in frame.players.iter().enumerate() {
        let player_path = format!("{frame_path}.players[{player_index}]");
        if !player_ids.insert(player.id) {
            issue(
                issues,
                format!("{player_path}.id"),
                format!("duplicate player ID {}", player.id),
            );
        }
        validate_finite_vector(&player.position, format!("{player_path}.position"), issues);
        validate_finite_vector(&player.velocity, format!("{player_path}.velocity"), issues);
        let path = format!("{player_path}.facing");
        if !player.facing.iter().all(|value| value.is_finite()) {
            issue(issues, path, "coordinates must be finite numbers");
        } else if player.facing.iter().all(|value| *value == 0.0) {
            issue(issues, path, "must not be the zero vector");
        }
    }
}

fn validate_finite_vector<const N: usize>(
    values: &[f32; N],
    path: String,
    issues: &mut Vec<ValidationIssue>,
) {
    if !values.iter().all(|value| value.is_finite()) {
        issue(issues, path, "coordinates must be finite numbers");
    }
}

fn validate_finite(value: f32, path: String, issues: &mut Vec<ValidationIssue>) {
    if !value.is_finite() {
        issue(issues, path, "must be a finite number");
    }
}

fn append_world_issues(
    issues: &mut Vec<ValidationIssue>,
    prefix: &str,
    world_issues: &[WorldValidationIssue],
) {
    issues.extend(world_issues.iter().map(|world_issue| ValidationIssue {
        path: format!("{prefix}.{}", world_issue.path),
        message: world_issue.message.clone(),
    }));
}

fn finish_validation(issues: Vec<ValidationIssue>) -> Result<(), ValidationErrors> {
    if issues.is_empty() {
        Ok(())
    } else {
        Err(ValidationErrors { issues })
    }
}

fn issue(issues: &mut Vec<ValidationIssue>, path: impl Into<String>, message: impl Into<String>) {
    issues.push(ValidationIssue {
        path: path.into(),
        message: message.into(),
    });
}

fn worlds_equal(left: &GameWorld, right: &GameWorld) -> bool {
    left.schema == right.schema
        && left.schema_version == right.schema_version
        && left.world_scale == right.world_scale
        && left.kill_plane == right.kill_plane
        && left.spawn_points == right.spawn_points
        && left.parts.len() == right.parts.len()
        && left
            .parts
            .iter()
            .zip(&right.parts)
            .all(|(left, right)| parts_equal(left, right))
}

fn parts_equal(left: &GamePart, right: &GamePart) -> bool {
    left.name == right.name
        && left.position == right.position
        && left.size == right.size
        && left.color == right.color
        && left.material == right.material
        && left.alpha == right.alpha
        && left.collidable == right.collidable
        && left.swimmable == right.swimmable
        && left.climbable == right.climbable
        && left.seat == right.seat
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_format::WORLD_SCHEMA_VERSION;

    fn valid_world() -> GameWorld {
        GameWorld {
            schema: None,
            schema_version: WORLD_SCHEMA_VERSION,
            world_scale: 0.3,
            kill_plane: -100.0,
            spawn_points: vec![[0.0, 0.0]],
            parts: vec![GamePart {
                name: "ground".to_owned(),
                position: [0.0, -0.5, 0.0],
                size: [10.0, 1.0, 10.0],
                color: "#67a84b".to_owned(),
                material: "grass".to_owned(),
                alpha: 255,
                collidable: true,
                swimmable: false,
                climbable: false,
                seat: false,
            }],
        }
    }

    fn player(id: u64) -> RenderPlayer {
        RenderPlayer {
            id,
            position: [id as f32, 1.0, 0.0],
            velocity: [0.0; 3],
            color_rgb: [100, 150, 200],
            alive: true,
            grounded: true,
            swimming: false,
            climbing: false,
            seated: false,
            facing: [0.0, 0.0, 1.0],
        }
    }

    fn frame(time_seconds: f64, ids: &[u64]) -> RenderFrame {
        RenderFrame {
            time_seconds,
            players: ids.iter().copied().map(player).collect(),
            camera: RenderCamera {
                focus: [0.0, 1.0, 0.0],
                yaw: 0.0,
                pitch: -0.2,
                radius: 8.0,
            },
            sky_blend: 0.5,
        }
    }

    fn request(frames: Vec<RenderFrame>) -> RenderRequest {
        RenderRequest {
            scene: RenderScene {
                world: valid_world(),
            },
            frames,
            width: 320,
            height: 180,
        }
    }

    #[test]
    fn validates_and_canonicalizes_player_order() {
        let request = request(vec![frame(0.0, &[9, 2, 5])])
            .into_validated()
            .expect("request should be valid");

        let ids: Vec<_> = request.frames[0]
            .players
            .iter()
            .map(|player| player.id)
            .collect();
        assert_eq!(ids, [2, 5, 9]);
    }

    #[test]
    fn accepts_authoritative_player_joins_and_removals() {
        let request = request(vec![
            frame(0.0, &[3, 1]),
            frame(1.0, &[4, 3]),
            frame(2.0, &[]),
            frame(3.0, &[1]),
        ])
        .into_validated()
        .expect("IDs may join, disappear, and reappear between frames");

        let ids: Vec<Vec<u64>> = request
            .frames
            .iter()
            .map(|frame| frame.players.iter().map(|player| player.id).collect())
            .collect();
        assert_eq!(ids, [vec![1, 3], vec![3, 4], vec![], vec![1]]);
    }

    #[test]
    fn reports_structured_paths_for_invalid_input() {
        let mut request = request(vec![frame(1.0, &[7, 7]), frame(1.0, &[])]);
        request.width = 0;
        request.scene.world.parts[0].size[1] = 0.0;
        request.frames[0].players[0].position[2] = f32::NAN;
        request.frames[0].players[1].facing = [0.0; 3];
        request.frames[0].camera.radius = -1.0;
        request.frames[0].sky_blend = 1.1;

        let errors = request.validate().expect_err("request should be invalid");
        let paths: Vec<_> = errors
            .issues
            .iter()
            .map(|issue| issue.path.as_str())
            .collect();
        assert!(paths.contains(&"scene.world.parts[0].size"));
        assert!(paths.contains(&"width"));
        assert!(paths.contains(&"frames[0].timeSeconds"));
        assert!(paths.contains(&"frames[0].players[0].position"));
        assert!(paths.contains(&"frames[0].players[1].id"));
        assert!(paths.contains(&"frames[0].players[1].facing"));
        assert!(paths.contains(&"frames[0].camera.radius"));
        assert!(paths.contains(&"frames[0].skyBlend"));
        assert!(paths.contains(&"frames[1].timeSeconds"));
    }

    #[test]
    fn rejects_empty_frames_and_unaddressable_dimensions() {
        let empty = request(vec![]).validate().expect_err("frames are required");
        assert_eq!(empty.issues[0].path, "frames");

        let mut oversized = request(vec![frame(0.0, &[])]);
        oversized.width = u32::MAX;
        oversized.height = u32::MAX;
        let errors = oversized
            .validate()
            .expect_err("RGBA byte length must be checked");
        assert!(errors.issues.iter().any(|issue| issue.path == "dimensions"));
    }

    #[test]
    fn accepts_maximum_render_dimensions_and_allocation() {
        let mut request = request(vec![frame(0.0, &[])]);
        request.width = MAX_RENDER_WIDTH;
        request.height = MAX_RENDER_HEIGHT;

        request
            .validate()
            .expect("the documented dimension boundary should be valid");
        assert_eq!(
            checked_rgba8_len(request.width, request.height),
            Some(MAX_RENDER_RGBA8_BYTES as usize)
        );
    }

    #[test]
    fn rejects_dimensions_above_backend_and_allocation_limits() {
        let mut too_wide = request(vec![frame(0.0, &[])]);
        too_wide.width = MAX_RENDER_WIDTH + 1;
        let errors = too_wide
            .validate()
            .expect_err("width above the shared backend limit should fail");
        assert!(errors.issues.iter().any(|issue| issue.path == "width"));

        let mut too_tall = request(vec![frame(0.0, &[])]);
        too_tall.height = MAX_RENDER_HEIGHT + 1;
        let errors = too_tall
            .validate()
            .expect_err("height above the shared backend limit should fail");
        assert!(errors.issues.iter().any(|issue| issue.path == "height"));

        let mut too_many_pixels = request(vec![frame(0.0, &[])]);
        too_many_pixels.width = MAX_RENDER_WIDTH;
        too_many_pixels.height = MAX_RENDER_HEIGHT + 1;
        let errors = too_many_pixels
            .validate()
            .expect_err("RGBA8 allocation above the documented limit should fail");
        assert!(errors.issues.iter().any(|issue| issue.path == "dimensions"));
    }

    #[test]
    fn parses_and_validates_world_json() {
        let json = serde_json::to_string(&valid_world()).expect("serialize fixture");
        let scene = RenderScene::from_world_json(&json).expect("valid world JSON");
        assert_eq!(scene.world.schema_version, WORLD_SCHEMA_VERSION);

        let invalid_json = json.replace("\"worldScale\":0.3", "\"worldScale\":0.0");
        let error = RenderScene::from_world_json(&invalid_json).expect_err("invalid scale");
        let WorldJsonError::Validation(errors) = error else {
            panic!("expected validation error")
        };
        assert_eq!(errors.issues[0].path, "world.worldScale");
    }

    #[test]
    fn accepts_finite_nonzero_facing_values() {
        let mut request = request(vec![frame(0.0, &[1])]);
        request.frames[0].players[0].facing = [0.0, 0.0, -1.0];
        request.validate().expect("facing values should be valid");
    }
}
