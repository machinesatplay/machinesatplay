use crate::{
    renderer_gltf_plugin, source_asset_root, RendererInput, RendererPlugin, RendererStatus,
};
use bevy::{
    app::{AppLabel, SubApps, TaskPoolPlugin},
    asset::{AssetMetaCheck, AssetPlugin, RenderAssetUsages, UntypedAssetLoadFailedEvent},
    camera::CameraPlugin,
    core_pipeline::CorePipelinePlugin,
    diagnostic::FrameCountPlugin,
    image::{Image, ImagePlugin},
    light::{EnvironmentMapLight, GeneratedEnvironmentMapLight, LightPlugin, Skybox},
    mesh::MeshPlugin,
    pbr::{PbrPlugin, PreparedMaterial, RenderMaterialInstances, RenderMeshInstances},
    prelude::*,
    render::{
        erased_render_asset::ErasedRenderAssets,
        error_handler::{
            ErrorType, RenderError as BevyRenderError, RenderErrorHandler, RenderErrorPolicy,
        },
        mesh::RenderMesh,
        render_asset::RenderAssets,
        render_resource::{
            CachedPipelineState, Extent3d, PipelineCache, PollType, TextureDimension,
            TextureFormat, TextureUsages,
        },
        renderer::RenderDevice,
        sync_world::MainEntity,
        texture::GpuImage,
        view::screenshot::{Screenshot, ScreenshotCaptured},
        RenderApp, RenderPlugin,
    },
    scene::ScenePlugin,
    state::app::StatesPlugin,
    time::{TimePlugin, TimeUpdateStrategy, Virtual},
    transform::TransformPlugin,
    window::{ExitCondition, WindowPlugin},
    world_serialization::WorldSerializationPlugin,
};
use render_api::{
    checked_rgba8_len, CapturedImage, RenderFrame, RenderRequest, ValidationErrors, ValidationIssue,
};
use std::{
    any::Any,
    error::Error,
    fmt,
    panic::{catch_unwind, AssertUnwindSafe},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
static CAPTURE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Native headless-render configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderOptions {
    /// Filesystem root containing the renderer's GLBs, textures, and cubemaps.
    pub asset_root: PathBuf,
    /// Total wall-clock budget for initialization, asset preparation, rendering, and readback.
    pub timeout: Duration,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            asset_root: source_asset_root(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// A failure produced while validating, preparing, rendering, or reading a capture.
#[derive(Debug)]
pub enum RenderError {
    InvalidInput(ValidationErrors),
    UnavailableGpu(String),
    AssetPreparation(String),
    Pipeline(String),
    Timeout {
        phase: &'static str,
        timeout: Duration,
    },
    Device(String),
    Readback(String),
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(error) => error.fmt(formatter),
            Self::UnavailableGpu(message) => write!(formatter, "GPU is unavailable: {message}"),
            Self::AssetPreparation(message) => {
                write!(formatter, "asset preparation failed: {message}")
            }
            Self::Pipeline(message) => write!(formatter, "render pipeline failed: {message}"),
            Self::Timeout { phase, timeout } => write!(
                formatter,
                "render timed out during {phase} after {:.3} seconds",
                timeout.as_secs_f64()
            ),
            Self::Device(message) => write!(formatter, "render device failed: {message}"),
            Self::Readback(message) => write!(formatter, "image readback failed: {message}"),
        }
    }
}

impl Error for RenderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidInput(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ValidationErrors> for RenderError {
    fn from(error: ValidationErrors) -> Self {
        Self::InvalidInput(error)
    }
}

/// Renders an ordered semantic frame sequence and captures its final frame.
///
/// This entrypoint is native-only. Request validation and player canonicalization happen before
/// Bevy starts GPU initialization.
pub fn render(
    request: RenderRequest,
    options: RenderOptions,
) -> Result<CapturedImage, RenderError> {
    let deadline = Deadline::new(options.timeout);
    deadline.remaining("total render")?;

    // Validation and adapter/device creation can both be synchronous. Run the complete operation
    // on a bounded worker so the public call honors one wall-clock budget. Only one capture may be
    // in flight; if a driver call stalls past its deadline, later calls cannot accumulate workers.
    let timeout = options.timeout;
    let worker_deadline = deadline;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("render-fn-capture".to_owned())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                render_request(request, options, worker_deadline)
            }))
            .unwrap_or_else(|panic| {
                Err(RenderError::Pipeline(format!(
                    "renderer worker panicked: {}",
                    panic_message(panic)
                )))
            });
            let _ = sender.send(result);
        })
        .map_err(|error| {
            RenderError::UnavailableGpu(format!("could not start renderer worker: {error}"))
        })?;

    let remaining = deadline.remaining("total render")?;
    match receiver.recv_timeout(remaining) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(RenderError::Timeout {
            phase: "total render",
            timeout,
        }),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(RenderError::Pipeline(
            "renderer worker stopped without producing a result".to_owned(),
        )),
    }
}

fn render_request(
    request: RenderRequest,
    options: RenderOptions,
    deadline: Deadline,
) -> Result<CapturedImage, RenderError> {
    let request = request.into_validated()?;
    deadline.remaining("request validation")?;
    let asset_root = validate_options(&options)?;
    deadline.remaining("asset-root validation")?;
    let deltas = semantic_deltas(&request.frames)?;
    deadline.remaining("frame timing validation")?;
    let _flight = CaptureFlight::acquire()?;
    render_validated(request, asset_root, deltas, deadline)
}

struct CaptureFlight;

impl CaptureFlight {
    fn acquire() -> Result<Self, RenderError> {
        CAPTURE_IN_FLIGHT
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self)
            .map_err(|_| {
                RenderError::Device(
                    "a previous render call is still active in this process".to_owned(),
                )
            })
    }
}

impl Drop for CaptureFlight {
    fn drop(&mut self) {
        CAPTURE_IN_FLIGHT.store(false, Ordering::Release);
    }
}

fn render_validated(
    request: RenderRequest,
    asset_root: String,
    deltas: Vec<Duration>,
    deadline: Deadline,
) -> Result<CapturedImage, RenderError> {
    deadline.remaining("initialization")?;

    let width = request.width;
    let height = request.height;
    let scene = request.scene;
    let frames = request.frames;
    let (mut apps, target) = initialize_renderer(scene, width, height, asset_root, &deadline)?;

    for (frame, delta) in frames.into_iter().zip(deltas) {
        let expected_players = frame
            .players
            .iter()
            .map(|player| player.id)
            .collect::<Vec<_>>();
        *apps.main.world_mut().resource_mut::<TimeUpdateStrategy>() =
            TimeUpdateStrategy::ManualDuration(delta);
        apps.main
            .world_mut()
            .resource_mut::<RendererInput>()
            .submit(frame);

        update_and_poll(&mut apps, &deadline, "semantic frame")?;

        // Loading and preparation may take any number of updates. Those updates are deliberately
        // zero-time and do not resubmit the semantic frame.
        *apps.main.world_mut().resource_mut::<TimeUpdateStrategy>() =
            TimeUpdateStrategy::ManualDuration(Duration::ZERO);
        wait_until_ready(&mut apps, &target, &expected_players, &deadline)?;
    }

    capture_target(&mut apps, target, width, height, &deadline)
}

fn semantic_deltas(frames: &[RenderFrame]) -> Result<Vec<Duration>, RenderError> {
    let mut previous_time = 0.0;
    frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            let seconds = if index == 0 {
                0.0
            } else {
                frame.time_seconds - previous_time
            };
            previous_time = frame.time_seconds;
            Duration::try_from_secs_f64(seconds).map_err(|_| {
                RenderError::InvalidInput(ValidationErrors {
                    issues: vec![ValidationIssue {
                        path: format!("frames[{index}].timeSeconds"),
                        message: "delta is too large for the native semantic clock".to_owned(),
                    }],
                })
            })
        })
        .collect()
}

fn validate_options(options: &RenderOptions) -> Result<String, RenderError> {
    if options.timeout.is_zero() {
        return Err(RenderError::Timeout {
            phase: "initialization",
            timeout: options.timeout,
        });
    }

    let metadata = std::fs::metadata(&options.asset_root).map_err(|error| {
        RenderError::AssetPreparation(format!(
            "could not access asset root {}: {error}",
            options.asset_root.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(RenderError::AssetPreparation(format!(
            "asset root is not a directory: {}",
            options.asset_root.display()
        )));
    }

    options
        .asset_root
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| {
            RenderError::AssetPreparation(format!(
                "asset root is not valid UTF-8: {}",
                options.asset_root.display()
            ))
        })
}

fn initialize_renderer(
    scene: render_api::RenderScene,
    width: u32,
    height: u32,
    asset_root: String,
    deadline: &Deadline,
) -> Result<(SubApps, Handle<Image>), RenderError> {
    deadline.remaining("initialization")?;

    let mut app = App::new();
    catch_unwind(AssertUnwindSafe(|| {
        // This is intentionally composed rather than derived from DefaultPlugins. In a workspace,
        // Cargo feature unification can otherwise add Winit back to a nominally headless crate.
        app.add_plugins(TaskPoolPlugin::default())
            .add_plugins(FrameCountPlugin)
            .add_plugins(TimePlugin)
            .add_plugins(TransformPlugin)
            .add_plugins(WindowPlugin {
                primary_window: None,
                exit_condition: ExitCondition::DontExit,
                ..default()
            })
            .add_plugins(AssetPlugin {
                file_path: asset_root,
                meta_check: AssetMetaCheck::Never,
                ..default()
            })
            .add_plugins(ScenePlugin)
            .add_plugins(WorldSerializationPlugin);
        configure_semantic_clock(app.world_mut());
    }))
    .map_err(|panic| {
        RenderError::Pipeline(format!("renderer setup panicked: {}", panic_message(panic)))
    })?;

    catch_unwind(AssertUnwindSafe(|| {
        app.add_plugins(RenderPlugin {
            synchronous_pipeline_compilation: true,
            ..default()
        });
    }))
    .map_err(|panic| {
        RenderError::UnavailableGpu(format!(
            "GPU initialization panicked: {}",
            panic_message(panic)
        ))
    })?;

    let target = catch_unwind(AssertUnwindSafe(|| {
        app.add_plugins(ImagePlugin::default())
            .add_plugins(MeshPlugin)
            .add_plugins(CameraPlugin)
            .add_plugins(LightPlugin)
            .add_plugins(CorePipelinePlugin)
            .add_plugins(renderer_gltf_plugin())
            .add_plugins(PbrPlugin::default())
            .add_plugins(StatesPlugin);

        app.init_resource::<CaptureAssetFailure>()
            .init_resource::<CaptureRenderFailure>()
            .insert_resource(RenderErrorHandler(capture_render_error))
            .add_systems(Update, record_asset_failures);

        let mut target_image = Image::new_uninit(
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::RENDER_WORLD,
        );
        target_image.texture_descriptor.usage |= TextureUsages::RENDER_ATTACHMENT;
        let target = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(target_image);

        app.add_plugins(
            RendererPlugin::new(scene)
                .with_target(bevy::camera::RenderTarget::Image(target.clone().into())),
        );
        target
    }))
    .map_err(|panic| {
        RenderError::Pipeline(format!(
            "renderer or generated pipeline setup panicked: {}",
            panic_message(panic)
        ))
    })?;

    deadline.remaining("initialization")?;
    let apps = catch_unwind(AssertUnwindSafe(|| {
        app.finish();
        app.cleanup();

        std::mem::take(app.sub_apps_mut())
    }))
    .map_err(|panic| {
        RenderError::UnavailableGpu(format!(
            "GPU initialization panicked: {}",
            panic_message(panic)
        ))
    })?;

    deadline.remaining("initialization")?;

    if apps.main.world().get_resource::<RenderDevice>().is_none() {
        return Err(RenderError::UnavailableGpu(
            "Bevy did not create a render device".to_owned(),
        ));
    }

    Ok((apps, target))
}

fn configure_semantic_clock(world: &mut World) {
    // Render requests define semantic time explicitly. The capture harness must not apply Bevy's
    // default 250 ms hitch-protection clamp to gaps between requested frames.
    world
        .resource_mut::<Time<Virtual>>()
        .set_max_delta(Duration::MAX);
    world.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO));
}

fn wait_until_ready(
    apps: &mut SubApps,
    target: &Handle<Image>,
    expected_players: &[u64],
    deadline: &Deadline,
) -> Result<(), RenderError> {
    loop {
        check_failures(apps)?;
        let renderer_ready = {
            let status = apps.main.world().resource::<RendererStatus>();
            status.ready
                && expected_players.iter().all(|id| {
                    status
                        .players
                        .get(id)
                        .is_some_and(|player| player.avatar_ready)
                })
        };
        if renderer_ready
            && gpu_assets_ready(apps, target)?
            && matches!(pipeline_readiness(apps)?, PipelineReadiness::Ready)
        {
            return Ok(());
        }

        deadline.remaining("asset and pipeline preparation")?;
        update_and_poll(apps, deadline, "asset and pipeline preparation")?;
    }
}

/// Confirms that every GPU-backed asset referenced by the active visual world has a prepared
/// render-world representation. CPU load completion alone is insufficient: Bevy can defer meshes
/// and materials while their texture dependencies are still being uploaded.
fn gpu_assets_ready(apps: &SubApps, target: &Handle<Image>) -> Result<bool, RenderError> {
    let mut mesh_entities = Vec::new();
    let mut images = std::collections::HashSet::from([target.id()]);

    for entity in apps.main.world().iter_entities() {
        if entity.get::<Mesh3d>().is_some()
            && entity
                .get::<ViewVisibility>()
                .is_some_and(|visibility| visibility.get())
        {
            mesh_entities.push(MainEntity::from(entity.id()));
        }
        if let Some(skybox) = entity.get::<Skybox>() {
            images.extend(skybox.image.as_ref().map(Handle::id));
        }
        if let Some(environment) = entity.get::<EnvironmentMapLight>() {
            images.insert(environment.diffuse_map.id());
            images.insert(environment.specular_map.id());
        }
        if let Some(environment) = entity.get::<GeneratedEnvironmentMapLight>() {
            images.insert(environment.environment_map.id());
            // Bevy adds these filtered output handles only after it has allocated the runtime
            // environment maps. Waiting for them prevents a source cubemap upload from being
            // mistaken for completed environment-light preparation.
            if entity.get::<EnvironmentMapLight>().is_none() {
                return Ok(false);
            }
        }
    }

    let render_app = apps
        .sub_apps
        .get(&RenderApp.intern())
        .ok_or_else(|| RenderError::UnavailableGpu("render sub-app is missing".to_owned()))?;
    let world = render_app.world();
    let render_meshes = world
        .get_resource::<RenderAssets<RenderMesh>>()
        .ok_or_else(|| RenderError::UnavailableGpu("render mesh assets are missing".to_owned()))?;
    let render_materials = world
        .get_resource::<ErasedRenderAssets<PreparedMaterial>>()
        .ok_or_else(|| {
            RenderError::UnavailableGpu("render material assets are missing".to_owned())
        })?;
    let render_images = world
        .get_resource::<RenderAssets<GpuImage>>()
        .ok_or_else(|| RenderError::UnavailableGpu("render image assets are missing".to_owned()))?;
    let mesh_instances = world.get_resource::<RenderMeshInstances>().ok_or_else(|| {
        RenderError::UnavailableGpu("render mesh instances are missing".to_owned())
    })?;
    let material_instances = world
        .get_resource::<RenderMaterialInstances>()
        .ok_or_else(|| {
            RenderError::UnavailableGpu("render material instances are missing".to_owned())
        })?;

    Ok(mesh_entities.iter().all(|entity| {
        mesh_instances
            .mesh_asset_id(*entity)
            .is_some_and(|id| render_meshes.get(id).is_some())
            && material_instances
                .instances
                .get(entity)
                .is_some_and(|instance| render_materials.get(instance.asset_id).is_some())
    }) && images.iter().all(|id| render_images.get(*id).is_some()))
}

fn capture_target(
    apps: &mut SubApps,
    target: Handle<Image>,
    width: u32,
    height: u32,
    deadline: &Deadline,
) -> Result<CapturedImage, RenderError> {
    let (sender, receiver) = mpsc::sync_channel(1);
    apps.main
        .world_mut()
        .spawn(Screenshot::image(target))
        .observe(move |captured: On<ScreenshotCaptured>| {
            let _ = sender.send(captured.image.clone());
        });

    *apps.main.world_mut().resource_mut::<TimeUpdateStrategy>() =
        TimeUpdateStrategy::ManualDuration(Duration::ZERO);

    loop {
        update_and_poll(apps, deadline, "image readback")?;
        match receive_capture(&receiver, width, height)? {
            Some(image) => return Ok(image),
            None => {
                deadline.remaining("image readback")?;
            }
        }
    }
}

fn receive_capture(
    receiver: &Receiver<Image>,
    width: u32,
    height: u32,
) -> Result<Option<CapturedImage>, RenderError> {
    let image = match receiver.try_recv() {
        Ok(image) => image,
        Err(TryRecvError::Empty) => return Ok(None),
        Err(TryRecvError::Disconnected) => {
            return Err(RenderError::Readback(
                "screenshot channel disconnected before producing an image".to_owned(),
            ));
        }
    };

    let actual_size = image.texture_descriptor.size;
    if actual_size.width != width
        || actual_size.height != height
        || actual_size.depth_or_array_layers != 1
    {
        return Err(RenderError::Readback(format!(
            "expected a {width}x{height} image, received {}x{} with {} layers",
            actual_size.width, actual_size.height, actual_size.depth_or_array_layers
        )));
    }

    let rgba8_srgb = image
        .try_into_dynamic()
        .map_err(|error| RenderError::Readback(format!("unsupported screenshot format: {error}")))?
        .to_rgba8()
        .into_raw();
    let expected = checked_rgba8_len(width, height)
        .ok_or_else(|| RenderError::Readback("RGBA8 output length overflowed usize".to_owned()))?;
    if rgba8_srgb.len() != expected {
        return Err(RenderError::Readback(format!(
            "expected {expected} RGBA8 bytes, received {}",
            rgba8_srgb.len()
        )));
    }

    Ok(Some(CapturedImage {
        width,
        height,
        rgba8_srgb,
    }))
}

fn update_and_poll(
    apps: &mut SubApps,
    deadline: &Deadline,
    phase: &'static str,
) -> Result<(), RenderError> {
    catch_unwind(AssertUnwindSafe(|| apps.update())).map_err(|panic| {
        RenderError::Pipeline(format!("Bevy update panicked: {}", panic_message(panic)))
    })?;

    let remaining = deadline.remaining(phase)?;
    let poll_result = apps
        .main
        .world()
        .resource::<RenderDevice>()
        .poll(PollType::Wait {
            submission_index: None,
            timeout: Some(remaining),
        });
    if let Err(error) = poll_result {
        // With no submission index, the only expected polling error is expiry of this bounded
        // wait. Preserve any asynchronously reported device error when one is available.
        check_failures(apps)?;
        return Err(if deadline.elapsed() >= deadline.timeout {
            deadline.timeout_error(phase)
        } else {
            RenderError::Device(error.to_string())
        });
    }

    check_failures(apps)
}

fn check_failures(apps: &SubApps) -> Result<(), RenderError> {
    let world = apps.main.world();

    if let Some(failure) = world.resource::<CaptureRenderFailure>().0.as_ref() {
        return Err(match failure.kind {
            CaptureRenderFailureKind::Pipeline => RenderError::Pipeline(failure.message.clone()),
            CaptureRenderFailureKind::Device => RenderError::Device(failure.message.clone()),
        });
    }
    if let Some(message) = world.resource::<CaptureAssetFailure>().0.as_ref() {
        return Err(RenderError::AssetPreparation(message.clone()));
    }
    if let Some(message) = world.resource::<RendererStatus>().error.as_ref() {
        return Err(RenderError::AssetPreparation(message.clone()));
    }

    // Pipeline compilation errors live only in the render world. Inspect them directly so a bad
    // generated shader cannot degrade into an otherwise unexplained readiness timeout.
    let _ = pipeline_readiness(apps)?;

    Ok(())
}

enum PipelineReadiness {
    Pending,
    Ready,
}

fn pipeline_readiness(apps: &SubApps) -> Result<PipelineReadiness, RenderError> {
    let render_app = apps
        .sub_apps
        .get(&RenderApp.intern())
        .ok_or_else(|| RenderError::UnavailableGpu("render sub-app is missing".to_owned()))?;
    let cache = render_app
        .world()
        .get_resource::<PipelineCache>()
        .ok_or_else(|| RenderError::UnavailableGpu("pipeline cache is missing".to_owned()))?;
    let waiting = cache
        .waiting_pipelines()
        .collect::<std::collections::HashSet<_>>();
    let mut pipeline_count = 0;
    let mut pending = !waiting.is_empty();

    for (id, pipeline) in cache.pipelines().enumerate() {
        pipeline_count += 1;
        match &pipeline.state {
            CachedPipelineState::Ok(_) => {}
            CachedPipelineState::Queued | CachedPipelineState::Creating(_) => pending = true,
            CachedPipelineState::Err(_) if waiting.contains(&id) => pending = true,
            CachedPipelineState::Err(error) => {
                return Err(RenderError::Pipeline(error.to_string()));
            }
        }
    }

    Ok(if pipeline_count == 0 || pending {
        PipelineReadiness::Pending
    } else {
        PipelineReadiness::Ready
    })
}

#[derive(Resource, Default)]
struct CaptureAssetFailure(Option<String>);

fn record_asset_failures(
    mut failures: MessageReader<UntypedAssetLoadFailedEvent>,
    mut capture_failure: ResMut<CaptureAssetFailure>,
) {
    if capture_failure.0.is_some() {
        return;
    }

    if let Some(failure) = failures.read().next() {
        capture_failure.0 = Some(format!("{}: {}", failure.path, failure.error));
    }
}

#[derive(Clone, Copy)]
enum CaptureRenderFailureKind {
    Pipeline,
    Device,
}

struct CaptureRenderFailureDetails {
    kind: CaptureRenderFailureKind,
    message: String,
}

#[derive(Resource, Default)]
struct CaptureRenderFailure(Option<CaptureRenderFailureDetails>);

fn capture_render_error(
    error: &BevyRenderError,
    main_world: &mut World,
    _render_world: &mut World,
) -> RenderErrorPolicy {
    let kind = match error.ty {
        ErrorType::Validation => CaptureRenderFailureKind::Pipeline,
        ErrorType::Internal | ErrorType::OutOfMemory | ErrorType::DeviceLost => {
            CaptureRenderFailureKind::Device
        }
    };
    let message = if error.description.is_empty() {
        format!("{:?}", error.ty)
    } else {
        error.description.clone()
    };
    main_world.resource_mut::<CaptureRenderFailure>().0 =
        Some(CaptureRenderFailureDetails { kind, message });
    RenderErrorPolicy::StopRendering
}

#[derive(Clone, Copy)]
struct Deadline {
    started: Instant,
    timeout: Duration,
}

impl Deadline {
    fn new(timeout: Duration) -> Self {
        Self {
            started: Instant::now(),
            timeout,
        }
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn remaining(&self, phase: &'static str) -> Result<Duration, RenderError> {
        self.timeout
            .checked_sub(self.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| self.timeout_error(phase))
    }

    fn timeout_error(&self, phase: &'static str) -> RenderError {
        RenderError::Timeout {
            phase,
            timeout: self.timeout,
        }
    }
}

fn panic_message(panic: Box<dyn Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = panic.downcast_ref::<&'static str>() {
        (*message).to_owned()
    } else {
        "unknown panic".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use render_api::{RenderCamera, RenderScene, MAX_RENDER_WIDTH};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn default_timeout_is_sixty_seconds() {
        assert_eq!(RenderOptions::default().timeout, Duration::from_secs(60));
    }

    #[test]
    fn invalid_request_fails_before_asset_or_gpu_setup() {
        let mut request = valid_request();
        request.width = 0;
        let options = RenderOptions {
            asset_root: missing_path(),
            ..RenderOptions::default()
        };

        assert!(matches!(
            render(request, options),
            Err(RenderError::InvalidInput(_))
        ));
    }

    #[test]
    fn unsupported_dimensions_fail_before_asset_or_gpu_setup() {
        let mut request = valid_request();
        request.width = MAX_RENDER_WIDTH + 1;
        let options = RenderOptions {
            asset_root: missing_path(),
            ..RenderOptions::default()
        };

        assert!(matches!(
            render(request, options),
            Err(RenderError::InvalidInput(_))
        ));
    }

    #[test]
    fn missing_asset_root_is_an_asset_preparation_error() {
        let options = RenderOptions {
            asset_root: missing_path(),
            ..RenderOptions::default()
        };

        assert!(matches!(
            render(valid_request(), options),
            Err(RenderError::AssetPreparation(_))
        ));
    }

    #[test]
    fn file_asset_root_is_an_asset_preparation_error() {
        let options = RenderOptions {
            asset_root: std::env::current_exe().expect("test executable path"),
            ..RenderOptions::default()
        };

        assert!(matches!(
            render(valid_request(), options),
            Err(RenderError::AssetPreparation(_))
        ));
    }

    #[test]
    fn zero_timeout_expires_before_asset_or_gpu_setup() {
        let options = RenderOptions {
            asset_root: missing_path(),
            timeout: Duration::ZERO,
        };

        assert!(matches!(
            render(valid_request(), options),
            Err(RenderError::Timeout { .. })
        ));
    }

    #[test]
    fn semantic_clock_does_not_clamp_sparse_frames_or_advance_during_loading() {
        let mut app = App::new();
        app.add_plugins(TimePlugin);
        configure_semantic_clock(app.world_mut());

        // Prime Time<Real> the same way the required time-zero first frame does in a capture.
        app.update();

        let sparse_delta = Duration::from_secs(1);
        *app.world_mut().resource_mut::<TimeUpdateStrategy>() =
            TimeUpdateStrategy::ManualDuration(sparse_delta);
        app.update();

        let semantic_time = app.world().resource::<Time<Virtual>>();
        assert_eq!(semantic_time.delta(), sparse_delta);
        assert_eq!(semantic_time.elapsed(), sparse_delta);
        let shader_time = app.world().resource::<Time>();
        assert_eq!(shader_time.delta(), sparse_delta);
        assert_eq!(shader_time.elapsed(), sparse_delta);

        *app.world_mut().resource_mut::<TimeUpdateStrategy>() =
            TimeUpdateStrategy::ManualDuration(Duration::ZERO);
        app.update();

        let loading_time = app.world().resource::<Time<Virtual>>();
        assert_eq!(loading_time.delta(), Duration::ZERO);
        assert_eq!(loading_time.elapsed(), sparse_delta);
        let loading_shader_time = app.world().resource::<Time>();
        assert_eq!(loading_shader_time.delta(), Duration::ZERO);
        assert_eq!(loading_shader_time.elapsed(), sparse_delta);
    }

    #[test]
    fn readback_accepts_tightly_packed_rgba8_srgb() {
        let pixels = vec![12, 34, 56, 255];
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.send(rgba_image(1, 1, pixels.clone())).unwrap();

        let captured = receive_capture(&receiver, 1, 1)
            .expect("valid readback")
            .expect("available image");
        assert_eq!(captured.width, 1);
        assert_eq!(captured.height, 1);
        assert_eq!(captured.rgba8_srgb, pixels);
    }

    #[test]
    fn readback_rejects_an_unexpected_byte_length() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut image = rgba_image(2, 1, vec![0; 8]);
        image.data = Some(vec![12, 34, 56, 255]);
        sender.send(image).unwrap();

        assert!(matches!(
            receive_capture(&receiver, 2, 1),
            Err(RenderError::Readback(_))
        ));
    }

    fn valid_request() -> RenderRequest {
        let scene = RenderScene::from_world_json(
            r##"{
                "schemaVersion": 2,
                "worldScale": 0.3,
                "killPlane": -500.0,
                "spawnPoints": [[0.0, 0.0]],
                "parts": [{
                    "name": "floor",
                    "position": [0.0, 0.0, 0.0],
                    "size": [10.0, 1.0, 10.0],
                    "color": "#a3a3a3",
                    "material": "plastic"
                }]
            }"##,
        )
        .expect("valid test world");

        RenderRequest {
            scene,
            frames: vec![RenderFrame {
                time_seconds: 0.0,
                players: Vec::new(),
                camera: RenderCamera {
                    focus: [0.0, 1.0, 0.0],
                    yaw: 0.0,
                    pitch: -0.2,
                    radius: 8.0,
                },
                sky_blend: 0.0,
            }],
            width: 1,
            height: 1,
        }
    }

    fn rgba_image(width: u32, height: u32, data: Vec<u8>) -> Image {
        Image::new(
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            data,
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::MAIN_WORLD,
        )
    }

    fn missing_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("render-fn-missing-{}-{nonce}", std::process::id()))
    }
}
