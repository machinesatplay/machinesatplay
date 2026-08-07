use bevy::{
    asset::{LoadContext, RenderAssetUsages},
    gltf::{
        convert_coordinates::GltfConvertCoordinates,
        extensions::{ErasedGltfExtensionHandler, GltfExtensionHandler, GltfExtensionHandlers},
        vertex_attributes::convert_attribute,
        GltfLoaderSettings, GltfPlugin, MorphTargetNames, PrimitiveMorphAttributesIter,
    },
    log::warn,
    mesh::{Indices, Mesh, MeshVertexAttribute, PrimitiveTopology, VertexAttributeValues},
    prelude::{App, Plugin},
    tasks::ConditionalSendFuture,
};
use gltf::{mesh::util::ReadIndices, Semantic};

/// Reduces extra glTF skin influences to Bevy's strongest normalized four.
///
/// Add this after [`bevy::gltf::GltfPlugin`]. [`crate::RendererPlugin`] does
/// this automatically for live and capture renderers.
pub struct RendererGltfExtensionsPlugin;

impl Plugin for RendererGltfExtensionsPlugin {
    fn build(&self, app: &mut App) {
        let default_convert_coordinates = app
            .get_added_plugins::<GltfPlugin>()
            .last()
            .map(|plugin| plugin.convert_coordinates)
            .unwrap_or_default();
        let handler = SecondarySkinWeights {
            default_convert_coordinates,
            ..Default::default()
        };

        #[cfg(target_family = "wasm")]
        bevy::tasks::block_on(async {
            app.world_mut()
                .resource_mut::<GltfExtensionHandlers>()
                .0
                .write()
                .await
                .push(Box::new(handler));
        });

        #[cfg(not(target_family = "wasm"))]
        app.world_mut()
            .resource_mut::<GltfExtensionHandlers>()
            .0
            .write_blocking()
            .push(Box::new(handler));
    }
}

#[derive(Clone, Default)]
struct SecondarySkinWeights {
    load_meshes: RenderAssetUsages,
    default_convert_coordinates: GltfConvertCoordinates,
    convert_coordinates: GltfConvertCoordinates,
    copied_buffer_data: Option<Vec<Vec<u8>>>,
}

impl GltfExtensionHandler for SecondarySkinWeights {
    fn dyn_clone(&self) -> Box<dyn ErasedGltfExtensionHandler> {
        Box::new(self.clone())
    }

    fn on_root(
        &mut self,
        _load_context: &mut LoadContext<'_>,
        _gltf: &gltf::Gltf,
        settings: &GltfLoaderSettings,
    ) {
        self.load_meshes = settings.load_meshes;
        self.convert_coordinates = settings
            .convert_coordinates
            .unwrap_or(self.default_convert_coordinates);
    }

    fn on_gltf_primitive(
        &mut self,
        _load_context: &mut LoadContext<'_>,
        _gltf_document: &gltf::Gltf,
        gltf_mesh: &gltf::Mesh,
        primitive: &gltf::Primitive,
        buffer_data: &[Vec<u8>],
        custom_vertex_attributes: &bevy::platform::collections::HashMap<
            Box<str>,
            MeshVertexAttribute,
        >,
        gltf_mesh_on_skinned_nodes: bool,
        _gltf_mesh_on_non_skinned_nodes: bool,
        user_mesh: &mut Option<Mesh>,
    ) -> impl ConditionalSendFuture<Output = ()> {
        async move {
            if !gltf_mesh_on_skinned_nodes {
                return;
            }
            let Some(CombinedSkinInfluences {
                joints: joint_indices,
                weights: joint_weights,
            }) = combined_skin_influences(primitive, buffer_data)
            else {
                return;
            };

            let Some(topology) = primitive_topology(primitive.mode()) else {
                return;
            };
            let mut mesh = Mesh::new(topology, self.load_meshes);

            let copied_buffer_data = self
                .copied_buffer_data
                .get_or_insert_with(|| buffer_data.to_vec());
            for (semantic, accessor) in primitive.attributes() {
                if matches!(semantic, Semantic::Joints(_) | Semantic::Weights(_)) {
                    continue;
                }
                match convert_attribute(
                    semantic,
                    accessor,
                    copied_buffer_data,
                    custom_vertex_attributes,
                    self.convert_coordinates.rotate_meshes,
                ) {
                    Ok((attribute, values)) => mesh.insert_attribute(attribute, values),
                    Err(error) => warn!("{error}"),
                }
            }

            mesh.insert_attribute(
                Mesh::ATTRIBUTE_JOINT_INDEX,
                VertexAttributeValues::Uint16x4(joint_indices),
            );
            mesh.insert_attribute(
                Mesh::ATTRIBUTE_JOINT_WEIGHT,
                VertexAttributeValues::Float32x4(joint_weights),
            );

            let reader =
                primitive.reader(|buffer| buffer_data.get(buffer.index()).map(Vec::as_slice));
            if let Some(indices) = reader.read_indices() {
                mesh.insert_indices(match indices {
                    ReadIndices::U8(values) => {
                        Indices::U16(values.map(|value| value as u16).collect())
                    }
                    ReadIndices::U16(values) => Indices::U16(values.collect()),
                    ReadIndices::U32(values) => Indices::U32(values.collect()),
                });
            }

            let morph_target_reader = reader.read_morph_targets();
            if morph_target_reader.len() != 0 {
                mesh.set_morph_targets(
                    morph_target_reader
                        .flat_map(|target| PrimitiveMorphAttributesIter {
                            convert_coordinates: self.convert_coordinates.rotate_meshes,
                            positions: target.0,
                            normals: target.1,
                            tangents: target.2,
                        })
                        .collect(),
                );

                if let Some(names) = gltf_mesh
                    .extras()
                    .as_ref()
                    .and_then(|extras| serde_json::from_str::<MorphTargetNames>(extras.get()).ok())
                {
                    mesh.set_morph_target_names(names.target_names);
                }
            }

            *user_mesh = Some(mesh);
        }
    }
}

struct CombinedSkinInfluences {
    joints: Vec<[u16; 4]>,
    weights: Vec<[f32; 4]>,
}

fn combined_skin_influences(
    primitive: &gltf::Primitive,
    buffer_data: &[Vec<u8>],
) -> Option<CombinedSkinInfluences> {
    let mut highest_joint_set = None;
    let mut highest_weight_set = None;
    for (semantic, _) in primitive.attributes() {
        match semantic {
            Semantic::Joints(set) => {
                highest_joint_set = Some(highest_joint_set.unwrap_or(set).max(set));
            }
            Semantic::Weights(set) => {
                highest_weight_set = Some(highest_weight_set.unwrap_or(set).max(set));
            }
            _ => {}
        }
    }
    let highest_set = highest_joint_set?;
    if highest_set == 0 || highest_weight_set != Some(highest_set) {
        return None;
    }

    let reader = primitive.reader(|buffer| buffer_data.get(buffer.index()).map(Vec::as_slice));
    let mut joint_sets = Vec::new();
    let mut weight_sets = Vec::new();
    for set in 0..=highest_set {
        joint_sets.push(reader.read_joints(set)?.into_u16().collect::<Vec<_>>());
        weight_sets.push(reader.read_weights(set)?.into_f32().collect::<Vec<_>>());
    }

    let vertex_count = joint_sets.first()?.len();
    if joint_sets.iter().any(|set| set.len() != vertex_count)
        || weight_sets.iter().any(|set| set.len() != vertex_count)
    {
        return None;
    }

    let mut result_joints = Vec::with_capacity(vertex_count);
    let mut result_weights = Vec::with_capacity(vertex_count);
    for vertex in 0..vertex_count {
        let mut influences = Vec::<(u16, f32)>::with_capacity(joint_sets.len() * 4);
        for (joints, weights) in joint_sets.iter().zip(&weight_sets) {
            for (&joint, &weight) in joints[vertex].iter().zip(&weights[vertex]) {
                if weight <= 0.0 || !weight.is_finite() {
                    continue;
                }
                if let Some((_, total)) = influences
                    .iter_mut()
                    .find(|(existing_joint, _)| *existing_joint == joint)
                {
                    *total += weight;
                } else {
                    influences.push((joint, weight));
                }
            }
        }
        influences.sort_by(|left, right| right.1.total_cmp(&left.1));
        influences.truncate(4);

        let retained_total = influences.iter().map(|(_, weight)| weight).sum::<f32>();
        let mut joints = [0; 4];
        let mut weights = [0.0; 4];
        if retained_total > 0.0 {
            for (slot, (joint, weight)) in influences.into_iter().enumerate() {
                joints[slot] = joint;
                weights[slot] = weight / retained_total;
            }
        }
        result_joints.push(joints);
        result_weights.push(weights);
    }

    Some(CombinedSkinInfluences {
        joints: result_joints,
        weights: result_weights,
    })
}

fn primitive_topology(mode: gltf::mesh::Mode) -> Option<PrimitiveTopology> {
    match mode {
        gltf::mesh::Mode::Points => Some(PrimitiveTopology::PointList),
        gltf::mesh::Mode::Lines => Some(PrimitiveTopology::LineList),
        gltf::mesh::Mode::LineStrip => Some(PrimitiveTopology::LineStrip),
        gltf::mesh::Mode::Triangles => Some(PrimitiveTopology::TriangleList),
        gltf::mesh::Mode::TriangleStrip => Some(PrimitiveTopology::TriangleStrip),
        gltf::mesh::Mode::LineLoop | gltf::mesh::Mode::TriangleFan => None,
    }
}
