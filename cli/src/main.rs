//! Local development CLI for machines at play projects.

#[cfg(target_family = "wasm")]
compile_error!("the mach CLI is only available on native targets");

mod browser_bindgen;
mod build_seed;
mod cache;
mod deploy;
mod dev;
mod managed_tools;
mod project;
mod self_update;
mod starter_bundle;
mod telemetry;

use build_seed::{
    cargo_target_dir, configure_cargo_home, prepare_build_seed, prepare_deploy_build_seed,
    prepare_project_cache,
};
use cache::{cache_command, lock_cache_shared, mach_cache_root, CacheCommand};
use clap::{Parser, Subcommand};
use deploy::{
    deploy_command, deployments_command, login_command, logout_command, rollback_command,
    whoami_command,
};
use dev::{
    build_job_budget, checked, dev, dev_open, ensure_local_build_tools, ensure_native_build_tools,
    ensure_valid_project, host_server_target, is_source_engine, local_creator_build, open_browser,
    platform_id, validate_engine_root, BrowserBuild, DEPLOY_SERVER_TARGET,
};
#[cfg(test)]
use dev::{
    dev_build_plan, ensure_server_port_available, lock_server_port_at, DevBuildPlan,
    GameServerChild,
};
use fs2::FileExt;
use notify::{RecursiveMode, Watcher};
#[cfg(test)]
use project::validate_source_starter_root;
use project::{
    activate_validated_file, create_project, doctor_command, load_project, project_issues,
    validate_command, ProjectFiles,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

const DEFAULT_SERVER_PORT: u16 = 5888;
const CERT_MAX_AGE: Duration = Duration::from_secs(13 * 24 * 60 * 60);
const WASM_BINDGEN_VERSION: &str = "0.2.126";
const ENGINE_VERSION: &str = "0.1.24";
const RELEASES_URL: &str = "https://machinesatplay.com/releases";
const DEFAULT_API_URL: &str = "https://machinesatplay.com";
const CLI_AUTH_CLIENT_ID: &str = "machinesatplay-cli";
const GIB: u64 = 1024 * 1024 * 1024;
const STARTER_GAME_SCHEMA: &str = include_str!("../../game.schema.json");
const EMBEDDED_SOURCE_STARTER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/starter.zip"));
const STARTER_README: &str = include_str!("../starter/README.md");
const STARTER_AGENTS: &str = include_str!("../starter/AGENTS.md");
static VERBOSE_BUILD: AtomicBool = AtomicBool::new(false);

#[derive(Parser)]
#[command(
    name = "mach",
    version,
    about = "Create and develop multiplayer browser games"
)]
struct Cli {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Subcommand)]
enum CommandKind {
    /// Create a multiplayer game.
    New { directory: PathBuf },
    /// Run the game locally and rebuild when files change.
    Dev {
        #[command(subcommand)]
        command: Option<DevCommand>,
        #[arg(
            long,
            hide = true,
            value_parser = clap::value_parser!(u16).range(1..)
        )]
        server_port: Option<u16>,
        /// Do not launch the game window automatically.
        #[arg(long)]
        no_open: bool,
        #[arg(long, hide = true)]
        verbose: bool,
    },
    /// Download the tools and build data needed for local development.
    Setup,
    /// Validate the source project manifest without starting the engine.
    Validate {
        #[arg(default_value = ".")]
        directory: PathBuf,
        /// Emit machine-readable JSON for coding agents and automation.
        #[arg(long, hide = true)]
        json: bool,
    },
    /// Check whether this machine can run the game locally.
    Doctor {
        /// Emit machine-readable JSON.
        #[arg(long, hide = true)]
        json: bool,
    },
    /// Sign this CLI into machinesatplay.com.
    Login {
        /// Print the browser URL without opening it.
        #[arg(long)]
        no_open: bool,
    },
    /// Remove the saved machinesatplay.com session.
    Logout,
    /// Print the account used for deployments.
    Whoami,
    /// Build and publish this game to machinesatplay.com.
    Deploy {
        #[arg(default_value = ".")]
        directory: PathBuf,
        /// Use existing browser artifacts without rebuilding a source engine.
        #[arg(long, hide = true)]
        no_build: bool,
        /// Emit the deployment result as JSON.
        #[arg(long, hide = true)]
        json: bool,
    },
    /// List deployments owned by the signed-in account.
    Deployments {
        /// Emit machine-readable JSON.
        #[arg(long, hide = true)]
        json: bool,
    },
    /// Activate an earlier deployment for a game.
    Rollback { slug: String, deployment: String },
    #[command(hide = true)]
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    #[command(hide = true)]
    Prepare {
        #[arg(long, hide = true)]
        deploy: bool,
    },
    #[command(hide = true)]
    PrepareProject {
        #[arg(default_value = ".")]
        directory: PathBuf,
    },
    #[command(hide = true)]
    SendTelemetry,
}

#[derive(Subcommand)]
enum DevCommand {
    /// Open another client in the running dev session.
    Open,
}

fn main() {
    if let Err(error) = self_update::update_and_reexec() {
        eprintln!("mach: cannot install the latest CLI: {error}");
        std::process::exit(1);
    }
    let cli = Cli::parse();
    if matches!(cli.command, CommandKind::SendTelemetry) {
        telemetry::send_pending_file();
        return;
    }
    let invocation = cli
        .command
        .telemetry_name()
        .map(telemetry::Invocation::start);
    let result = run(cli.command);
    if let Some(invocation) = invocation {
        invocation.finish(&result);
    }
    if let Err(error) = result {
        eprintln!("mach: {error}");
        std::process::exit(1);
    }
}

impl CommandKind {
    fn telemetry_name(&self) -> Option<&'static str> {
        match self {
            Self::New { .. } => Some("new"),
            Self::Dev {
                command: Some(DevCommand::Open),
                ..
            } => Some("dev_open"),
            Self::Dev { .. } => Some("dev"),
            Self::Setup => Some("setup"),
            Self::Validate { .. } => Some("validate"),
            Self::Doctor { .. } => Some("doctor"),
            Self::Login { .. } => Some("login"),
            Self::Logout => Some("logout"),
            Self::Whoami => Some("whoami"),
            Self::Deploy { .. } => Some("deploy"),
            Self::Deployments { .. } => Some("deployments"),
            Self::Rollback { .. } => Some("rollback"),
            Self::Cache { .. }
            | Self::Prepare { .. }
            | Self::PrepareProject { .. }
            | Self::SendTelemetry => None,
        }
    }
}

fn run(command: CommandKind) -> Result<(), String> {
    if let Err(error) = cache::maybe_prune_stale_cache() {
        eprintln!("mach: cache cleanup skipped: {error}");
    }
    match command {
        CommandKind::New { directory } => create_project(&directory),
        CommandKind::Dev {
            command,
            server_port,
            no_open,
            verbose,
        } => match command {
            Some(DevCommand::Open) if server_port.is_some() || no_open || verbose => {
                Err("mach dev open does not accept dev startup options".to_owned())
            }
            Some(DevCommand::Open) => dev_open(),
            None => dev(server_port, no_open, verbose),
        },
        CommandKind::Setup => setup_command(),
        CommandKind::Validate { directory, json } => validate_command(&directory, json),
        CommandKind::Doctor { json } => doctor_command(json),
        CommandKind::Login { no_open } => login_command(no_open).map(|_| ()),
        CommandKind::Logout => logout_command(),
        CommandKind::Whoami => whoami_command(),
        CommandKind::Deploy {
            directory,
            no_build,
            json,
        } => deploy_command(&directory, no_build, json),
        CommandKind::Deployments { json } => deployments_command(json),
        CommandKind::Rollback { slug, deployment } => rollback_command(&slug, &deployment),
        CommandKind::Cache { command } => cache_command(command),
        CommandKind::Prepare { deploy } => prepare_command(deploy),
        CommandKind::PrepareProject { directory } => prepare_project_command(&directory),
        CommandKind::SendTelemetry => Ok(()),
    }
}

fn setup_command() -> Result<(), String> {
    let started = Instant::now();
    let _cache_lock = lock_cache_shared()?;
    starter_bundle::ensure_cached()?;
    let seed_ready = build_seed::seed_is_ready()?;
    spawn_background_prepare()?;
    println!("mach: ready");
    telemetry::setup_summary(
        "success",
        started.elapsed(),
        if seed_ready {
            "ready"
        } else {
            "background_started"
        },
    );
    Ok(())
}

fn prepare_command(deployment: bool) -> Result<(), String> {
    let started = Instant::now();
    let _cache_lock = lock_cache_shared()?;
    let result = (|| {
        ensure_native_build_tools()?;
        let _build_seed = if deployment {
            prepare_deploy_build_seed()?
        } else {
            prepare_build_seed()?
        };
        Ok(())
    })();
    telemetry::setup_summary(
        if result.is_ok() { "success" } else { "error" },
        started.elapsed(),
        if result.is_ok() { "ready" } else { "failed" },
    );
    telemetry::flush();
    result
}

fn prepare_project_command(directory: &Path) -> Result<(), String> {
    let project = load_project(directory).map_err(|error| error.to_string())?;
    let _cache_lock = lock_cache_shared()?;
    let _build_seed = prepare_build_seed()?;
    prepare_project_cache(&project.root, false)?;
    println!("{}", cargo_target_dir(&project.root)?.display());
    Ok(())
}

fn spawn_background_prepare() -> Result<(), String> {
    if build_seed::seed_is_ready()? {
        return Ok(());
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("cannot locate the mach executable: {error}"))?;
    Command::new(executable)
        .arg("prepare")
        .env("MACH_SKIP_UPDATE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot start background cache preparation: {error}"))?;
    println!("mach: development cache is downloading in the background");
    Ok(())
}

pub(crate) fn releases_url() -> String {
    std::env::var("MACH_RELEASES_URL")
        .unwrap_or_else(|_| RELEASES_URL.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_rejects_zero_ports() {
        assert!(Cli::try_parse_from(["mach", "dev", "--port", "0"]).is_err());
        assert!(Cli::try_parse_from(["mach", "dev", "--server-port", "0"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["mach", "dev", "--verbose"])
                .expect("parse verbose dev")
                .command,
            CommandKind::Dev { verbose: true, .. }
        ));
        assert!(Cli::try_parse_from(["mach", "dev", "--jobs", "2"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["mach", "dev", "open"])
                .expect("parse dev open")
                .command,
            CommandKind::Dev {
                command: Some(DevCommand::Open),
                ..
            }
        ));
    }

    #[test]
    fn removed_cli_knobs_stay_removed() {
        assert!(Cli::try_parse_from(["mach", "version"]).is_err());
        assert!(Cli::try_parse_from(["mach", "inspect"]).is_err());
        assert!(Cli::try_parse_from(["mach", "dev", "--with-webgl2"]).is_err());
        assert!(Cli::try_parse_from(["mach", "deploy", "--with-webgl2"]).is_err());
    }

    #[test]
    fn normal_help_only_shows_normal_options() {
        use clap::CommandFactory;

        let root_help = Cli::command().render_long_help().to_string();
        assert!(!root_help.contains("inspect"));
        assert!(!root_help.contains("cache"));

        let mut command = Cli::command();
        let dev_help = command
            .find_subcommand_mut("dev")
            .expect("dev command")
            .render_long_help()
            .to_string();
        assert!(dev_help.contains("--no-open"));
        assert!(dev_help.contains("open"));
        for hidden in ["--port", "--server-port", "--verbose", "--with-webgl2"] {
            assert!(!dev_help.contains(hidden), "unexpected option: {hidden}");
        }

        for (subcommand, hidden) in [
            ("validate", &["--json"][..]),
            ("doctor", &["--json"][..]),
            ("deploy", &["--json", "--no-build"][..]),
            ("deployments", &["--json"][..]),
        ] {
            let mut command = Cli::command();
            let help = command
                .find_subcommand_mut(subcommand)
                .expect("public command")
                .render_long_help()
                .to_string();
            for option in hidden {
                assert!(!help.contains(option), "unexpected option: {option}");
            }
        }
    }

    #[test]
    fn dev_change_plan_rebuilds_only_affected_game_targets() {
        let root = Path::new("/game");

        assert_eq!(
            dev_build_plan(
                &[root.join("crates/game-client/src/render.rs")],
                root,
                root,
                true,
            ),
            DevBuildPlan {
                native_client: true,
                ..Default::default()
            }
        );
        assert_eq!(
            dev_build_plan(
                &[root.join("crates/game-core/Cargo.toml")],
                root,
                root,
                true,
            ),
            DevBuildPlan {
                native_client: true,
                native_server: true,
                ..Default::default()
            }
        );
        assert_eq!(
            dev_build_plan(
                &[root.join("crates/game-client/Cargo.toml")],
                root,
                root,
                true,
            ),
            DevBuildPlan {
                native_client: true,
                ..Default::default()
            }
        );
        assert_eq!(
            dev_build_plan(
                &[root.join("crates/game-server/Cargo.toml")],
                root,
                root,
                true,
            ),
            DevBuildPlan {
                native_server: true,
                ..Default::default()
            }
        );
        assert_eq!(
            dev_build_plan(
                &[root.join("crates/game-server/src/game.rs")],
                root,
                root,
                true,
            ),
            DevBuildPlan {
                native_server: true,
                ..Default::default()
            }
        );
        assert_eq!(
            dev_build_plan(
                &[root.join("crates/game-core/src/lib.rs")],
                root,
                root,
                true
            ),
            DevBuildPlan {
                native_client: true,
                native_server: true,
                ..Default::default()
            }
        );
        assert_eq!(
            dev_build_plan(&[root.join("assets/map.glb")], root, root, false),
            DevBuildPlan {
                refresh: true,
                ..Default::default()
            }
        );
        assert_eq!(
            dev_build_plan(
                &[
                    root.join("crates/game-client/src/client.rs"),
                    root.join("crates/game-server/src/game.rs")
                ],
                root,
                root,
                true,
            ),
            DevBuildPlan {
                native_client: true,
                native_server: true,
                ..Default::default()
            }
        );
        assert_eq!(
            dev_build_plan(&[root.join("src/render.rs")], root, root, true),
            DevBuildPlan {
                native_client: true,
                ..Default::default()
            }
        );
        assert_eq!(
            dev_build_plan(
                &[
                    root.join("web/mach_webgpu.js.next"),
                    root.join("web/snippets/generated/inline0.js"),
                    root.join(".mach/bin/mach-server.next"),
                ],
                root,
                root,
                true,
            ),
            DevBuildPlan::default()
        );
    }

    #[test]
    fn mach_new_uses_the_embedded_bevy_starter() {
        let test_root = unique_test_root("offline-new");
        create_project(&test_root).expect("create offline starter");

        assert!(!test_root.join("src/lib.rs").exists());
        assert!(test_root.join("src/main.rs").is_file());
        assert!(test_root.join("src/runtime.rs").is_file());
        assert!(test_root.join("crates/game-client/src/lib.rs").is_file());
        assert!(test_root.join("crates/game-core/src/lib.rs").is_file());
        assert!(test_root.join("crates/game-server/src/main.rs").is_file());
        assert!(test_root.join("crates/game-server/src/game.rs").is_file());
        assert!(test_root.join("assets").is_dir());
        assert!(!test_root.join("server").exists());
        assert!(!test_root.join("sandbox").exists());
        assert!(!test_root.join("src/stage.rs").exists());
        let cargo =
            fs::read_to_string(test_root.join("Cargo.toml")).expect("read generated Cargo.toml");
        assert!(cargo.contains("default = [\"client\"]"));
        assert!(cargo.contains("[profile.mach-dev]"));
        assert!(cargo.contains("[profile.mach-dev.package.game-client]"));
        assert!(cargo.contains("[profile.mach-dev.package.game-server]"));
        assert!(!cargo.contains("name = \"game-host\""));
        assert!(!cargo.contains("host = ["));
        assert!(!cargo.contains("room-host"));
        assert!(!cargo.contains("vendor/walrus"));
        assert!(!cargo.contains("vendor/wasm-bindgen-cli-support"));
        let fetch = Command::new("cargo")
            .args([
                "fetch",
                "--locked",
                "--offline",
                "--target",
                "wasm32-unknown-unknown",
            ])
            .current_dir(&test_root)
            .status()
            .expect("run cargo fetch for generated starter");
        assert!(fetch.success(), "generated client lockfile must be stable");
        fs::remove_dir_all(&test_root).expect("remove test directory");
    }

    #[test]
    fn native_server_changes_do_not_rebuild_the_client() {
        let root = Path::new("/game");
        assert_eq!(
            dev_build_plan(
                &[root.join("crates/game-server/src/game.rs")],
                root,
                root,
                true,
            ),
            DevBuildPlan {
                native_server: true,
                ..Default::default()
            }
        );
    }

    #[test]
    fn validated_artifact_replaces_the_last_good_file() {
        let test_root = unique_test_root("validated-artifact");
        fs::create_dir_all(&test_root).expect("create test directory");
        let active = test_root.join("mach-server");
        let candidate = test_root.join("mach-server.next");
        fs::write(&active, b"last good").expect("write active artifact");
        fs::write(&candidate, b"next good").expect("write candidate artifact");

        activate_validated_file(&candidate, &active).expect("activate candidate");

        assert_eq!(
            fs::read(&active).expect("read active artifact"),
            b"next good"
        );
        assert!(!candidate.exists());
        fs::remove_dir_all(&test_root).expect("remove test directory");
    }

    #[test]
    fn server_child_fixture() {
        if std::env::var_os("MACH_CLI_SERVER_CHILD_FIXTURE").is_some() {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    #[test]
    fn server_child_guard_stops_and_reaps_child() {
        let child = Command::new(std::env::current_exe().expect("find test executable"))
            .arg("--exact")
            .arg("tests::server_child_fixture")
            .arg("--nocapture")
            .env("MACH_CLI_SERVER_CHILD_FIXTURE", "1")
            .spawn()
            .expect("start server child fixture");
        let mut child = GameServerChild { child };

        child.stop();

        assert!(child.try_wait().expect("inspect stopped child").is_some());
    }

    #[test]
    fn server_port_lock_rejects_a_second_dev_process() {
        let test_root = unique_test_root("server-port-lock");
        let first = lock_server_port_at(&test_root, 5911).expect("lock server port");

        let error = lock_server_port_at(&test_root, 5911).expect_err("reject second lock");
        assert_eq!(
            error,
            "server port 5911 is already in use by another mach dev process"
        );

        drop(first);
        lock_server_port_at(&test_root, 5911).expect("reuse released server port");
        fs::remove_dir_all(&test_root).expect("remove test directory");
    }

    #[test]
    fn server_port_check_rejects_an_occupied_udp_port() {
        let socket = std::net::UdpSocket::bind(("0.0.0.0", 0)).expect("bind UDP fixture");
        let port = socket
            .local_addr()
            .expect("read UDP fixture address")
            .port();

        let error = ensure_server_port_available(port).expect_err("reject occupied UDP port");
        assert!(
            error.starts_with(&format!("server port {port} is unavailable:")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn generated_schemas_match_parser_defaults() {
        let project_schema: serde_json::Value =
            serde_json::from_str(STARTER_GAME_SCHEMA).expect("parse project schema");
        assert_eq!(
            project_schema["required"],
            serde_json::json!(["name", "engineVersion"])
        );
        assert_eq!(project_schema["properties"]["schemaVersion"]["default"], 1);
        assert!(project_schema["properties"].get("world").is_none());
    }

    #[test]
    fn repository_contains_every_source_starter_boundary() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("CLI crate has a repository parent");

        validate_source_starter_root(root).expect("repository is a complete source starter");
    }

    #[test]
    fn cache_subcommands_parse() {
        assert!(matches!(
            Cli::try_parse_from(["mach", "cache", "size"])
                .expect("parse cache size")
                .command,
            CommandKind::Cache {
                command: CacheCommand::Size
            }
        ));
        assert!(Cli::try_parse_from(["mach", "cache", "prune"]).is_err());
    }

    fn unique_test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mach-cli-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system clock is after the Unix epoch")
                .as_nanos()
        ))
    }
}
