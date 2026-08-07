use bevy::{
    asset::{AssetMetaCheck, AssetPlugin, AssetServer, Assets, RecursiveDependencyLoadState},
    gltf::{convert_coordinates::GltfConvertCoordinates, GltfAssetLabel, GltfPlugin},
    image::ImagePlugin,
    mesh::{Mesh, MeshPlugin, VertexAttributeValues},
    prelude::*,
    scene::ScenePlugin,
    transform::TransformPlugin,
    world_serialization::WorldSerializationPlugin,
};
use render_fn::{renderer_gltf_plugin, RendererGltfExtensionsPlugin};

const FIXTURE: &str = "secondary_skin_weights_with_morph_target.gltf";

fn test_app(gltf_plugin: GltfPlugin) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        TransformPlugin,
        AssetPlugin {
            file_path: format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR")),
            meta_check: AssetMetaCheck::Never,
            ..default()
        },
        ScenePlugin,
        WorldSerializationPlugin,
        ImagePlugin::default(),
        MeshPlugin,
        gltf_plugin,
        RendererGltfExtensionsPlugin,
    ));
    app.finish();
    app.cleanup();
    app
}

fn load_primitive(app: &mut App, fixture: &str, mesh: usize) -> Handle<Mesh> {
    let primitive = app.world().resource::<AssetServer>().load::<Mesh>(
        GltfAssetLabel::Primitive { mesh, primitive: 0 }.from_asset(fixture.to_owned()),
    );
    for _ in 0..10_000 {
        app.update();
        match app
            .world()
            .resource::<AssetServer>()
            .recursive_dependency_load_state(&primitive)
        {
            RecursiveDependencyLoadState::Loaded => return primitive,
            RecursiveDependencyLoadState::Failed(error) => {
                panic!("fixture failed to load: {error}")
            }
            _ => {}
        }
    }
    panic!("fixture did not finish loading");
}

#[test]
fn secondary_skin_weights_preserve_morph_targets_and_global_coordinate_conversion() {
    let mut gltf_plugin = renderer_gltf_plugin();
    gltf_plugin.convert_coordinates = GltfConvertCoordinates {
        rotate_meshes: true,
        ..default()
    };
    let mut app = test_app(gltf_plugin);
    let primitive = load_primitive(&mut app, FIXTURE, 0);

    let meshes = app.world().resource::<Assets<Mesh>>();
    let mesh = meshes
        .get(&primitive)
        .expect("fixture primitive was not loaded");

    let VertexAttributeValues::Uint16x4(joint_indices) = mesh
        .attribute(Mesh::ATTRIBUTE_JOINT_INDEX)
        .expect("joint indices should be imported")
    else {
        panic!("joint indices were not converted to Uint16x4");
    };
    assert_eq!(joint_indices[0], [0, 1, 2, 4]);

    let VertexAttributeValues::Float32x3(positions) = mesh
        .attribute(Mesh::ATTRIBUTE_POSITION)
        .expect("positions should be imported")
    else {
        panic!("positions were not converted to Float32x3");
    };
    assert_eq!(
        positions,
        &[[0.0, 0.0, 0.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]
    );

    let morph_targets = mesh
        .morph_targets()
        .expect("morph target should be imported");
    assert_eq!(morph_targets.len(), 3);
    assert!(morph_targets
        .iter()
        .all(|target| target.position == Vec3::new(0.0, 0.0, -0.25)));
    assert_eq!(mesh.morph_target_names().unwrap(), ["Smile"]);
}
