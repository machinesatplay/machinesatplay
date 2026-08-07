use super::*;
use std::collections::BTreeSet;
use uuid::Uuid;

const DEV_SESSION_VERSION: u8 = 1;
const DEV_SESSION_FILE: &str = "dev-session.json";
const DEV_SESSION_LOCK_FILE: &str = "dev-session.lock";

#[derive(Serialize, Deserialize)]
struct DevSessionDescriptor {
    version: u8,
    pid: u32,
    address: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum DevControlRequest {
    OpenClient,
    ClientJoined { client_id: u64, token: String },
}

#[derive(Serialize, Deserialize)]
struct DevControlResponse {
    client_id: Option<u64>,
    error: Option<String>,
}

impl DevControlResponse {
    fn opened(client_id: u64) -> Self {
        Self {
            client_id: Some(client_id),
            error: None,
        }
    }

    fn failed(error: impl Into<String>) -> Self {
        Self {
            client_id: None,
            error: Some(error.into()),
        }
    }
}

enum DevControlCommand {
    OpenClient {
        response: mpsc::Sender<DevControlResponse>,
    },
    ClientJoined {
        client_id: u64,
    },
}

struct DevControlServer {
    descriptor_path: PathBuf,
    address: String,
    stopping: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    _session_lock: fs::File,
}

impl DevControlServer {
    fn start(
        project_root: &Path,
        session_lock: fs::File,
        token: String,
    ) -> Result<(Self, mpsc::Receiver<DevControlCommand>), String> {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .map_err(|error| format!("cannot start dev session control: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("cannot configure dev session control: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("cannot read dev session control address: {error}"))?;
        let descriptor_path = dev_session_path(project_root);
        let descriptor = DevSessionDescriptor {
            version: DEV_SESSION_VERSION,
            pid: std::process::id(),
            address: address.to_string(),
        };
        let bytes = serde_json::to_vec(&descriptor)
            .map_err(|error| format!("cannot encode dev session: {error}"))?;

        let (command_tx, command_rx) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let thread_stopping = stopping.clone();
        let thread = std::thread::Builder::new()
            .name("mach-dev-control".to_owned())
            .spawn(move || run_dev_control(listener, command_tx, thread_stopping, token))
            .map_err(|error| format!("cannot start dev session control: {error}"))?;
        let candidate = descriptor_path.with_extension("next");
        let publish = fs::write(&candidate, bytes)
            .map_err(|error| format!("cannot write {}: {error}", candidate.display()))
            .and_then(|()| activate_validated_file(&candidate, &descriptor_path));
        if let Err(error) = publish {
            stopping.store(true, Ordering::SeqCst);
            let _ = thread.join();
            let _ = fs::remove_file(candidate);
            return Err(error);
        }

        Ok((
            Self {
                descriptor_path,
                address: address.to_string(),
                stopping,
                thread: Some(thread),
                _session_lock: session_lock,
            },
            command_rx,
        ))
    }

    fn address(&self) -> &str {
        &self.address
    }
}

impl Drop for DevControlServer {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.descriptor_path);
    }
}

fn run_dev_control(
    listener: std::net::TcpListener,
    command_tx: mpsc::Sender<DevControlCommand>,
    stopping: Arc<AtomicBool>,
    token: String,
) {
    while !stopping.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                handle_dev_control_connection(stream, &command_tx, &stopping, &token)
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                eprintln!("mach: dev session control stopped: {error}");
                break;
            }
        }
    }
}

fn handle_dev_control_connection(
    mut stream: std::net::TcpStream,
    command_tx: &mpsc::Sender<DevControlCommand>,
    stopping: &AtomicBool,
    token: &str,
) {
    let timeout = Duration::from_secs(2);
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let request = serde_json::from_reader::<_, DevControlRequest>(&mut stream);
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_write_timeout(None);
    let response = match request {
        Ok(DevControlRequest::OpenClient) => {
            let (response_tx, response_rx) = mpsc::channel();
            if command_tx
                .send(DevControlCommand::OpenClient {
                    response: response_tx,
                })
                .is_err()
            {
                DevControlResponse::failed("the dev session stopped")
            } else {
                loop {
                    match response_rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(response) => break response,
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            break DevControlResponse::failed("the dev session stopped");
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) if stopping.load(Ordering::SeqCst) => {
                            break DevControlResponse::failed("the dev session stopped");
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            }
        }
        Ok(DevControlRequest::ClientJoined {
            client_id,
            token: provided,
        }) if provided == token => {
            if command_tx
                .send(DevControlCommand::ClientJoined { client_id })
                .is_ok()
            {
                DevControlResponse::opened(client_id)
            } else {
                DevControlResponse::failed("the dev session stopped")
            }
        }
        Ok(DevControlRequest::ClientJoined { .. }) => {
            DevControlResponse::failed("invalid dev session token")
        }
        Err(error) => DevControlResponse::failed(format!("invalid dev session request: {error}")),
    };
    let _ = serde_json::to_writer(&mut stream, &response);
    let _ = stream.write_all(b"\n");
    let _ = stream.flush();
}

fn dev_session_path(project_root: &Path) -> PathBuf {
    project_root.join(".mach").join(DEV_SESSION_FILE)
}

fn lock_dev_session(project_root: &Path) -> Result<fs::File, String> {
    let mach_dir = project_root.join(".mach");
    fs::create_dir_all(&mach_dir)
        .map_err(|error| format!("cannot create {}: {error}", mach_dir.display()))?;
    let path = mach_dir.join(DEV_SESSION_LOCK_FILE);
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    file.try_lock_exclusive()
        .map_err(|_| "mach dev is already running for this project".to_owned())?;
    Ok(file)
}

pub(super) fn dev_open() -> Result<(), String> {
    let project_root = std::env::current_dir().map_err(|error| error.to_string())?;
    let client_id = request_dev_open(&project_root)?;
    println!("mach: opened client {client_id}");
    Ok(())
}

fn request_dev_open(project_root: &Path) -> Result<u64, String> {
    let descriptor_path = dev_session_path(project_root);
    let bytes = fs::read(&descriptor_path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            "no dev session is running for this project".to_owned()
        } else {
            format!("cannot read {}: {error}", descriptor_path.display())
        }
    })?;
    let descriptor: DevSessionDescriptor = serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot read {}: {error}", descriptor_path.display()))?;
    if descriptor.version != DEV_SESSION_VERSION {
        return Err("the running dev session uses a different control version".to_owned());
    }
    let address = descriptor
        .address
        .parse::<std::net::SocketAddr>()
        .map_err(|error| format!("cannot read {}: {error}", descriptor_path.display()))?;
    if !address.ip().is_loopback() {
        return Err(format!(
            "cannot read {}: control address is not local",
            descriptor_path.display()
        ));
    }

    let mut stream = match std::net::TcpStream::connect(address) {
        Ok(stream) => stream,
        Err(error) => match lock_dev_session(project_root) {
            Ok(_stale_session_lock) => {
                let _ = fs::remove_file(&descriptor_path);
                return Err("no dev session is running for this project".to_owned());
            }
            Err(_) => {
                return Err(format!(
                    "the dev session is not accepting requests yet: {error}"
                ));
            }
        },
    };
    let timeout = Duration::from_secs(10);
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("cannot configure dev session connection: {error}"))?;
    serde_json::to_writer(&mut stream, &DevControlRequest::OpenClient)
        .map_err(|error| format!("cannot send dev session request: {error}"))?;
    stream
        .write_all(b"\n")
        .map_err(|error| format!("cannot send dev session request: {error}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|error| format!("cannot finish dev session request: {error}"))?;
    let response: DevControlResponse = serde_json::from_reader(&mut stream)
        .map_err(|error| format!("cannot read dev session response: {error}"))?;
    if let Some(error) = response.error {
        return Err(error);
    }
    let client_id = response
        .client_id
        .ok_or_else(|| "the dev session returned an empty response".to_owned())?;
    Ok(client_id)
}

pub(super) fn build_job_budget() -> Result<usize, String> {
    for name in ["MACH_BUILD_JOBS", "CARGO_BUILD_JOBS"] {
        if let Some(value) = std::env::var_os(name) {
            return value
                .to_string_lossy()
                .parse::<usize>()
                .ok()
                .filter(|jobs| *jobs > 0)
                .ok_or_else(|| format!("{name} must be a positive integer"));
        }
    }
    let parallelism = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2);
    let memory_jobs = system_memory_bytes()
        .map(|bytes| (bytes / (3 * GIB)).max(1) as usize)
        .unwrap_or(2);
    Ok(parallelism.min(memory_jobs).clamp(1, 12))
}

fn system_memory_bytes() -> Option<u64> {
    if cfg!(target_os = "macos") {
        return Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|value| value.trim().parse().ok());
    }
    if cfg!(target_os = "linux") {
        return fs::read_to_string("/proc/meminfo")
            .ok()?
            .lines()
            .find_map(|line| {
                let kib = line
                    .strip_prefix("MemAvailable:")?
                    .split_whitespace()
                    .next()?;
                kib.parse::<u64>().ok()?.checked_mul(1024)
            });
    }
    None
}

fn configure_cargo_build(command: &mut Command, total_jobs: usize, concurrent_builds: usize) {
    let jobs = (total_jobs / concurrent_builds.max(1)).max(1);
    command.env("CARGO_BUILD_JOBS", jobs.to_string());
    if !VERBOSE_BUILD.load(Ordering::Relaxed) {
        command.arg("--quiet");
    }
}

fn concurrent_build_jobs(total_jobs: usize) -> (usize, usize) {
    let server_jobs = (total_jobs / 6).max(1);
    let client_jobs = total_jobs.saturating_sub(server_jobs).max(1);
    (client_jobs, server_jobs)
}

pub(super) fn format_duration(duration: Duration) -> String {
    if duration < Duration::from_secs(1) {
        format!("{}ms", duration.as_millis())
    } else {
        format!("{:.1}s", duration.as_secs_f32())
    }
}

fn with_build_heartbeat<T>(operation: impl FnOnce() -> T) -> T {
    let (done_tx, done_rx) = mpsc::channel();
    let reporter = std::thread::spawn(move || {
        let started = Instant::now();
        loop {
            match done_rx.recv_timeout(Duration::from_secs(15)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => println!(
                    "  build     still working ({})",
                    format_duration(started.elapsed())
                ),
            }
        }
    });
    let result = operation();
    let _ = done_tx.send(());
    let _ = reporter.join();
    result
}

pub(super) fn dev(
    requested_server_port: Option<u16>,
    no_open: bool,
    verbose: bool,
) -> Result<(), String> {
    let dev_started = Instant::now();
    let dev_session_id = Uuid::new_v4().to_string();
    let dev_control_token = Uuid::new_v4().to_string();
    VERBOSE_BUILD.store(verbose, Ordering::Relaxed);
    let build_jobs = build_job_budget()?;
    let _cache_lock = lock_cache_shared()?;
    let (server_port, _server_port_lock) = select_server_port(requested_server_port)?;
    let project_root = std::env::current_dir().map_err(|error| error.to_string())?;
    let project = load_project(&project_root).map_err(|error| error.to_string())?;
    ensure_valid_project(&project)?;
    let dev_session_lock = lock_dev_session(&project.root)?;

    let stopping = Arc::new(AtomicBool::new(false));
    let signal = stopping.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))
        .map_err(|error| format!("cannot install Ctrl-C handler: {error}"))?;

    let (event_tx, event_rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = event_tx.send(event);
    })
    .map_err(|error| format!("cannot start file watcher: {error}"))?;
    configure_project_watcher(&mut watcher, &project.root, &project.root, true)?;

    let starter = match crate::starter_bundle::install_if_pristine(&project.root) {
        Ok(installed) => installed,
        Err(error) => {
            eprintln!("mach: prebuilt starter unavailable, building locally ({error})");
            false
        }
    };
    let (client_binary, server_binary) = if starter {
        println!("  client    ready from starter bundle");
        println!("  server    ready from starter bundle");
        crate::spawn_background_prepare()?;
        (
            project.root.join(".mach/bin/mach-client"),
            project.root.join(".mach/bin/mach-server"),
        )
    } else {
        with_build_heartbeat(|| initial_dev_build(&project.root, build_jobs))?
    };
    let certificate_dir = ensure_certificate(&server_binary, &project.root)?;
    let mut local_build_ready = !starter;
    let mut server = Some(start_game_server(
        &server_binary,
        &project.root,
        &certificate_dir,
        server_port,
    )?);
    let (dev_control, control_rx) =
        DevControlServer::start(&project.root, dev_session_lock, dev_control_token.clone())?;
    let mut clients = BTreeMap::new();
    if !no_open {
        clients.insert(
            1,
            start_game_client(
                &client_binary,
                &project.root,
                &certificate_dir,
                server_port,
                1,
                dev_control.address(),
                &dev_control_token,
            )?,
        );
    }
    let mut next_client_id: u64 = if no_open { 1 } else { 2 };
    let mut clients_opened = if no_open { 0 } else { 1 };
    let mut joined_clients = BTreeSet::new();
    let mut first_join_reported = false;
    let mut rebuild_attempts = 0;
    let mut rebuild_successes = 0;
    let mut rebuild_failures = 0;

    crate::telemetry::dev_ready(&dev_session_id, dev_started.elapsed(), starter, !no_open);

    println!("\n  game      native window");
    println!("  server    127.0.0.1:{server_port}");
    println!("  watching  client / server / assets");
    println!("  open      mach dev open");
    println!("  stop      ctrl-c\n");

    while !stopping.load(Ordering::SeqCst) {
        if let Some(child) = server.as_mut() {
            if let Some(status) = child
                .try_wait()
                .map_err(|error| format!("cannot inspect game server: {error}"))?
            {
                eprintln!("mach: server exited unexpectedly with {status}; watching for changes");
                server = None;
            }
        }
        let mut exited_clients = Vec::new();
        for (&client_id, child) in &mut clients {
            if let Some(status) = child
                .try_wait()
                .map_err(|error| format!("cannot inspect game client: {error}"))?
            {
                eprintln!("mach: client {client_id} exited with {status}; watching for changes");
                exited_clients.push(client_id);
            }
        }
        for client_id in exited_clients {
            clients.remove(&client_id);
        }

        while let Ok(command) = control_rx.try_recv() {
            match command {
                DevControlCommand::OpenClient { response } => {
                    let client_id = next_client_id;
                    let Some(following_client_id) = next_client_id.checked_add(1) else {
                        let _ = response.send(DevControlResponse::failed(
                            "cannot assign another client id",
                        ));
                        continue;
                    };
                    let result = match start_game_client(
                        &project.root.join(".mach/bin/mach-client"),
                        &project.root,
                        &certificate_dir,
                        server_port,
                        client_id,
                        dev_control.address(),
                        &dev_control_token,
                    ) {
                        Ok(client) => {
                            clients.insert(client_id, client);
                            next_client_id = following_client_id;
                            clients_opened += 1;
                            DevControlResponse::opened(client_id)
                        }
                        Err(error) => DevControlResponse::failed(error),
                    };
                    let _ = response.send(result);
                }
                DevControlCommand::ClientJoined { client_id } => {
                    if !clients.contains_key(&client_id) || !joined_clients.insert(client_id) {
                        continue;
                    }
                    if !first_join_reported {
                        crate::telemetry::dev_world_joined(
                            &dev_session_id,
                            dev_started.elapsed(),
                            if !no_open && client_id == 1 {
                                "initial"
                            } else {
                                "dev_open"
                            },
                        );
                        first_join_reported = true;
                    }
                }
            }
        }

        let Ok(event) = event_rx.recv_timeout(Duration::from_millis(100)) else {
            continue;
        };
        let Ok(event) = event else {
            continue;
        };
        // Coalesce the burst of filesystem notifications emitted by one save.
        let mut changed_paths = event.paths;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_millis(5)) {
            if let Ok(event) = event {
                changed_paths.extend(event.paths);
            }
        }
        let plan = dev_build_plan(&changed_paths, &project.root, &project.root, true);
        if plan.is_empty() {
            continue;
        }
        let next_project = match load_project(&project.root) {
            Ok(project) => project,
            Err(error) => {
                eprintln!("\nmach: change is invalid: {error}");
                println!("mach: keeping the last valid build and watching for a fix");
                continue;
            }
        };
        if let Err(error) = ensure_valid_project(&next_project) {
            eprintln!("\nmach: change is invalid: {error}");
            println!("mach: keeping the last valid build and watching for a fix");
            continue;
        }
        rebuild_attempts += 1;
        let result = with_build_heartbeat(|| {
            apply_local_dev_build(
                &next_project.root,
                build_jobs,
                &certificate_dir,
                server_port,
                &mut server,
                &mut clients,
                plan,
                &mut local_build_ready,
                dev_control.address(),
                &dev_control_token,
            )
        });
        if let Err(error) = result {
            rebuild_failures += 1;
            eprintln!("mach: local build failed: {error}");
            println!("mach: last good build is still running");
        } else {
            rebuild_successes += 1;
        }
    }

    drop(clients);
    drop(server);
    crate::telemetry::dev_session_summary(
        &dev_session_id,
        dev_started.elapsed(),
        clients_opened,
        joined_clients.len() as u64,
        rebuild_attempts,
        rebuild_successes,
        rebuild_failures,
    );
    println!("mach: stopped");
    Ok(())
}

pub(super) fn local_creator_build(
    project_root: &Path,
    build_jobs: usize,
    mode: BrowserBuild,
) -> Result<PathBuf, String> {
    let deployment = matches!(mode, BrowserBuild::Deployment);
    let _build_seed = if deployment {
        crate::build_seed::prepare_deploy_build_seed()?
    } else {
        prepare_build_seed()?
    };
    crate::build_seed::prepare_project_cache(project_root, deployment)?;
    if build_jobs > 1 {
        let (client_jobs, server_jobs) = concurrent_build_jobs(build_jobs);
        let (client, server) = std::thread::scope(|scope| {
            let client = scope.spawn(|| build_browser(project_root, client_jobs, mode));
            let server = scope.spawn(|| build_native_server(project_root, server_jobs, mode));
            (client.join(), server.join())
        });
        client.map_err(|_| "client build thread panicked".to_owned())??;
        server.map_err(|_| "server build thread panicked".to_owned())?
    } else {
        build_browser(project_root, 1, mode)?;
        build_native_server(project_root, 1, mode)
    }
}

fn initial_dev_build(project_root: &Path, build_jobs: usize) -> Result<(PathBuf, PathBuf), String> {
    ensure_native_build_tools()?;
    let _build_seed = prepare_build_seed()?;
    crate::build_seed::prepare_project_cache(project_root, false)?;
    build_native_pair(project_root, build_jobs)
}

#[allow(clippy::too_many_arguments)]
fn apply_local_dev_build(
    project_root: &Path,
    build_jobs: usize,
    certificate_dir: &Path,
    server_port: u16,
    server: &mut Option<GameServerChild>,
    clients: &mut BTreeMap<u64, GameServerChild>,
    plan: DevBuildPlan,
    local_build_ready: &mut bool,
    dev_control_address: &str,
    dev_control_token: &str,
) -> Result<(), String> {
    if !*local_build_ready {
        ensure_native_build_tools()?;
        crate::build_seed::prepare_project_cache(project_root, false)?;
        *local_build_ready = true;
    }
    let started = Instant::now();
    let (client_binary, server_binary) = match (plan.native_client, plan.native_server) {
        (true, true) => {
            let (client, server) = build_native_pair(project_root, build_jobs)?;
            (Some(client), Some(server))
        }
        (true, false) => (Some(build_native_client(project_root, build_jobs)?), None),
        (false, true) => (
            None,
            Some(build_native_server(
                project_root,
                build_jobs,
                BrowserBuild::Development,
            )?),
        ),
        (false, false) => (None, None),
    };
    if let Some(server_binary) = server_binary {
        server.take();
        *server = Some(start_game_server(
            &server_binary,
            project_root,
            certificate_dir,
            server_port,
        )?);
    }
    if (client_binary.is_some() || plan.refresh) && !clients.is_empty() {
        let binary = client_binary.unwrap_or_else(|| project_root.join(".mach/bin/mach-client"));
        let client_ids = clients.keys().copied().collect::<Vec<_>>();
        clients.clear();
        for client_id in client_ids {
            clients.insert(
                client_id,
                start_game_client(
                    &binary,
                    project_root,
                    certificate_dir,
                    server_port,
                    client_id,
                    dev_control_address,
                    dev_control_token,
                )?,
            );
        }
    }
    if plan.native_client || plan.native_server {
        println!(
            "mach: local build ready in {}",
            format_duration(started.elapsed())
        );
    }
    Ok(())
}

pub(super) fn ensure_valid_project(project: &ProjectFiles) -> Result<(), String> {
    let issues = project_issues(project);
    if issues.is_empty() {
        return Ok(());
    }
    Err(format!(
        "project validation failed:\n  - {}",
        issues
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n  - ")
    ))
}

pub(super) fn validate_engine_root(root: &Path) -> Result<(), String> {
    for required in ["Cargo.toml", "src/main.rs", "web/index.html", "assets"] {
        if !root.join(required).exists() {
            return Err(format!("{} is missing {required}", root.display()));
        }
    }
    Ok(())
}

fn lock_server_port(port: u16) -> Result<fs::File, String> {
    let lock_root = std::env::temp_dir().join("mach-server-ports");
    lock_server_port_at(&lock_root, port)
}

fn select_server_port(requested: Option<u16>) -> Result<(u16, fs::File), String> {
    if let Some(port) = requested {
        let lock = lock_server_port(port)?;
        ensure_server_port_available(port)?;
        return Ok((port, lock));
    }

    for offset in 0..100 {
        let Some(port) = DEFAULT_SERVER_PORT.checked_add(offset) else {
            break;
        };
        let Ok(lock) = lock_server_port(port) else {
            continue;
        };
        if ensure_server_port_available(port).is_ok() {
            return Ok((port, lock));
        }
    }
    Err("cannot find an available local server port".to_owned())
}

pub(super) fn lock_server_port_at(lock_root: &Path, port: u16) -> Result<fs::File, String> {
    fs::create_dir_all(lock_root)
        .map_err(|error| format!("cannot create server port lock directory: {error}"))?;
    let path = lock_root.join(format!("{port}.lock"));
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    file.try_lock_exclusive()
        .map_err(|_| format!("server port {port} is already in use by another mach dev process"))?;
    Ok(file)
}

pub(super) fn ensure_server_port_available(port: u16) -> Result<(), String> {
    let socket = std::net::UdpSocket::bind(("0.0.0.0", port))
        .map_err(|error| format!("server port {port} is unavailable: {error}"))?;
    drop(socket);
    Ok(())
}

pub(super) fn is_source_engine(root: &Path) -> bool {
    root.join("Cargo.toml").is_file() && root.join("src/main.rs").is_file()
}

pub(super) fn platform_id() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("macos-aarch64"),
        ("macos", "x86_64") => Ok("macos-x86_64"),
        ("linux", "x86_64") => Ok("linux-x86_64"),
        ("linux", "aarch64") => Ok("linux-aarch64"),
        ("windows", "x86_64") => Ok("windows-x86_64"),
        (os, arch) => Err(format!("mach does not support {os}-{arch}")),
    }
}

pub(super) const DEPLOY_SERVER_TARGET: &str = "x86_64-unknown-linux-musl";

pub(super) fn host_server_target() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        (os, arch) => Err(format!("mach does not support {os}-{arch}")),
    }
}

fn watch_path(watcher: &mut impl Watcher, path: &Path, mode: RecursiveMode) -> Result<(), String> {
    if path.exists() {
        watcher
            .watch(path, mode)
            .map_err(|error| format!("cannot watch {}: {error}", path.display()))?;
    }
    Ok(())
}

fn configure_project_watcher(
    watcher: &mut impl Watcher,
    project_root: &Path,
    engine_root: &Path,
    developing_engine: bool,
) -> Result<(), String> {
    if developing_engine {
        watch_path(
            watcher,
            &project_root.join("game.json"),
            RecursiveMode::NonRecursive,
        )?;
        watch_path(
            watcher,
            &project_root.join("assets"),
            RecursiveMode::Recursive,
        )?;
        for path in [
            "src",
            "client",
            "shared",
            "crates",
            "Cargo.toml",
            ".cargo",
            "web",
        ] {
            watch_path(watcher, &engine_root.join(path), RecursiveMode::Recursive)?;
        }
    } else {
        watch_path(watcher, project_root, RecursiveMode::Recursive)?;
    }
    Ok(())
}

pub(super) fn ensure_local_build_tools() -> Result<(), String> {
    crate::managed_tools::ensure_base()?;
    let _build_seed = crate::build_seed::prepare_build_seed()?;
    crate::managed_tools::ensure()
}

pub(super) fn ensure_native_build_tools() -> Result<(), String> {
    crate::managed_tools::ensure_native()
}

fn build_native_pair(project_root: &Path, build_jobs: usize) -> Result<(PathBuf, PathBuf), String> {
    let target_dir = cargo_target_dir(project_root)?;
    let started = Instant::now();
    println!("  client    building");
    println!("  server    building");
    let mut build = crate::managed_tools::cargo_command()?;
    build
        .args(["build", "--locked", "--profile", "mach-dev"])
        .args(["--package", "mach", "--bin", "mach"])
        .args(["--package", "game-server", "--bin", "mach-server"])
        .args([
            "--no-default-features",
            "--features",
            "mach/client,mach/browser-webgpu",
        ])
        .env("CARGO_TARGET_DIR", &target_dir)
        .current_dir(project_root);
    configure_cargo_home(&mut build)?;
    configure_cargo_build(&mut build, build_jobs, 1);
    checked(&mut build, "native client and server build failed")?;
    let client = install_native_binary(project_root, &target_dir, "mach", "mach-client")?;
    let server = install_native_binary(project_root, &target_dir, "mach-server", "mach-server")?;
    println!(
        "  native    ready in {}",
        format_duration(started.elapsed())
    );
    Ok((client, server))
}

fn install_native_binary(
    project_root: &Path,
    target_dir: &Path,
    cargo_name: &str,
    installed_name: &str,
) -> Result<PathBuf, String> {
    let output = project_root.join(".mach/bin");
    fs::create_dir_all(&output).map_err(|error| format!("cannot create binary output: {error}"))?;
    let source_name = if cfg!(target_os = "windows") {
        format!("{cargo_name}.exe")
    } else {
        cargo_name.to_owned()
    };
    let executable = if cfg!(target_os = "windows") {
        format!("{installed_name}.exe")
    } else {
        installed_name.to_owned()
    };
    let source = target_dir.join("mach-dev").join(source_name);
    let active = output.join(&executable);
    let candidate = output.join(format!("{executable}.next-{}", std::process::id()));
    fs::copy(&source, &candidate)
        .map_err(|error| format!("cannot install binary from {}: {error}", source.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("cannot make binary executable: {error}"))?;
    }
    activate_validated_file(&candidate, &active)?;
    Ok(active)
}

fn build_native_client(project_root: &Path, build_jobs: usize) -> Result<PathBuf, String> {
    let target_dir = cargo_target_dir(project_root)?;
    let started = Instant::now();
    println!("  client    building");
    let mut client_build = crate::managed_tools::cargo_command()?;
    client_build
        .args(["build", "--locked", "--profile", "mach-dev"])
        .args([
            "--package",
            "mach",
            "--bin",
            "mach",
            "--no-default-features",
            "--features",
            "client,browser-webgpu",
        ])
        .env("CARGO_TARGET_DIR", &target_dir)
        .current_dir(project_root);
    configure_cargo_home(&mut client_build)?;
    configure_cargo_build(&mut client_build, build_jobs, 1);
    checked(&mut client_build, "native client build failed")?;

    let client = install_native_binary(project_root, &target_dir, "mach", "mach-client")?;
    println!(
        "  client    ready in {}",
        format_duration(started.elapsed())
    );
    Ok(client)
}

pub(super) fn build_native_server(
    project_root: &Path,
    build_jobs: usize,
    mode: BrowserBuild,
) -> Result<PathBuf, String> {
    let target_dir = cargo_target_dir(project_root)?;
    let started = Instant::now();
    let profile = match mode {
        BrowserBuild::Development => "mach-dev",
        BrowserBuild::Deployment => "mach-deploy",
    };
    let deploy_target = matches!(mode, BrowserBuild::Deployment).then_some(DEPLOY_SERVER_TARGET);
    if let Some(target) = deploy_target {
        crate::managed_tools::ensure_rust_target(target)?;
        if host_server_target()? != target {
            ensure_zig_builder()?;
        }
    }
    println!("  server    building");
    let mut server_build = crate::managed_tools::cargo_command()?;
    server_build.arg(
        if deploy_target
            .is_some_and(|target| host_server_target().ok().is_some_and(|host| host != target))
        {
            "zigbuild"
        } else {
            "build"
        },
    );
    server_build
        .args(["--locked", "--profile", profile])
        .args([
            "--package",
            "game-server",
            "--bin",
            "mach-server",
            "--no-default-features",
        ])
        .env("CARGO_TARGET_DIR", &target_dir)
        .current_dir(project_root);
    if let Some(target) = deploy_target {
        server_build.args(["--target", target]);
    }
    configure_cargo_home(&mut server_build)?;
    configure_cargo_build(&mut server_build, build_jobs, 1);
    checked(&mut server_build, "authoritative server build failed")?;
    let server = if matches!(mode, BrowserBuild::Development) {
        install_native_binary(project_root, &target_dir, "mach-server", "mach-server")?
    } else {
        let output = project_root.join(".mach/bin");
        fs::create_dir_all(&output)
            .map_err(|error| format!("cannot create server output: {error}"))?;
        let executable = "mach-server";
        let source = target_dir
            .join(DEPLOY_SERVER_TARGET)
            .join(profile)
            .join(executable);
        let server = output.join(executable);
        let candidate = output.join(format!("{executable}.next-{}", std::process::id()));
        fs::copy(&source, &candidate)
            .map_err(|error| format!("cannot install server from {}: {error}", source.display()))?;
        activate_validated_file(&candidate, &server)?;
        server
    };
    println!(
        "  server    ready in {}",
        format_duration(started.elapsed())
    );
    Ok(server)
}

fn ensure_zig_builder() -> Result<(), String> {
    let zig_ready = Command::new("zig")
        .arg("version")
        .output()
        .is_ok_and(|output| output.status.success());
    let mut cargo = crate::managed_tools::cargo_command()?;
    let zigbuild_ready = cargo
        .args(["zigbuild", "--help"])
        .output()
        .is_ok_and(|output| output.status.success());
    if zig_ready && zigbuild_ready {
        return Ok(());
    }
    Err(
        "cross-platform server builds require zig and cargo-zigbuild; install both, then run mach deploy again"
            .to_owned(),
    )
}

fn ensure_certificate(host: &Path, project_root: &Path) -> Result<PathBuf, String> {
    let certificates = project_root.join(".mach/certificates");
    let marker = project_root.join(".mach/certificate-created-at");
    let complete = ["cert.pem", "key.pem", "digest.txt"]
        .iter()
        .all(|name| certificates.join(name).exists());
    let fresh = fs::metadata(&marker)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < CERT_MAX_AGE);
    if complete && fresh {
        return Ok(certificates);
    }

    println!("mach: generating local WebTransport certificate...");
    checked(
        Command::new(host)
            .arg("gen-certs")
            .env("GAME_CERT_DIR", &certificates)
            .current_dir(project_root),
        "certificate generation failed",
    )?;
    fs::create_dir_all(marker.parent().expect("marker has parent"))
        .map_err(|error| format!("cannot create .mach state: {error}"))?;
    fs::write(marker, b"generated by mach dev\n")
        .map_err(|error| format!("cannot record certificate age: {error}"))?;
    Ok(certificates)
}

#[derive(Clone, Copy)]
pub(super) enum BrowserBuild {
    Development,
    Deployment,
}

struct BrowserWebGpuBuild {
    started: Instant,
    wasm_binary: PathBuf,
    web_output: PathBuf,
    target_dir: PathBuf,
}

pub(super) fn build_browser(
    root: &Path,
    build_jobs: usize,
    mode: BrowserBuild,
) -> Result<(), String> {
    let build = compile_browser_webgpu(root, build_jobs, mode)?;
    generate_browser_webgpu(&build)?;

    if matches!(mode, BrowserBuild::Deployment) {
        println!("  client    adding webgl2");
        let mut webgl_build = crate::managed_tools::cargo_command()?;
        webgl_build.args(["rustc", "--locked"]);
        webgl_build.args(["--profile", "mach-deploy"]);
        webgl_build
            .args([
                "--target",
                "wasm32-unknown-unknown",
                "--bin",
                "mach",
                "--no-default-features",
                "--features",
                "client,browser-webgl2",
            ])
            .env("RUSTFLAGS", "--cfg getrandom_backend=\"wasm_js\"")
            .env("CARGO_TARGET_DIR", &build.target_dir)
            .current_dir(root);
        configure_cargo_home(&mut webgl_build)?;
        configure_cargo_build(&mut webgl_build, build_jobs, 1);
        configure_browser_link(&mut webgl_build);
        checked(&mut webgl_build, "WebGL2 browser build failed")?;
        crate::browser_bindgen::generate(&build.wasm_binary, &build.web_output, "mach_webgl2")?;
    }

    report_browser_ready(&build);
    Ok(())
}

fn compile_browser_webgpu(
    root: &Path,
    build_jobs: usize,
    mode: BrowserBuild,
) -> Result<BrowserWebGpuBuild, String> {
    let started = Instant::now();
    let web_output = root.join(".mach/web");
    fs::create_dir_all(&web_output)
        .map_err(|error| format!("cannot create browser output: {error}"))?;
    let target_dir = cargo_target_dir(root)?;
    let profile_dir = match mode {
        BrowserBuild::Development => "mach-dev",
        BrowserBuild::Deployment => "mach-deploy",
    };
    let wasm_binary = target_dir.join(format!("wasm32-unknown-unknown/{profile_dir}/mach.wasm"));
    println!("  client    building");
    let mut webgpu_build = crate::managed_tools::cargo_command()?;
    webgpu_build.args(["rustc", "--locked"]);
    webgpu_build.args([
        "--profile",
        match mode {
            BrowserBuild::Development => "mach-dev",
            BrowserBuild::Deployment => "mach-deploy",
        },
    ]);
    webgpu_build
        .args([
            "--target",
            "wasm32-unknown-unknown",
            "--bin",
            "mach",
            "--no-default-features",
            "--features",
            "client,browser-webgpu",
        ])
        .env("RUSTFLAGS", "--cfg getrandom_backend=\"wasm_js\"")
        .env("CARGO_TARGET_DIR", &target_dir)
        .current_dir(root);
    configure_cargo_home(&mut webgpu_build)?;
    configure_cargo_build(&mut webgpu_build, build_jobs, 1);
    configure_browser_link(&mut webgpu_build);
    let rust_started = Instant::now();
    checked(&mut webgpu_build, "WebGPU browser build failed")?;
    if std::env::var_os("MACH_PROFILE_BUILD").is_some() {
        eprintln!(
            "  profile   rust and wasm link in {:.0}ms",
            rust_started.elapsed().as_secs_f64() * 1000.0,
        );
    }
    Ok(BrowserWebGpuBuild {
        started,
        wasm_binary,
        web_output,
        target_dir,
    })
}

fn generate_browser_webgpu(build: &BrowserWebGpuBuild) -> Result<(), String> {
    let bindgen_started = Instant::now();
    crate::browser_bindgen::generate(&build.wasm_binary, &build.web_output, "mach_webgpu")?;
    if std::env::var_os("MACH_PROFILE_BUILD").is_some() {
        eprintln!(
            "  profile   browser bindings in {:.0}ms",
            bindgen_started.elapsed().as_secs_f64() * 1000.0,
        );
    }
    Ok(())
}

fn report_browser_ready(build: &BrowserWebGpuBuild) {
    println!(
        "  client    ready in {}",
        format_duration(build.started.elapsed())
    );
}

fn configure_browser_link(command: &mut Command) {
    command.args([
        "--",
        "-C",
        "link-arg=--compress-relocations",
        "-C",
        "link-arg=--strip-all",
        "-C",
        "link-arg=--keep-section=__wasm_bindgen_unstable",
        "-C",
        "link-arg=--keep-section=target_features",
        "-C",
        "link-arg=--keep-section=name",
    ]);
}

fn start_game_server(
    server_binary: &Path,
    project_root: &Path,
    certificate_dir: &Path,
    server_port: u16,
) -> Result<GameServerChild, String> {
    println!("mach: starting authoritative server...");
    let mut command = Command::new(server_binary);
    command
        .arg("server")
        .env("GAME_PROJECT_DIR", project_root)
        .env("GAME_CERT_DIR", certificate_dir)
        .env("GAME_SERVER_PORT", server_port.to_string());
    let child = command
        .current_dir(project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("cannot start game server: {error}"))?;
    Ok(GameServerChild { child })
}

fn start_game_client(
    client_binary: &Path,
    project_root: &Path,
    certificate_dir: &Path,
    server_port: u16,
    client_id: u64,
    dev_control_address: &str,
    dev_control_token: &str,
) -> Result<GameServerChild, String> {
    println!("mach: starting native client {client_id}...");
    let child = Command::new(client_binary)
        .arg("client")
        .arg("--client-id")
        .arg(client_id.to_string())
        .env("GAME_PROJECT_DIR", project_root)
        .env("GAME_CERT_DIR", certificate_dir)
        .env("GAME_SERVER_PORT", server_port.to_string())
        .env("MACH_DEV_CONTROL_ADDRESS", dev_control_address)
        .env("MACH_DEV_CONTROL_TOKEN", dev_control_token)
        .current_dir(project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("cannot start native client: {error}"))?;
    Ok(GameServerChild { child })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct DevBuildPlan {
    pub(super) native_client: bool,
    pub(super) native_server: bool,
    pub(super) refresh: bool,
}

impl DevBuildPlan {
    fn is_empty(self) -> bool {
        !self.native_client && !self.native_server && !self.refresh
    }

    fn merge(&mut self, other: Self) {
        self.native_client |= other.native_client;
        self.native_server |= other.native_server;
        self.refresh |= other.refresh;
    }
}

pub(super) fn dev_build_plan(
    paths: &[PathBuf],
    project_root: &Path,
    engine_root: &Path,
    developing_engine: bool,
) -> DevBuildPlan {
    let mut plan = DevBuildPlan::default();
    for path in paths {
        let mut next = DevBuildPlan::default();
        let relative = path.strip_prefix(project_root).unwrap_or(path);
        let generated_web = relative.parent() == Some(Path::new("web"))
            && relative
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mach_"));
        if relative.starts_with(".mach")
            || relative == Path::new("web")
            || relative.starts_with("web/snippets")
            || generated_web
        {
            continue;
        }
        if relative == Path::new("game.json") || relative.starts_with("assets") {
            next.refresh = true;
        }

        if developing_engine && is_engine_source_change(path, engine_root) {
            let relative = path.strip_prefix(engine_root).unwrap_or(path);
            let first = relative.components().next();
            match first {
                Some(Component::Normal(name)) if name == "assets" || name == "web" => {
                    next.refresh = true;
                }
                Some(Component::Normal(name)) if name == "client" => {
                    next.native_client = true;
                }
                Some(Component::Normal(name)) if name == "src" => {
                    next.native_client = true;
                }
                Some(Component::Normal(name))
                    if name == "crates"
                        && relative.starts_with(Path::new("crates/game-server")) =>
                {
                    next.native_server = true;
                }
                Some(Component::Normal(name))
                    if name == "crates" && relative.starts_with(Path::new("crates/game-core")) =>
                {
                    next.native_client = true;
                    next.native_server = true;
                }
                Some(Component::Normal(name))
                    if name == "crates"
                        && (relative.starts_with(Path::new("crates/game-client"))
                            || relative.starts_with(Path::new("crates/game-format"))
                            || relative.starts_with(Path::new("crates/render-api"))
                            || relative.starts_with(Path::new("crates/render-fn"))) =>
                {
                    next.native_client = true;
                }
                Some(Component::Normal(name)) if name == "crates" => {
                    next.native_client = true;
                    next.native_server = true;
                }
                _ => {
                    next.native_client = true;
                    next.native_server = true;
                }
            }
        }
        plan.merge(next);
    }
    plan
}

fn is_engine_source_change(path: &Path, engine_root: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(engine_root) else {
        return false;
    };
    matches!(
        relative.components().next(),
        Some(Component::Normal(name))
            if name == "src"
                || name == "client"
                || name == "shared"
                || name == "crates"
                || name == "Cargo.toml"
                || name == "Cargo.lock"
                || name == ".cargo"
                || name == "web"
                || name == "assets"
    )
}

pub(super) fn open_browser(url: &str) {
    let result = if cfg!(target_os = "macos") {
        Command::new("open").arg(url).status()
    } else if cfg!(target_os = "windows") {
        Command::new("cmd").args(["/C", "start", "", url]).status()
    } else {
        Command::new("xdg-open").arg(url).status()
    };
    if result.is_err() {
        eprintln!("mach: could not open the browser; visit {url}");
    }
}

pub(super) fn checked(command: &mut Command, context: &str) -> Result<(), String> {
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("{context}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{context} ({status})"))
    }
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub(super) struct GameServerChild {
    pub(super) child: Child,
}

impl GameServerChild {
    pub(super) fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    pub(super) fn stop(&mut self) {
        stop_child(&mut self.child);
    }
}

impl Drop for GameServerChild {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mach-cli-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("test clock after epoch")
                .as_nanos()
        ))
    }

    #[test]
    fn concurrent_builds_split_the_job_budget() {
        assert_eq!(concurrent_build_jobs(1), (1, 1));
        assert_eq!(concurrent_build_jobs(2), (1, 1));
        assert_eq!(concurrent_build_jobs(3), (2, 1));
        assert_eq!(concurrent_build_jobs(4), (3, 1));
        assert_eq!(concurrent_build_jobs(12), (10, 2));
    }

    #[test]
    fn running_dev_session_answers_open_requests() {
        let root = test_root("dev-open");
        let session_lock = lock_dev_session(&root).expect("lock dev session");
        let (server, commands) =
            DevControlServer::start(&root, session_lock, "test-control-token".to_owned())
                .expect("start dev control");
        let request_root = root.clone();
        let request = std::thread::spawn(move || request_dev_open(&request_root));

        let command = commands
            .recv_timeout(Duration::from_secs(2))
            .expect("receive open request");
        let DevControlCommand::OpenClient { response } = command else {
            panic!("expected open request");
        };
        response
            .send(DevControlResponse::opened(2))
            .expect("answer open request");

        assert_eq!(request.join().expect("join open request"), Ok(2));
        let descriptor = dev_session_path(&root);
        assert!(descriptor.exists());
        drop(server);
        assert!(!descriptor.exists());
        fs::remove_dir_all(&root).expect("remove test directory");
    }

    #[test]
    fn dev_open_reports_missing_session() {
        let root = test_root("missing-dev-open");
        fs::create_dir_all(&root).expect("create test directory");

        assert_eq!(
            request_dev_open(&root),
            Err("no dev session is running for this project".to_owned())
        );

        fs::remove_dir_all(&root).expect("remove test directory");
    }

    #[test]
    fn dev_open_clears_stale_session() {
        let root = test_root("stale-dev-open");
        let session_lock = lock_dev_session(&root).expect("create mach directory");
        drop(session_lock);
        let unused_listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind test port");
        let unused_address = unused_listener.local_addr().expect("read test port");
        drop(unused_listener);
        let descriptor = dev_session_path(&root);
        fs::write(
            &descriptor,
            serde_json::to_vec(&DevSessionDescriptor {
                version: DEV_SESSION_VERSION,
                pid: 1,
                address: unused_address.to_string(),
            })
            .expect("encode stale session"),
        )
        .expect("write stale session");

        assert_eq!(
            request_dev_open(&root),
            Err("no dev session is running for this project".to_owned())
        );
        assert!(!descriptor.exists());

        fs::remove_dir_all(&root).expect("remove test directory");
    }

    #[test]
    fn project_allows_one_dev_session() {
        let root = test_root("dev-session-lock");
        let first = lock_dev_session(&root).expect("lock first dev session");

        assert_eq!(
            lock_dev_session(&root).expect_err("reject second dev session"),
            "mach dev is already running for this project"
        );

        drop(first);
        lock_dev_session(&root).expect("reuse released dev session lock");
        fs::remove_dir_all(&root).expect("remove test directory");
    }
}
