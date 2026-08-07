mod game;

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;
use clap::{Parser, Subcommand};
use core::net::{Ipv4Addr, SocketAddr};
use core::time::Duration;
use game_core::shared;
use game_core::shared::{FIXED_TIMESTEP_HZ, PRIVATE_KEY, PROTOCOL_ID, SERVER_PORT};
use lightyear::prelude::*;

const NETCODE_TIMEOUT_SECS: i32 = 30;

#[derive(Parser)]
#[command(about = "mach game server")]
struct Cli {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Clone, Subcommand)]
enum Mode {
    /// Run the dedicated server.
    Server,
    /// Generate a local self-signed certificate.
    GenCerts,
}

fn main() {
    match Cli::parse().mode {
        Mode::Server => run_server(),
        Mode::GenCerts => gen_certs(),
    }
}

fn run_server() {
    let tick_duration = Duration::from_secs_f64(1.0 / FIXED_TIMESTEP_HZ);
    let mut app = headless_app(tick_duration);
    app.add_plugins(lightyear::prelude::server::ServerPlugins { tick_duration });
    app.add_plugins(shared::SharedPlugin);
    app.add_plugins(game::GameServerPlugin);
    spawn_server(&mut app);
    app.add_systems(Startup, start_server);
    app.run();
}

fn headless_app(tick_duration: Duration) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(tick_duration)),
        TransformPlugin,
        bevy::input::InputPlugin,
        bevy::log::LogPlugin::default(),
        bevy::state::app::StatesPlugin,
        bevy::diagnostic::DiagnosticsPlugin,
    ));
    app
}

fn spawn_server(app: &mut App) -> Entity {
    use async_compat::Compat;
    use bevy::tasks::IoTaskPool;
    use lightyear::netcode::NetcodeServer;
    use lightyear::prelude::server::*;

    let certificate_dir = certificate_dir();
    let cert = certificate_dir.join("cert.pem");
    let key = certificate_dir.join("key.pem");
    let identity = IoTaskPool::get()
        .scope(|scope| {
            scope.spawn(Compat::new(async move {
                Identity::load_pemfiles(&cert, &key)
                    .await
                    .expect("failed to load WebTransport certificates")
            }));
        })
        .pop()
        .unwrap();

    app.world_mut()
        .spawn((
            Name::from("Server"),
            NetcodeServer::new(NetcodeConfig {
                client_timeout_secs: NETCODE_TIMEOUT_SECS,
                protocol_id: PROTOCOL_ID,
                private_key: PRIVATE_KEY,
                ..Default::default()
            }),
            LocalAddr(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), server_port())),
            WebTransportServerIo {
                certificate: identity,
            },
        ))
        .id()
}

fn start_server(mut commands: Commands, server: Single<Entity, With<lightyear::prelude::Server>>) {
    commands.trigger(lightyear::prelude::server::Start {
        entity: server.into_inner(),
    });
}

fn server_port() -> u16 {
    std::env::var("GAME_SERVER_PORT")
        .ok()
        .map(|value| {
            value
                .parse::<u16>()
                .unwrap_or_else(|error| panic!("invalid GAME_SERVER_PORT `{value}`: {error}"))
        })
        .unwrap_or(SERVER_PORT)
}

fn gen_certs() {
    use lightyear::webtransport::prelude::Identity;

    let dir = certificate_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let hosts = std::env::var("GAME_CERT_HOSTS")
        .unwrap_or_else(|_| "localhost,127.0.0.1,::1".to_owned())
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let identity = Identity::self_signed(hosts).unwrap();
    let cert = &identity.certificate_chain().as_slice()[0];
    let digest = cert.hash().to_string().replace(':', "");
    std::fs::write(dir.join("cert.pem"), cert.to_pem()).unwrap();
    std::fs::write(dir.join("key.pem"), identity.private_key().to_secret_pem()).unwrap();
    std::fs::write(dir.join("digest.txt"), &digest).unwrap();
    println!("wrote new certificate to {}", dir.display());
    println!("digest: {digest}");
}

fn certificate_dir() -> std::path::PathBuf {
    std::env::var_os("GAME_CERT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .expect("current directory is available")
                .join(".mach/certificates")
        })
}
