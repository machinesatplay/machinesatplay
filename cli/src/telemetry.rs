use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const SCHEMA_VERSION: u8 = 1;
const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
const MAX_BATCH_EVENTS: usize = 16;
const MAX_PENDING_SENDS: usize = 4;
const DEFAULT_ENDPOINT: &str = "https://machinesatplay.com/api/metrics/events";
const TELEMETRY_FILE_ENV: &str = "MACH_TELEMETRY_FILE";

static STATE: OnceLock<TelemetryState> = OnceLock::new();

struct TelemetryState {
    installation_id: String,
    invocation_id: String,
    context: EventContext,
    root: PathBuf,
    events: Mutex<Vec<TelemetryEvent>>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallationState {
    schema_version: u8,
    installation_id: String,
    created_at_ms: u64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EventContext {
    cli_version: &'static str,
    engine_version: &'static str,
    os: &'static str,
    os_major: u16,
    architecture: &'static str,
    execution_context: String,
    coding_agent: String,
    agent_candidates: Vec<String>,
    host_environment: String,
    detection_markers: Vec<String>,
    detection_confidence: String,
    is_ci: bool,
    is_interactive: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EventEnvelope<'a> {
    schema_version: u8,
    installation_id: &'a str,
    invocation_id: &'a str,
    sent_at_ms: u64,
    events: &'a [TelemetryEvent],
    context: &'a EventContext,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TelemetryEvent {
    event_id: String,
    name: &'static str,
    occurred_at_ms: u64,
    properties: Value,
}

#[derive(Debug, PartialEq, Eq)]
struct AgentDetection {
    execution_context: String,
    coding_agent: String,
    agent_candidates: Vec<String>,
    host_environment: String,
    detection_markers: Vec<String>,
    detection_confidence: String,
    is_ci: bool,
    is_interactive: bool,
}

pub(crate) struct Invocation {
    command: &'static str,
    started: Instant,
    enabled: bool,
}

impl Invocation {
    pub(crate) fn start(command: &'static str) -> Self {
        let enabled = initialize();
        Self {
            command,
            started: Instant::now(),
            enabled,
        }
    }

    pub(crate) fn finish(self, result: &Result<(), String>) {
        if !self.enabled {
            return;
        }
        if self.command == "dev" && result.is_ok() {
            flush();
            return;
        }
        let properties = match result {
            Ok(()) => json!({
                "command": self.command,
                "result": "success",
                "durationMs": duration_ms(self.started.elapsed()),
                "updatedCli": false,
            }),
            Err(error) => json!({
                "command": self.command,
                "result": "error",
                "durationMs": duration_ms(self.started.elapsed()),
                "errorCode": classify_error(error),
                "updatedCli": false,
            }),
        };
        record("command_completed", properties);
        flush();
    }
}

pub(crate) fn project_created(duration: Duration) {
    record(
        "project_created",
        json!({
            "starterVersion": super::ENGINE_VERSION,
            "durationMs": duration_ms(duration),
        }),
    );
}

pub(crate) fn setup_summary(result: &str, duration: Duration, build_seed: &str) {
    record(
        "setup_summary",
        json!({
            "result": result,
            "durationMs": duration_ms(duration),
            "starterBundle": "hit",
            "buildSeed": build_seed,
        }),
    );
}

pub(crate) fn update_summary(from_version: &str, to_version: &str, duration: Duration) {
    record(
        "update_summary",
        json!({
            "result": "success",
            "fromVersion": from_version,
            "toVersion": to_version,
            "durationMs": duration_ms(duration),
        }),
    );
}

pub(crate) fn dev_ready(
    session_id: &str,
    duration: Duration,
    starter: bool,
    initial_client_opened: bool,
) {
    record(
        "dev_ready",
        json!({
            "devSessionId": session_id,
            "durationMs": duration_ms(duration),
            "starterPath": if starter { "prebuilt_bundle" } else { "local_build" },
            "cacheState": if starter { "background" } else { "ready" },
            "initialClientOpened": initial_client_opened,
        }),
    );
    flush();
}

pub(crate) fn dev_world_joined(session_id: &str, duration: Duration, launch_source: &str) {
    record(
        "dev_world_joined",
        json!({
            "devSessionId": session_id,
            "durationMs": duration_ms(duration),
            "launchSource": launch_source,
        }),
    );
    flush();
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn dev_session_summary(
    session_id: &str,
    duration: Duration,
    clients_opened: u64,
    clients_joined: u64,
    rebuild_attempts: u64,
    rebuild_successes: u64,
    rebuild_failures: u64,
) {
    record(
        "dev_session_summary",
        json!({
            "devSessionId": session_id,
            "result": "stopped",
            "durationMs": duration_ms(duration),
            "clientsOpened": clients_opened,
            "clientsJoined": clients_joined,
            "rebuildAttempts": rebuild_attempts,
            "rebuildSuccesses": rebuild_successes,
            "rebuildFailures": rebuild_failures,
        }),
    );
}

pub(crate) fn installation_id() -> Option<String> {
    STATE.get().map(|state| state.installation_id.clone())
}

fn initialize() -> bool {
    if !telemetry_enabled() {
        return false;
    }
    if STATE.get().is_some() {
        return true;
    }
    let Some(root) = telemetry_root() else {
        return false;
    };
    let Ok((installation, created)) = load_or_create_installation(&root) else {
        return false;
    };
    let detection = detect_agent_environment(&current_environment(), terminal_is_interactive());
    let state = TelemetryState {
        installation_id: installation.installation_id,
        invocation_id: Uuid::new_v4().to_string(),
        context: EventContext {
            cli_version: env!("CARGO_PKG_VERSION"),
            engine_version: super::ENGINE_VERSION,
            os: std::env::consts::OS,
            os_major: 0,
            architecture: std::env::consts::ARCH,
            execution_context: detection.execution_context,
            coding_agent: detection.coding_agent,
            agent_candidates: detection.agent_candidates,
            host_environment: detection.host_environment,
            detection_markers: detection.detection_markers,
            detection_confidence: detection.detection_confidence,
            is_ci: detection.is_ci,
            is_interactive: detection.is_interactive,
        },
        root,
        events: Mutex::new(Vec::new()),
    };
    if STATE.set(state).is_err() {
        return true;
    }
    if created {
        record(
            "installation_created",
            json!({
                "installSource": "unknown",
                "cliVersion": env!("CARGO_PKG_VERSION"),
            }),
        );
    }
    true
}

fn telemetry_enabled() -> bool {
    option_env!("MACH_OFFICIAL_RELEASE") == Some("1")
        || std::env::var_os("MACH_TELEMETRY_TEST").is_some()
}

fn record(name: &'static str, properties: Value) {
    if STATE.get().is_none() && !initialize() {
        return;
    }
    let Some(state) = STATE.get() else {
        return;
    };
    let Ok(mut events) = state.events.lock() else {
        return;
    };
    events.push(TelemetryEvent {
        event_id: Uuid::new_v4().to_string(),
        name,
        occurred_at_ms: now_ms(),
        properties,
    });
    let should_flush = events.len() >= MAX_BATCH_EVENTS;
    drop(events);
    if should_flush {
        flush();
    }
}

pub(crate) fn flush() {
    let Some(state) = STATE.get() else {
        return;
    };
    let Ok(mut queued) = state.events.lock() else {
        return;
    };
    if queued.is_empty() {
        return;
    }
    let events = std::mem::take(&mut *queued);
    drop(queued);

    for batch in events.chunks(MAX_BATCH_EVENTS) {
        write_batch(state, batch);
    }
}

fn write_batch(state: &TelemetryState, events: &[TelemetryEvent]) {
    let batch_id = Uuid::new_v4().to_string();
    let envelope = EventEnvelope {
        schema_version: SCHEMA_VERSION,
        installation_id: &state.installation_id,
        invocation_id: &state.invocation_id,
        sent_at_ms: now_ms(),
        events,
        context: &state.context,
    };
    let Ok(payload) = serde_json::to_vec(&envelope) else {
        return;
    };
    if payload.len() > MAX_PAYLOAD_BYTES {
        return;
    }
    let pending = state.root.join("pending");
    if fs::create_dir_all(&pending).is_err() {
        return;
    }
    let candidate = pending.join(format!("{batch_id}.next"));
    let destination = pending.join(format!("{batch_id}.json"));
    if fs::write(&candidate, payload).is_err() || fs::rename(&candidate, &destination).is_err() {
        let _ = fs::remove_file(candidate);
        return;
    }
    spawn_sender(&destination);
}

fn spawn_sender(path: &Path) {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let mut command = Command::new(executable);
    command
        .arg("send-telemetry")
        .env("MACH_SKIP_UPDATE", "1")
        .env(TELEMETRY_FILE_ENV, path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let _ = command.spawn();
}

pub(crate) fn send_pending_file() {
    if !telemetry_enabled() {
        return;
    }
    let Some(path) = std::env::var_os(TELEMETRY_FILE_ENV).map(PathBuf::from) else {
        return;
    };
    let mut paths = vec![path.clone()];
    if let Some(parent) = path.parent() {
        if let Ok(entries) = fs::read_dir(parent) {
            paths.extend(
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|candidate| {
                        candidate != &path
                            && candidate.extension().and_then(|value| value.to_str())
                                == Some("json")
                    })
                    .take(MAX_PENDING_SENDS.saturating_sub(1)),
            );
        }
    }
    let endpoint = telemetry_endpoint();
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(1))
        .timeout_read(Duration::from_secs(2))
        .timeout_write(Duration::from_secs(2))
        .build();
    for path in paths {
        if !send_file(&agent, &endpoint, &path) {
            break;
        }
    }
}

fn send_file(agent: &ureq::Agent, endpoint: &str, path: &Path) -> bool {
    let payload = fs::File::open(path).and_then(|file| {
        let mut payload = Vec::new();
        file.take((MAX_PAYLOAD_BYTES + 1) as u64)
            .read_to_end(&mut payload)?;
        Ok(payload)
    });
    let Ok(payload) = payload else {
        return false;
    };
    if payload.len() > MAX_PAYLOAD_BYTES {
        let _ = fs::remove_file(path);
        return true;
    }
    let sent = agent
        .post(endpoint)
        .set("content-type", "application/json")
        .set("accept", "application/json")
        .set("x-mach-client", "cli")
        .set("user-agent", &format!("mach/{}", env!("CARGO_PKG_VERSION")))
        .send_bytes(&payload)
        .is_ok();
    if sent {
        let _ = fs::remove_file(path);
        true
    } else {
        false
    }
}

fn telemetry_endpoint() -> String {
    if std::env::var_os("MACH_TELEMETRY_TEST").is_some() {
        if let Ok(endpoint) = std::env::var("MACH_TELEMETRY_ENDPOINT") {
            return endpoint;
        }
    }
    DEFAULT_ENDPOINT.to_owned()
}

fn telemetry_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("MACH_TELEMETRY_DIR") {
        return Some(PathBuf::from(root));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".mach").join("telemetry"))
}

fn load_or_create_installation(root: &Path) -> Result<(InstallationState, bool), String> {
    let mach_root = root
        .parent()
        .ok_or_else(|| "telemetry directory has no parent".to_owned())?;
    fs::create_dir_all(mach_root).map_err(|error| error.to_string())?;
    let path = mach_root.join("install.json");
    if let Some(state) = read_installation(&path) {
        return Ok((state, false));
    }
    if path.exists() {
        let _ = fs::remove_file(&path);
    }
    let state = InstallationState {
        schema_version: SCHEMA_VERSION,
        installation_id: Uuid::new_v4().to_string(),
        created_at_ms: now_ms(),
    };
    let bytes = serde_json::to_vec(&state).map_err(|error| error.to_string())?;
    let candidate = mach_root.join(format!("install-{}.next", Uuid::new_v4()));
    fs::write(&candidate, bytes).map_err(|error| error.to_string())?;
    match fs::hard_link(&candidate, &path) {
        Ok(()) => {
            let _ = fs::remove_file(candidate);
            Ok((state, true))
        }
        Err(_) => {
            let _ = fs::remove_file(candidate);
            read_installation(&path)
                .map(|winner| (winner, false))
                .ok_or_else(|| "cannot create installation identity".to_owned())
        }
    }
}

fn read_installation(path: &Path) -> Option<InstallationState> {
    let bytes = fs::read(path).ok()?;
    let state = serde_json::from_slice::<InstallationState>(&bytes).ok()?;
    (state.schema_version == SCHEMA_VERSION && Uuid::parse_str(&state.installation_id).is_ok())
        .then_some(state)
}

fn current_environment() -> BTreeMap<String, String> {
    std::env::vars().collect()
}

fn terminal_is_interactive() -> bool {
    std::io::stdin().is_terminal() || std::io::stdout().is_terminal()
}

fn detect_agent_environment(
    env: &BTreeMap<String, String>,
    is_interactive: bool,
) -> AgentDetection {
    let mut matches = Vec::<(&str, &str, &str)>::new();
    let mut markers = BTreeSet::new();

    if let Some(value) = env
        .get("AI_AGENT")
        .map(|value| value.trim().to_ascii_lowercase())
    {
        let agent = match value.as_str() {
            "codex" => "codex",
            "claude" | "claude-code" => "claude_code",
            "cursor" | "cursor-cli" => "cursor_agent",
            "gemini" | "gemini-cli" => "gemini",
            "github-copilot" | "github-copilot-cli" => "copilot",
            "opencode" => "opencode",
            "replit" => "replit",
            "auggie" => "auggie",
            "antigravity" => "antigravity",
            "pi" => "pi",
            "crush" => "crush",
            "amp" => "amp",
            "qwen-code" => "qwen_code",
            "windsurf" => "windsurf",
            _ if !value.is_empty() => "other",
            _ => "",
        };
        if !agent.is_empty() {
            matches.push((agent, "explicit", "ai_agent"));
        }
    }

    let direct = [
        ("codex", "CODEX_THREAD_ID", None, "codex_thread_id"),
        ("codex", "CODEX_SANDBOX", None, "codex_sandbox"),
        ("codex", "CODEX_CI", None, "codex_ci"),
        ("claude_code", "CLAUDECODE", None, "claudecode"),
        ("claude_code", "CLAUDE_CODE", None, "claude_code"),
        ("cursor_agent", "CURSOR_AGENT", None, "cursor_agent"),
        (
            "cursor_agent",
            "CURSOR_EXTENSION_HOST_ROLE",
            Some("agent-exec"),
            "cursor_extension_agent",
        ),
        ("gemini", "GEMINI_CLI", None, "gemini_cli"),
        ("copilot", "COPILOT_MODEL", None, "copilot_model"),
        ("copilot", "COPILOT_ALLOW_ALL", None, "copilot_allow_all"),
        (
            "copilot",
            "COPILOT_GITHUB_TOKEN",
            None,
            "copilot_github_token",
        ),
        ("opencode", "OPENCODE", None, "opencode"),
        ("opencode", "OPENCODE_CLIENT", None, "opencode_client"),
        ("opencode", "OPENCODE_SERVER", None, "opencode_server"),
        ("replit", "REPL_ID", None, "repl_id"),
        ("auggie", "AUGMENT_AGENT", None, "augment_agent"),
        (
            "antigravity",
            "ANTIGRAVITY_AGENT",
            None,
            "antigravity_agent",
        ),
        (
            "antigravity",
            "ANTIGRAVITY_PROJECT_ID",
            None,
            "antigravity_project_id",
        ),
        ("pi", "PI_CODING_AGENT", Some("true"), "pi_coding_agent"),
        ("crush", "CRUSH", Some("1"), "crush"),
        (
            "amp",
            "AMP_CURRENT_THREAD_ID",
            None,
            "amp_current_thread_id",
        ),
        ("qwen_code", "QWEN_CODE", Some("1"), "qwen_code"),
        (
            "windsurf",
            "CODEIUM_EDITOR_APP_ROOT",
            None,
            "codeium_editor_app_root",
        ),
    ];
    for (agent, key, expected, marker) in direct {
        let matched = env.get(key).is_some_and(|value| {
            !value.is_empty() && expected.is_none_or(|expected| value == expected)
        });
        if matched {
            matches.push((agent, "direct", marker));
        }
    }
    for (agent, expected) in [("crush", "crush"), ("amp", "amp")] {
        if env.get("AGENT").is_some_and(|value| value == expected) {
            matches.push((agent, "direct", "agent"));
        }
    }

    let mut candidates = BTreeSet::new();
    for (agent, _, marker) in &matches {
        candidates.insert((*agent).to_owned());
        markers.insert((*marker).to_owned());
    }
    let primary = matches
        .iter()
        .find(|(_, confidence, _)| *confidence == "explicit")
        .or_else(|| matches.first());
    let (coding_agent, confidence) = primary
        .map(|(agent, confidence, _)| ((*agent).to_owned(), (*confidence).to_owned()))
        .unwrap_or_else(|| ("unknown".to_owned(), "none".to_owned()));

    let host_environment = if env
        .get("CURSOR_TRACE_ID")
        .is_some_and(|value| !value.is_empty())
    {
        markers.insert("cursor_trace_id".to_owned());
        "cursor"
    } else if env
        .get("TERM_PROGRAM")
        .is_some_and(|value| value == "vscode")
    {
        markers.insert("term_program_vscode".to_owned());
        "vscode"
    } else if env.get("REPL_ID").is_some_and(|value| !value.is_empty()) {
        "replit"
    } else if is_interactive {
        "terminal"
    } else {
        "unknown"
    }
    .to_owned();
    let is_ci = env.get("CI").is_some_and(|value| !value.is_empty());
    let execution_context = if coding_agent != "unknown" {
        "coding_agent"
    } else if is_ci {
        "ci"
    } else if host_environment != "terminal" && host_environment != "unknown" {
        "interactive_ai_ide"
    } else if is_interactive {
        "interactive_shell"
    } else {
        "unknown_noninteractive"
    }
    .to_owned();

    AgentDetection {
        execution_context,
        coding_agent,
        agent_candidates: candidates.into_iter().take(4).collect(),
        host_environment,
        detection_markers: markers.into_iter().collect(),
        detection_confidence: confidence,
        is_ci,
        is_interactive,
    }
}

fn classify_error(error: &str) -> &'static str {
    let error = error.to_ascii_lowercase();
    if error.contains("not signed in") || error.contains("authentication required") {
        "auth_required"
    } else if error.contains("checksum") || error.contains("manifest") {
        "cache_manifest"
    } else if error.contains("download") || error.contains("network") || error.contains("http") {
        "network"
    } else if error.contains("certificate") {
        "certificate"
    } else if error.contains("client") && error.contains("build") {
        "build_client"
    } else if error.contains("server") && error.contains("build") {
        "build_server"
    } else if error.contains("invalid") || error.contains("missing") {
        "invalid_project"
    } else {
        "unknown"
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(values: &[(&str, &str)]) -> BTreeMap<String, String> {
        values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn detects_agent_and_host_without_values() {
        let detection = detect_agent_environment(
            &environment(&[
                ("CODEX_THREAD_ID", "private-session-id"),
                ("CURSOR_TRACE_ID", "private-cursor-id"),
            ]),
            false,
        );
        assert_eq!(detection.coding_agent, "codex");
        assert_eq!(detection.host_environment, "cursor");
        assert_eq!(detection.agent_candidates, ["codex"]);
        assert_eq!(
            detection.detection_markers,
            ["codex_thread_id", "cursor_trace_id"]
        );
        let encoded = serde_json::to_string(&detection.detection_markers).unwrap();
        assert!(!encoded.contains("private"));
    }

    #[test]
    fn explicit_agent_wins_and_all_candidates_remain() {
        let detection = detect_agent_environment(
            &environment(&[
                ("AI_AGENT", "claude-code"),
                ("CODEX_THREAD_ID", "thread"),
                ("CI", "1"),
            ]),
            false,
        );
        assert_eq!(detection.coding_agent, "claude_code");
        assert_eq!(detection.detection_confidence, "explicit");
        assert_eq!(detection.agent_candidates, ["claude_code", "codex"]);
        assert!(detection.is_ci);
    }

    #[test]
    fn unknown_explicit_agent_is_bounded() {
        let detection = detect_agent_environment(
            &environment(&[("AI_AGENT", "private-custom-agent-name")]),
            false,
        );
        assert_eq!(detection.coding_agent, "other");
        assert_eq!(detection.agent_candidates, ["other"]);
        assert_eq!(detection.detection_markers, ["ai_agent"]);
    }
}
