use serde::de::{Error as DeserializeError, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fmt};

pub const PROJECT_SCHEMA_VERSION: u32 = 1;
pub const WORLD_SCHEMA_VERSION: u32 = 2;
pub const CANONICAL_MATERIALS: &[&str] = &[
    "plastic",
    "grass",
    "sand",
    "stone",
    "wood",
    "planks",
    "metal",
    "brick",
    "concrete",
    "slate",
    "ice",
    "fabric",
    "gravel",
    "cobblestone",
    "marble",
    "granite",
    "treadplate",
    "water",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GameManifest {
    #[serde(rename = "$schema", default)]
    pub schema: Option<String>,
    #[serde(default = "project_schema_version")]
    pub schema_version: u32,
    pub name: String,
    pub engine_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GameWorld {
    #[serde(rename = "$schema", default)]
    pub schema: Option<String>,
    #[serde(default = "world_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_world_scale")]
    pub world_scale: f32,
    #[serde(default = "default_kill_plane")]
    pub kill_plane: f32,
    #[serde(
        default = "default_spawn_points",
        deserialize_with = "deserialize_spawn_points"
    )]
    pub spawn_points: Vec<[f32; 2]>,
    pub parts: Vec<GamePart>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GamePart {
    pub name: String,
    pub position: [f32; 3],
    pub size: [f32; 3],
    #[serde(default = "default_color")]
    pub color: String,
    #[serde(default = "default_material")]
    pub material: String,
    #[serde(default = "default_alpha")]
    pub alpha: u8,
    #[serde(default = "default_true")]
    pub collidable: bool,
    #[serde(default)]
    pub swimmable: bool,
    #[serde(default)]
    pub climbable: bool,
    #[serde(default)]
    pub seat: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ValidationIssue {
    pub path: String,
    pub message: String,
}

impl fmt::Display for ValidationIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path, self.message)
    }
}

impl GameManifest {
    pub fn validate(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        if self.schema_version != PROJECT_SCHEMA_VERSION {
            issue(
                &mut issues,
                "schemaVersion",
                format!(
                    "unsupported project schema {}; expected {}",
                    self.schema_version, PROJECT_SCHEMA_VERSION
                ),
            );
        }
        if self.name.trim().is_empty() {
            issue(&mut issues, "name", "must not be empty");
        }
        if self
            .name
            .chars()
            .next()
            .is_some_and(|character| !character.is_ascii_alphanumeric())
        {
            issue(&mut issues, "name", "must start with a letter or number");
        }
        if !self.name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        }) {
            issue(
                &mut issues,
                "name",
                "use only letters, numbers, hyphens, and underscores",
            );
        }
        if self.engine_version.trim().is_empty()
            || !self.engine_version.chars().all(|character| {
                character.is_ascii_alphanumeric()
                    || character == '.'
                    || character == '-'
                    || character == '_'
            })
        {
            issue(
                &mut issues,
                "engineVersion",
                "use only letters, numbers, dots, hyphens, and underscores",
            );
        }
        issues
    }
}

impl GameWorld {
    pub fn validate(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        if self.schema_version != WORLD_SCHEMA_VERSION {
            issue(
                &mut issues,
                "schemaVersion",
                format!(
                    "unsupported world schema {}; expected {}",
                    self.schema_version, WORLD_SCHEMA_VERSION
                ),
            );
        }
        if !self.world_scale.is_finite() || self.world_scale <= 0.0 {
            issue(&mut issues, "worldScale", "must be a positive number");
        }
        if !self.kill_plane.is_finite() {
            issue(&mut issues, "killPlane", "must be a finite number");
        }
        if self.spawn_points.is_empty() {
            issue(
                &mut issues,
                "spawnPoints",
                "must contain at least one spawn",
            );
        }
        for (index, spawn) in self.spawn_points.iter().enumerate() {
            if !spawn.iter().all(|value| value.is_finite()) {
                issue(
                    &mut issues,
                    format!("spawnPoints[{index}]"),
                    "coordinates must be finite numbers",
                );
            }
        }
        if self.parts.is_empty() {
            issue(&mut issues, "parts", "must contain at least one part");
        }
        let mut names = HashSet::new();
        for (index, part) in self.parts.iter().enumerate() {
            let base = format!("parts[{index}]");
            if part.name.trim().is_empty() {
                issue(&mut issues, format!("{base}.name"), "must not be empty");
            } else if !names.insert(part.name.to_ascii_lowercase()) {
                issue(
                    &mut issues,
                    format!("{base}.name"),
                    format!("duplicate part name `{}`", part.name),
                );
            }
            if !part.position.iter().all(|value| value.is_finite()) {
                issue(
                    &mut issues,
                    format!("{base}.position"),
                    "coordinates must be finite numbers",
                );
            }
            if !part
                .size
                .iter()
                .all(|value| value.is_finite() && *value > 0.0)
            {
                issue(
                    &mut issues,
                    format!("{base}.size"),
                    "every dimension must be a positive number",
                );
            }
            if parse_hex_color(&part.color).is_none() {
                issue(
                    &mut issues,
                    format!("{base}.color"),
                    "must be a six-digit hex color such as #67a84b",
                );
            }
            if !CANONICAL_MATERIALS.contains(&part.material.as_str()) {
                issue(
                    &mut issues,
                    format!("{base}.material"),
                    format!(
                        "unknown material `{}`; use {}",
                        part.material,
                        CANONICAL_MATERIALS.join(", ")
                    ),
                );
            }
            if part.swimmable && part.collidable {
                issue(
                    &mut issues,
                    base,
                    "swimmable water must set collidable to false",
                );
            }
        }
        issues
    }
}

fn deserialize_spawn_points<'de, D>(deserializer: D) -> Result<Vec<[f32; 2]>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let points = Vec::<SpawnPointValues>::deserialize(deserializer)?;
    points
        .into_iter()
        .enumerate()
        .map(|(index, SpawnPointValues(point))| {
            point.try_into().map_err(|point: Vec<f32>| {
                D::Error::custom(format!(
                    "spawnPoints[{index}] must contain exactly 2 numbers, found {}",
                    point.len()
                ))
            })
        })
        .collect()
}

struct SpawnPointValues(Vec<f32>);

impl<'de> Deserialize<'de> for SpawnPointValues {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(SpawnPointVisitor)
    }
}

struct SpawnPointVisitor;

impl<'de> Visitor<'de> for SpawnPointVisitor {
    type Value = SpawnPointValues;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of exactly 2 numbers")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(2));
        while let Some(value) = sequence.next_element()? {
            values.push(value);
        }
        Ok(SpawnPointValues(values))
    }
}

pub fn parse_hex_color(value: &str) -> Option<u32> {
    let digits = value.strip_prefix('#')?;
    (digits.len() == 6)
        .then(|| u32::from_str_radix(digits, 16).ok())
        .flatten()
}

pub fn material_id(value: &str) -> Option<u8> {
    match value.to_ascii_lowercase().as_str() {
        "water" => Some(0),
        "plastic" => Some(1),
        "brick" => Some(2),
        "wood" => Some(3),
        "planks" => Some(4),
        "marble" => Some(5),
        "stone" | "slate" => Some(6),
        "concrete" => Some(7),
        "granite" => Some(8),
        "cobblestone" => Some(9),
        "gravel" => Some(10),
        "treadplate" | "tread-plate" => Some(11),
        "metal" => Some(12),
        "fabric" => Some(13),
        "grass" => Some(14),
        "sand" => Some(15),
        "ice" => Some(16),
        _ => None,
    }
}

fn issue(issues: &mut Vec<ValidationIssue>, path: impl Into<String>, message: impl Into<String>) {
    issues.push(ValidationIssue {
        path: path.into(),
        message: message.into(),
    });
}

const fn project_schema_version() -> u32 {
    PROJECT_SCHEMA_VERSION
}

const fn world_schema_version() -> u32 {
    WORLD_SCHEMA_VERSION
}

const fn default_world_scale() -> f32 {
    0.3
}

const fn default_kill_plane() -> f32 {
    -500.0
}

fn default_spawn_points() -> Vec<[f32; 2]> {
    vec![[-3.0, 0.0], [-1.0, 0.0], [1.0, 0.0], [3.0, 0.0]]
}

fn default_color() -> String {
    "#a3a3a3".to_owned()
}

fn default_material() -> String {
    "plastic".to_owned()
}

const fn default_alpha() -> u8 {
    255
}

const fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_error_lists_every_canonical_material() {
        let world = GameWorld {
            schema: None,
            schema_version: WORLD_SCHEMA_VERSION,
            world_scale: 0.3,
            kill_plane: -10.0,
            spawn_points: vec![[0.0, 0.0]],
            parts: vec![GamePart {
                name: "test".to_owned(),
                position: [0.0, 0.0, 0.0],
                size: [1.0, 1.0, 1.0],
                color: "#ffffff".to_owned(),
                material: "glass".to_owned(),
                alpha: 255,
                collidable: true,
                swimmable: false,
                climbable: false,
                seat: false,
            }],
        };
        let message = world.validate().remove(0).message;
        for material in CANONICAL_MATERIALS {
            assert!(message.contains(material), "message omitted {material}");
        }
    }

    #[test]
    fn validation_rejects_noncanonical_material_spelling() {
        let mut world = GameWorld {
            schema: None,
            schema_version: WORLD_SCHEMA_VERSION,
            world_scale: 0.3,
            kill_plane: -10.0,
            spawn_points: vec![[0.0, 0.0]],
            parts: vec![GamePart {
                name: "test".to_owned(),
                position: [0.0, 0.0, 0.0],
                size: [1.0, 1.0, 1.0],
                color: "#ffffff".to_owned(),
                material: "Concrete".to_owned(),
                alpha: 255,
                collidable: true,
                swimmable: false,
                climbable: false,
                seat: false,
            }],
        };

        let issues = world.validate();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, "parts[0].material");

        world.parts[0].material = "concrete".to_owned();
        assert!(world.validate().is_empty());
    }

    #[test]
    fn color_parser_requires_the_schema_hash_prefix() {
        assert_eq!(parse_hex_color("#67a84b"), Some(0x67a84b));
        assert_eq!(parse_hex_color("67a84b"), None);
    }

    #[test]
    fn three_coordinate_spawn_has_a_clear_error() {
        let error = serde_json::from_str::<GameWorld>(
            r#"{
                "parts": [{"name":"floor","position":[0,0,0],"size":[1,1,1]}],
                "spawnPoints": [[0, 10, 0]]
            }"#,
        )
        .expect_err("three-coordinate spawn should fail");
        assert!(
            error
                .to_string()
                .contains("spawnPoints[0] must contain exactly 2 numbers, found 3"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn object_spawn_explains_the_required_array_shape() {
        let error = serde_json::from_str::<GameWorld>(
            r#"{
                "parts": [{"name":"floor","position":[0,0,0],"size":[1,1,1]}],
                "spawnPoints": [{"x":0,"z":10}]
            }"#,
        )
        .expect_err("object spawn should fail");
        assert!(
            error
                .to_string()
                .contains("invalid type: map, expected an array of exactly 2 numbers"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn project_name_must_start_with_a_letter_or_number() {
        let manifest = GameManifest {
            schema: None,
            schema_version: PROJECT_SCHEMA_VERSION,
            name: "-bad".to_owned(),
            engine_version: "0.1.4".to_owned(),
        };

        assert_eq!(
            manifest.validate(),
            vec![ValidationIssue {
                path: "name".to_owned(),
                message: "must start with a letter or number".to_owned(),
            }]
        );
    }
}
