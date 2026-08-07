//! mach game engine: 3D server-authoritative multiplayer with Bevy and Lightyear.
//!
//! Native and browser clients connect to the authoritative game server.

#[cfg(feature = "client")]
use game_client::{client, render};
use game_core::shared;

use bevy::prelude::*;
#[cfg(feature = "client")]
use bevy::window::WindowCloseRequested;
#[cfg(target_family = "wasm")]
use core::net::IpAddr;
use core::net::{Ipv4Addr, SocketAddr};
use core::time::Duration;
#[cfg(feature = "client")]
use lightyear::connection::client::{ClientState, Disconnected};
#[cfg(feature = "client")]
use lightyear::netcode::client_plugin::NetcodeConfig as ClientNetcodeConfig;
#[cfg(feature = "client")]
use lightyear::netcode::{ConnectToken, NetcodeClient};
#[cfg(feature = "client")]
use lightyear::prelude::client::*;
use lightyear::prelude::*;

#[cfg(not(target_family = "wasm"))]
use game_core::shared::SERVER_PORT;
use game_core::shared::{FIXED_TIMESTEP_HZ, PRIVATE_KEY, PROTOCOL_ID};

/// Three seconds is shorter than a cold renderer startup under contention.
/// Keep the session alive through shader/asset initialization and ordinary
/// development-machine stalls; true failures are recovered by reconnecting.
const NETCODE_TIMEOUT_SECS: i32 = 30;
#[cfg(feature = "client")]
const RECONNECT_INITIAL_SECONDS: f32 = 0.5;
#[cfg(feature = "client")]
const RECONNECT_MAX_SECONDS: f32 = 5.0;

#[derive(Clone)]
struct RuntimeConfig {
    server_addr: SocketAddr,
    #[cfg(feature = "client")]
    certificate_digest: String,
}

#[cfg(feature = "client")]
#[derive(Component)]
struct ReconnectState {
    timer: Timer,
    attempts: u32,
    retry_enabled: bool,
    connected_reported: bool,
}

#[cfg(all(feature = "client", not(target_family = "wasm")))]
#[derive(Resource)]
struct DevJoinReporter {
    address: String,
    token: String,
    client_id: u64,
    reported: bool,
}

#[cfg(feature = "client")]
impl Default for ReconnectState {
    fn default() -> Self {
        Self {
            timer: Timer::from_seconds(RECONNECT_INITIAL_SECONDS, TimerMode::Once),
            attempts: 0,
            retry_enabled: true,
            connected_reported: false,
        }
    }
}
#[derive(Clone, Copy)]
enum Mode {
    #[cfg(feature = "client")]
    Client { client_id: u64 },
}

#[cfg(all(target_family = "wasm", feature = "client"))]
pub fn start_game(
    certificate_digest: &str,
    server_host: &str,
    server_port: u16,
) -> Result<(), wasm_bindgen::JsValue> {
    let server_host = server_host.parse::<IpAddr>().map_err(|error| {
        wasm_bindgen::JsValue::from_str(&format!("invalid game server address: {error}"))
    })?;
    // every browser tab gets a random identity
    run_app(
        Mode::Client {
            client_id: rand::random::<u64>(),
        },
        RuntimeConfig {
            server_addr: SocketAddr::new(server_host, server_port),
            certificate_digest: certificate_digest.to_owned(),
        },
    );
    Ok(())
}

#[cfg(not(target_family = "wasm"))]
pub fn run_native() {
    use clap::{Parser, Subcommand};

    #[derive(Parser)]
    #[command(about = "mach game engine")]
    struct Cli {
        #[command(subcommand)]
        mode: CliMode,
    }

    #[derive(Subcommand, Clone)]
    enum CliMode {
        /// Client connecting to a running server
        #[cfg(feature = "client")]
        Client {
            #[arg(short, long, default_value_t = 1)]
            client_id: u64,
        },
    }

    let mode = match Cli::parse().mode {
        #[cfg(feature = "client")]
        CliMode::Client { client_id } => Mode::Client { client_id },
    };
    let runtime = native_runtime_config().unwrap_or_else(|error| {
        eprintln!("mach: {error}");
        std::process::exit(1);
    });
    run_app(mode, runtime);
}

fn run_app(mode: Mode, runtime: RuntimeConfig) {
    let tick_duration = Duration::from_secs_f64(1.0 / FIXED_TIMESTEP_HZ);

    match mode {
        #[cfg(feature = "client")]
        Mode::Client { client_id } => {
            let mut app = gui_app(format!("game - client {client_id}"));
            app.add_plugins(ClientPlugins { tick_duration });
            app.add_plugins(shared::SharedPlugin);
            app.add_plugins(client::GameClientPlugin);
            app.add_plugins(render::GameRenderPlugin);
            spawn_client(&mut app, client_id, &runtime);
            #[cfg(not(target_family = "wasm"))]
            if let (Ok(address), Ok(token)) = (
                std::env::var("MACH_DEV_CONTROL_ADDRESS"),
                std::env::var("MACH_DEV_CONTROL_TOKEN"),
            ) {
                if !address.is_empty() && !token.is_empty() {
                    app.insert_resource(DevJoinReporter {
                        address,
                        token,
                        client_id,
                        reported: false,
                    });
                    app.add_systems(Update, report_dev_world_joined);
                }
            }
            app.add_systems(Startup, connect_client);
            app.add_systems(
                Update,
                (disconnect_on_window_close, maintain_client_connection).chain(),
            );
            app.add_observer(log_client_disconnected);
            app.run();
        }
    }
}

#[cfg(feature = "client")]
fn gui_app(title: String) -> App {
    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(WindowPlugin {
                primary_window: Some(Window {
                    title,
                    resolution: (1024, 768).into(),
                    // capture keys like arrows/tab in the browser instead of scrolling
                    prevent_default_event_handling: true,
                    ..default()
                }),
                ..default()
            })
            .set(AssetPlugin {
                file_path: game_asset_path(),
                // avoid 404s for .meta files when fetching assets over HTTP (wasm)
                meta_check: bevy::asset::AssetMetaCheck::Never,
                ..default()
            })
            .set(render_fn::renderer_gltf_plugin()),
    );
    app
}

#[cfg(target_family = "wasm")]
fn game_asset_path() -> String {
    "assets".to_owned()
}

#[cfg(not(target_family = "wasm"))]
fn game_asset_path() -> String {
    std::env::var_os("GAME_PROJECT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().expect("current directory is available"))
        .join("assets")
        .to_string_lossy()
        .into_owned()
}

#[cfg(not(target_family = "wasm"))]
fn certificate_dir() -> std::path::PathBuf {
    std::env::var_os("GAME_CERT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .expect("current directory is available")
                .join(".mach/certificates")
        })
}

#[cfg(not(target_family = "wasm"))]
fn native_runtime_config() -> Result<RuntimeConfig, String> {
    let port = std::env::var("GAME_SERVER_PORT")
        .ok()
        .map(|value| {
            value
                .parse::<u16>()
                .map_err(|error| format!("invalid GAME_SERVER_PORT `{value}`: {error}"))
        })
        .transpose()?
        .unwrap_or(SERVER_PORT);
    let certificate_dir = certificate_dir();
    #[cfg(feature = "client")]
    let certificate_digest = {
        let digest_path = certificate_dir.join("digest.txt");
        std::fs::read_to_string(&digest_path)
            .map_err(|error| format!("cannot read {}: {error}", digest_path.display()))?
    };
    Ok(RuntimeConfig {
        server_addr: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
        #[cfg(feature = "client")]
        certificate_digest: certificate_digest.trim().to_owned(),
    })
}

/// Clients connect over WebTransport (QUIC/UDP) on native and wasm alike.
#[cfg(feature = "client")]
fn spawn_client(app: &mut App, client_id: u64, runtime: &RuntimeConfig) {
    let server_addr = runtime.server_addr;
    let session_id = rand::random::<u64>();
    let token = ConnectToken::build(server_addr, PROTOCOL_ID, session_id, PRIVATE_KEY)
        .timeout_seconds(NETCODE_TIMEOUT_SECS)
        .expire_seconds(-1)
        .user_data(shared::encode_player_identity(client_id))
        .generate()
        .expect("failed to generate local connection token");
    let auth = Authentication::Token(token);
    let netcode_config = ClientNetcodeConfig {
        client_timeout_secs: NETCODE_TIMEOUT_SECS,
        token_expire_secs: -1,
        ..default()
    };
    app.world_mut().spawn((
        Client::default(),
        Link::new(None),
        LocalAddr(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)),
        PeerAddr(server_addr),
        PredictionManager::default(),
        NetcodeClient::new(auth, netcode_config).expect("failed to create netcode client"),
        WebTransportClientIo {
            certificate_digest: runtime.certificate_digest.clone(),
        },
        ReconnectState::default(),
        Name::from("Client"),
    ));
    info!("Prepared logical player {client_id} with unique session {session_id}");
}

#[cfg(feature = "client")]
fn connect_client(mut commands: Commands, client: Single<Entity, With<Client>>) {
    commands.trigger(Connect {
        entity: client.into_inner(),
    });
}

#[cfg(feature = "client")]
fn reconnect_delay(attempt: u32) -> f32 {
    (RECONNECT_INITIAL_SECONDS * 2.0_f32.powi(attempt.min(10) as i32)).min(RECONNECT_MAX_SECONDS)
}

/// Keep connection failure from becoming a permanent loading screen. This is
/// deliberately state-driven: it never starts a second handshake while one is
/// already in flight, and a successful connection resets the backoff.
#[cfg(feature = "client")]
fn maintain_client_connection(
    time: Res<Time<Real>>,
    mut clients: Query<(Entity, &Client, &mut ReconnectState)>,
    mut commands: Commands,
) {
    for (entity, client, mut reconnect) in &mut clients {
        if !reconnect.retry_enabled {
            continue;
        }
        match client.state {
            ClientState::Connected => {
                if !reconnect.connected_reported {
                    set_browser_connection_status("connected");
                    reconnect.connected_reported = true;
                }
                reconnect.attempts = 0;
                reconnect
                    .timer
                    .set_duration(Duration::from_secs_f32(RECONNECT_INITIAL_SECONDS));
                reconnect.timer.reset();
            }
            ClientState::Connecting | ClientState::Disconnecting => {
                if reconnect.connected_reported {
                    set_browser_connection_status("connecting");
                    reconnect.connected_reported = false;
                }
                // The active operation owns the connection. Start measuring a
                // retry delay only after it transitions to Disconnected.
                reconnect.timer.reset();
            }
            ClientState::Disconnected => {
                if reconnect.connected_reported {
                    set_browser_connection_status("disconnected");
                    reconnect.connected_reported = false;
                }
                reconnect.timer.tick(time.delta());
                if reconnect.timer.just_finished() {
                    reconnect.attempts = reconnect.attempts.saturating_add(1);
                    let delay = reconnect_delay(reconnect.attempts);
                    warn!(
                        "Connection retry {} (next retry in {:.1}s)",
                        reconnect.attempts, delay
                    );
                    commands.trigger(Connect { entity });
                    reconnect.timer.set_duration(Duration::from_secs_f32(delay));
                    reconnect.timer.reset();
                }
            }
        }
    }
}

#[cfg(all(feature = "client", target_family = "wasm"))]
#[wasm_bindgen::prelude::wasm_bindgen(
    inline_js = "export function set_game_connection_status(status) { document.documentElement.dataset.gameConnection = status; }"
)]
extern "C" {
    fn set_game_connection_status(status: &str);
}

#[cfg(all(feature = "client", target_family = "wasm"))]
fn set_browser_connection_status(status: &str) {
    set_game_connection_status(status);
}

#[cfg(all(feature = "client", not(target_family = "wasm")))]
fn set_browser_connection_status(_status: &str) {}

#[cfg(all(feature = "client", not(target_family = "wasm")))]
fn report_dev_world_joined(
    reporter: Option<ResMut<DevJoinReporter>>,
    controlled_player: Query<(), (With<game_core::protocol::Player>, With<Controlled>)>,
) {
    let Some(mut reporter) = reporter else {
        return;
    };
    if reporter.reported || controlled_player.is_empty() {
        return;
    }
    reporter.reported = true;
    let address = reporter.address.clone();
    let token = reporter.token.clone();
    let client_id = reporter.client_id;
    std::thread::spawn(move || {
        use std::io::Write;
        use std::net::TcpStream;

        let Ok(mut stream) = TcpStream::connect(address) else {
            return;
        };
        let timeout = Some(Duration::from_secs(1));
        let _ = stream.set_write_timeout(timeout);
        let message = format!(
            "{{\"command\":\"client_joined\",\"client_id\":{client_id},\"token\":\"{token}\"}}\n"
        );
        let _ = stream.write_all(message.as_bytes());
        let _ = stream.flush();
    });
}

#[cfg(feature = "client")]
fn disconnect_on_window_close(
    mut close_requests: MessageReader<WindowCloseRequested>,
    mut clients: Query<(Entity, &mut ReconnectState), With<Client>>,
    mut commands: Commands,
) {
    if close_requests.read().next().is_none() {
        return;
    }
    for (entity, mut reconnect) in &mut clients {
        reconnect.retry_enabled = false;
        commands.trigger(Disconnect { entity });
    }
    info!("Local game window closed; disconnected its player");
}

#[cfg(feature = "client")]
fn log_client_disconnected(trigger: On<Add, Disconnected>, disconnected: Query<&Disconnected>) {
    if let Ok(disconnected) = disconnected.get(trigger.entity) {
        warn!(
            "Client connection lost: {}",
            disconnected.reason.as_deref().unwrap_or("unknown reason")
        );
    }
}
