use super::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SavedAuth {
    api_url: String,
    access_token: String,
    user_email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    #[serde(default = "default_device_poll_interval")]
    interval: u64,
    #[serde(default = "default_device_expiry")]
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateDeploymentResponse {
    deployment_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrepareDeploymentResponse {
    missing: Vec<String>,
    upload_base: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FinalizeDeploymentResponse {
    deployment_id: String,
    slug: String,
    url: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeploymentFileManifest {
    path: String,
    size: u64,
    sha256: String,
    content_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_encoding: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeploymentManifest {
    schema_version: u32,
    engine_version: String,
    server_target: String,
    files: Vec<DeploymentFileManifest>,
}

enum ArtifactSource {
    File(PathBuf),
    Bytes(Vec<u8>),
}

struct PreparedDeploymentArtifact {
    file: DeploymentFileManifest,
    data: Vec<u8>,
}

fn default_device_poll_interval() -> u64 {
    5
}

fn default_device_expiry() -> u64 {
    30 * 60
}

fn api_base_url() -> String {
    std::env::var("MACH_API_URL")
        .unwrap_or_else(|_| DEFAULT_API_URL.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

fn auth_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("MACH_AUTH_FILE") {
        return Ok(PathBuf::from(path));
    }
    mach_cache_root()
        .map(|root| root.join("auth.json"))
        .ok_or_else(|| "cannot locate the user configuration directory".to_owned())
}

fn load_saved_auth() -> Result<Option<SavedAuth>, String> {
    let path = auth_path()?;
    let data = match fs::read_to_string(&path) {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    serde_json::from_str(&data)
        .map(Some)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))
}

fn save_auth(auth: &SavedAuth) -> Result<(), String> {
    let path = auth_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    fs::write(
        &path,
        serde_json::to_vec(auth).map_err(|error| format!("cannot serialize login: {error}"))?,
    )
    .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("cannot protect {}: {error}", path.display()))?;
    }
    Ok(())
}

fn current_token() -> Result<Option<String>, String> {
    if let Ok(token) = std::env::var("MACH_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(Some(token));
        }
    }
    let api_url = api_base_url();
    Ok(load_saved_auth()?
        .filter(|auth| auth.api_url == api_url)
        .map(|auth| auth.access_token))
}

pub(super) fn login_command(no_open: bool) -> Result<String, String> {
    let api_url = api_base_url();
    let response: DeviceCodeResponse = send_json(
        ureq::post(&format!("{api_url}/api/auth/device/code")),
        &serde_json::json!({
            "client_id": CLI_AUTH_CLIENT_ID,
            "scope": "openid profile email",
        }),
        "cannot start CLI login",
    )?;
    let browser_url = response
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&response.verification_uri);
    println!("mach: sign in at {}", response.verification_uri);
    println!("mach: code {}", response.user_code);
    if !no_open {
        open_browser(browser_url);
    }
    println!("mach: waiting for authorization...");

    let started = Instant::now();
    let mut interval = response.interval.max(1);
    loop {
        if started.elapsed() >= Duration::from_secs(response.expires_in) {
            return Err("login code expired; run `mach login` again".to_owned());
        }
        std::thread::sleep(Duration::from_secs(interval));
        let request = ureq::post(&format!("{api_url}/api/auth/device/token"))
            .set("accept", "application/json")
            .set("content-type", "application/json");
        let body = serde_json::json!({
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            "device_code": response.device_code,
            "client_id": CLI_AUTH_CLIENT_ID,
        });
        match request.send_string(&body.to_string()) {
            Ok(http_response) => {
                let token: DeviceTokenResponse =
                    read_json(http_response, "invalid login response")?;
                let auth = SavedAuth {
                    api_url,
                    access_token: token.access_token.clone(),
                    user_email: None,
                };
                save_auth(&auth)?;
                println!("mach: signed in");
                return Ok(token.access_token);
            }
            Err(ureq::Error::Status(status, http_response)) => {
                let response_body = http_response
                    .into_string()
                    .map_err(|error| format!("cannot read login response: {error}"))?;
                if response_body.trim().is_empty() {
                    if status == 429 {
                        interval += 5;
                        continue;
                    }
                    return Err(format!("login failed: HTTP {status}"));
                }
                let value: serde_json::Value =
                    serde_json::from_str(&response_body).map_err(|error| {
                        format!("login failed: invalid HTTP {status} response: {error}")
                    })?;
                match value.get("error").and_then(serde_json::Value::as_str) {
                    Some("authorization_pending") => {}
                    Some("slow_down") => interval += 5,
                    Some("access_denied") => return Err("login was denied".to_owned()),
                    Some("expired_token") => return Err("login code expired".to_owned()),
                    Some(error) => return Err(format!("login failed: {error}")),
                    None => return Err("login failed".to_owned()),
                }
            }
            Err(error) => return Err(format!("cannot check CLI login: {error}")),
        }
    }
}

pub(super) fn logout_command() -> Result<(), String> {
    if std::env::var_os("MACH_TOKEN").is_some() {
        println!("mach: MACH_TOKEN is set in the environment; remove it to sign out");
        return Ok(());
    }
    let path = auth_path()?;
    match fs::remove_file(&path) {
        Ok(()) => println!("mach: signed out"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            println!("mach: already signed out")
        }
        Err(error) => return Err(format!("cannot remove {}: {error}", path.display())),
    }
    Ok(())
}

pub(super) fn whoami_command() -> Result<(), String> {
    let token = current_token()?.ok_or_else(|| "not signed in; run `mach login`".to_owned())?;
    let api_url = api_base_url();
    let response = with_cli_identity(ureq::get(&format!("{api_url}/api/auth/get-session")))
        .set("authorization", &format!("Bearer {token}"))
        .set("accept", "application/json")
        .call()
        .map_err(|error| format!("cannot read account: {error}"))?;
    let value: serde_json::Value = read_json(response, "invalid account response")?;
    let user = value
        .get("user")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "session expired; run `mach login` again".to_owned())?;
    let identity = user
        .get("email")
        .or_else(|| user.get("name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("signed in");
    println!("{identity}");
    Ok(())
}

pub(super) fn authenticated_token() -> Result<String, String> {
    match current_token()? {
        Some(token) => Ok(token),
        None => login_command(false),
    }
}

pub(super) fn deploy_command(directory: &Path, no_build: bool, json: bool) -> Result<(), String> {
    let _cache_lock = lock_cache_shared()?;
    let project = load_project(directory).map_err(|error| error.to_string())?;
    ensure_valid_project(&project)?;
    if !is_source_engine(&project.root) {
        return Err("this project is missing its full browser client source".to_owned());
    }
    let token = authenticated_token()?;
    let artifact_engine_root;
    let server_binary = if no_build {
        artifact_engine_root = project.root.clone();
        let server = project.root.join(".mach/bin/mach-server");
        if !server.is_file() {
            return Err(
                "required server binary is missing; run mach deploy without --no-build".to_owned(),
            );
        }
        server
    } else {
        artifact_engine_root = project.root.clone();
        ensure_local_build_tools()?;
        local_creator_build(&project.root, build_job_budget()?, BrowserBuild::Deployment)?
    };
    validate_engine_root(&artifact_engine_root)?;

    let artifact_started = Instant::now();
    let artifacts = deployment_artifacts(&project, &artifact_engine_root, &server_binary)?;
    let prepared = prepare_deployment_artifacts(artifacts)?;
    if !json {
        println!(
            "mach: artifacts ready in {}",
            crate::dev::format_duration(artifact_started.elapsed())
        );
    }
    let manifest = DeploymentManifest {
        schema_version: 4,
        engine_version: project.manifest.engine_version.clone(),
        server_target: if no_build {
            host_server_target()?.to_owned()
        } else {
            DEPLOY_SERVER_TARGET.to_owned()
        },
        files: prepared
            .iter()
            .map(|artifact| artifact.file.clone())
            .collect(),
    };

    let slug = deployment_slug(&project.manifest.name);
    let api_url = api_base_url();
    if !json {
        println!("mach: creating deployment for {slug}...");
    }
    let created: CreateDeploymentResponse = send_authenticated_json(
        "POST",
        &format!("{api_url}/api/deployments"),
        &token,
        &serde_json::json!({
            "slug": slug,
            "name": project.manifest.name,
            "engineVersion": project.manifest.engine_version,
        }),
        "cannot create deployment",
    )?;

    let upload_plan: PrepareDeploymentResponse = send_authenticated_json(
        "POST",
        &format!(
            "{api_url}/api/deployments/{}/prepare",
            created.deployment_id
        ),
        &token,
        &manifest,
        "cannot prepare deployment",
    )?;
    let mut missing = upload_plan.missing.into_iter().collect::<HashSet<_>>();
    if !json {
        println!(
            "mach: uploading {} changed files ({} total)...",
            missing.len(),
            prepared.len()
        );
    }
    for artifact in &prepared {
        if !missing.remove(&artifact.file.sha256) {
            continue;
        }
        let upload_url = absolute_url(
            &api_url,
            &format!("{}{}", upload_plan.upload_base, artifact.file.sha256),
        );
        upload_file(
            &upload_url,
            &token,
            &artifact.data,
            &artifact.file.sha256,
            &artifact.file.content_type,
            artifact.file.content_encoding.as_deref(),
        )?;
    }
    if !missing.is_empty() {
        return Err("deployment requested unknown artifact hashes".to_owned());
    }
    let finalized: FinalizeDeploymentResponse = send_authenticated_json(
        "POST",
        &format!(
            "{api_url}/api/deployments/{}/finalize",
            created.deployment_id
        ),
        &token,
        &manifest,
        "cannot finalize deployment",
    )?;
    let live_url = absolute_url(&api_url, &finalized.url);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "deploymentId": finalized.deployment_id,
                "slug": finalized.slug,
                "url": live_url,
            })
        );
    } else {
        println!("mach: live at {live_url}");
    }
    Ok(())
}

pub(super) fn deployments_command(json_output: bool) -> Result<(), String> {
    let token = authenticated_token()?;
    let api_url = api_base_url();
    let response = with_cli_identity(ureq::get(&format!("{api_url}/api/deployments")))
        .set("authorization", &format!("Bearer {token}"))
        .set("accept", "application/json")
        .call()
        .map_err(render_http_error("cannot list deployments"))?;
    let value: serde_json::Value = read_json(response, "invalid deployment response")?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).expect("deployment response serializes")
        );
        return Ok(());
    }
    let deployments = value
        .get("deployments")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    if deployments.is_empty() {
        println!("no deployments");
    }
    for deployment in deployments {
        println!(
            "{}  {}  {}{}",
            deployment
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown"),
            deployment
                .get("slug")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown"),
            deployment
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown"),
            if deployment.get("id") == deployment.get("currentDeploymentId") {
                "  current"
            } else {
                ""
            }
        );
    }
    Ok(())
}

pub(super) fn rollback_command(slug: &str, deployment: &str) -> Result<(), String> {
    let token = authenticated_token()?;
    let api_url = api_base_url();
    let slug = deployment_slug(slug);
    let response: FinalizeDeploymentResponse = send_authenticated_json(
        "POST",
        &format!("{api_url}/api/games/{slug}/rollback"),
        &token,
        &serde_json::json!({ "deploymentId": deployment }),
        "cannot roll back deployment",
    )?;
    println!("mach: live at {}", absolute_url(&api_url, &response.url));
    Ok(())
}

fn deployment_slug(name: &str) -> String {
    name.chars()
        .map(|character| match character {
            'A'..='Z' => character.to_ascii_lowercase(),
            'a'..='z' | '0'..='9' | '-' => character,
            _ => '-',
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

fn deployment_artifacts(
    project: &ProjectFiles,
    engine_root: &Path,
    server_binary: &Path,
) -> Result<BTreeMap<String, ArtifactSource>, String> {
    let web = engine_root.join("web");
    let generated_web = engine_root.join(".mach/web");
    let generated_web = if generated_web.is_dir() {
        generated_web
    } else {
        web.clone()
    };
    let mut artifacts = BTreeMap::new();
    for name in [
        "index.html",
        "mach_webgpu.js",
        "mach_webgpu_bg.wasm",
        "mach_webgl2.js",
        "mach_webgl2_bg.wasm",
    ] {
        let path = if name == "index.html" {
            web.join(name)
        } else {
            generated_web.join(name)
        };
        if path.is_file() {
            artifacts.insert(name.to_owned(), ArtifactSource::File(path));
        }
    }
    collect_artifacts(&generated_web.join("snippets"), "snippets", &mut artifacts)?;
    collect_artifacts(&project.root.join("assets"), "assets", &mut artifacts)?;
    artifacts.insert(
        "server".to_owned(),
        ArtifactSource::File(server_binary.to_path_buf()),
    );
    artifacts.insert(
        "game.json".to_owned(),
        ArtifactSource::Bytes(
            fs::read(project.root.join("game.json"))
                .map_err(|error| format!("cannot read game.json: {error}"))?,
        ),
    );
    Ok(artifacts)
}

fn prepare_deployment_artifacts(
    artifacts: BTreeMap<String, ArtifactSource>,
) -> Result<Vec<PreparedDeploymentArtifact>, String> {
    let mut prepared = Vec::with_capacity(artifacts.len());
    let mut compressed = Vec::new();
    for (path, source) in artifacts {
        let raw = match source {
            ArtifactSource::File(file) => fs::read(&file)
                .map_err(|error| format!("cannot read {}: {error}", file.display()))?,
            ArtifactSource::Bytes(bytes) => bytes,
        };
        if path.ends_with("_bg.wasm") || path == "server" {
            compressed.push((path, raw));
        } else {
            prepared.push(prepare_deployment_artifact(path, raw, false)?);
        }
    }

    compressed.sort_by_key(|(_, raw)| std::cmp::Reverse(raw.len()));
    let workers = compressed.len().min(3);
    let mut queues = (0..workers).map(|_| Vec::new()).collect::<Vec<_>>();
    for (index, artifact) in compressed.into_iter().enumerate() {
        queues[index % workers].push(artifact);
    }
    std::thread::scope(|scope| -> Result<(), String> {
        let tasks = queues
            .into_iter()
            .map(|queue| {
                scope.spawn(move || {
                    queue
                        .into_iter()
                        .map(|(path, raw)| prepare_deployment_artifact(path, raw, true))
                        .collect::<Result<Vec<_>, _>>()
                })
            })
            .collect::<Vec<_>>();
        for task in tasks {
            prepared.extend(
                task.join()
                    .map_err(|_| "deployment artifact worker panicked".to_owned())??,
            );
        }
        Ok(())
    })?;
    prepared.sort_by(|left, right| left.file.path.cmp(&right.file.path));
    Ok(prepared)
}

fn prepare_deployment_artifact(
    path: String,
    raw: Vec<u8>,
    compress: bool,
) -> Result<PreparedDeploymentArtifact, String> {
    let (data, content_encoding) = if compress {
        (gzip(&raw)?, Some("gzip".to_owned()))
    } else {
        (raw, None)
    };
    Ok(PreparedDeploymentArtifact {
        file: DeploymentFileManifest {
            path: path.clone(),
            size: data.len() as u64,
            sha256: hex_digest(&data),
            content_type: content_type(&path).to_owned(),
            content_encoding,
        },
        data,
    })
}

fn collect_artifacts(
    root: &Path,
    prefix: &str,
    artifacts: &mut BTreeMap<String, ArtifactSource>,
) -> Result<(), String> {
    if !root.exists() {
        return Ok(());
    }
    for entry in
        fs::read_dir(root).map_err(|error| format!("cannot read {}: {error}", root.display()))?
    {
        let entry = entry.map_err(|error| format!("cannot read {}: {error}", root.display()))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("artifact names must be UTF-8 in {}", root.display()))?;
        let artifact_path = format!("{prefix}/{name}");
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", entry.path().display()))?;
        if file_type.is_symlink() {
            return Err(format!(
                "artifact symlinks are not supported: {}",
                entry.path().display()
            ));
        }
        if file_type.is_dir() {
            collect_artifacts(&entry.path(), &artifact_path, artifacts)?;
        } else if file_type.is_file() {
            artifacts.insert(artifact_path, ArtifactSource::File(entry.path()));
        }
    }
    Ok(())
}

fn gzip(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(data)
        .map_err(|error| format!("cannot compress browser module: {error}"))?;
    encoder
        .finish()
        .map_err(|error| format!("cannot finish browser module: {error}"))
}

fn hex_digest(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn content_type(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("wasm") => "application/wasm",
        Some("glb") => "model/gltf-binary",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("mp3") => "audio/mpeg",
        Some("wgsl") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn upload_file(
    url: &str,
    token: &str,
    data: &[u8],
    sha256: &str,
    content_type: &str,
    content_encoding: Option<&str>,
) -> Result<(), String> {
    let mut last_error = None;
    for attempt in 0..3 {
        let mut request = with_cli_identity(ureq::put(url))
            .set("authorization", &format!("Bearer {token}"))
            .set("content-type", content_type)
            .set("x-game-sha256", sha256)
            .set("x-game-size", &data.len().to_string());
        if let Some(encoding) = content_encoding {
            request = request.set("content-encoding", encoding);
        }
        match request.send_bytes(data) {
            Ok(_) => return Ok(()),
            Err(error) => {
                last_error = Some(render_ureq_error(error));
                if attempt < 2 {
                    std::thread::sleep(Duration::from_millis(250 * (attempt + 1) as u64));
                }
            }
        }
    }
    Err(format!(
        "cannot upload artifact: {}",
        last_error.unwrap_or_else(|| "unknown error".to_owned())
    ))
}

fn send_authenticated_json<T: serde::de::DeserializeOwned, B: Serialize>(
    method: &str,
    url: &str,
    token: &str,
    body: &B,
    context: &str,
) -> Result<T, String> {
    let request = with_cli_identity(ureq::request(method, url))
        .set("authorization", &format!("Bearer {token}"))
        .set("accept", "application/json");
    send_json(request, body, context)
}

fn with_cli_identity(mut request: ureq::Request) -> ureq::Request {
    request = request.set("x-mach-cli-version", env!("CARGO_PKG_VERSION"));
    if let Some(installation_id) = crate::telemetry::installation_id() {
        request = request.set("x-mach-installation-id", &installation_id);
    }
    request
}

fn send_json<T: serde::de::DeserializeOwned, B: Serialize>(
    request: ureq::Request,
    body: &B,
    context: &str,
) -> Result<T, String> {
    let body = serde_json::to_string(body).map_err(|error| format!("{context}: {error}"))?;
    let response = request
        .set("content-type", "application/json")
        .set("accept", "application/json")
        .send_string(&body)
        .map_err(render_http_error(context))?;
    read_json(response, context)
}

fn read_json<T: serde::de::DeserializeOwned>(
    response: ureq::Response,
    context: &str,
) -> Result<T, String> {
    serde_json::from_reader(response.into_reader()).map_err(|error| format!("{context}: {error}"))
}

fn render_http_error(context: &str) -> impl FnOnce(ureq::Error) -> String + '_ {
    move |error| format!("{context}: {}", render_ureq_error(error))
}

fn render_ureq_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(status, response) => {
            let detail = response
                .into_string()
                .ok()
                .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
                .and_then(|value| {
                    value
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                });
            detail.map_or_else(
                || format!("HTTP {status}"),
                |detail| format!("HTTP {status}: {detail}"),
            )
        }
        ureq::Error::Transport(error) => error.to_string(),
    }
}

fn absolute_url(base: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        path.to_owned()
    } else {
        format!(
            "{}{}",
            base.trim_end_matches('/'),
            if path.starts_with('/') {
                path.to_owned()
            } else {
                format!("/{path}")
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn deployment_artifacts_are_ordered_and_gzipped() {
        let mut artifacts = BTreeMap::new();
        artifacts.insert(
            "server".to_owned(),
            ArtifactSource::Bytes(b"server bytes".to_vec()),
        );
        artifacts.insert(
            "assets/game.json".to_owned(),
            ArtifactSource::Bytes(b"{}".to_vec()),
        );

        let prepared = prepare_deployment_artifacts(artifacts).expect("prepare artifacts");
        assert_eq!(prepared[0].file.path, "assets/game.json");
        assert_eq!(prepared[0].data, b"{}");
        assert_eq!(prepared[1].file.path, "server");
        assert_eq!(prepared[1].file.content_encoding.as_deref(), Some("gzip"));

        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(prepared[1].data.as_slice())
            .read_to_end(&mut decoded)
            .expect("decode artifact");
        assert_eq!(decoded, b"server bytes");
    }
}
