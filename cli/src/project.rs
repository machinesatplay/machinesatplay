use super::*;

pub(super) struct ProjectFiles {
    pub(super) root: PathBuf,
    pub(super) manifest: game_format::GameManifest,
}

#[derive(Debug)]
pub(super) struct ProjectLoadError {
    path: PathBuf,
    issue_path: Option<String>,
    line: Option<usize>,
    column: Option<usize>,
    message: String,
}

impl ProjectLoadError {
    fn io(path: &Path, action: &str, error: impl std::fmt::Display) -> Self {
        Self {
            path: path.to_path_buf(),
            issue_path: None,
            line: None,
            column: None,
            message: format!("cannot {action}: {error}"),
        }
    }

    fn json(path: &Path, error: serde_json::Error) -> Self {
        let line = error.line();
        let column = error.column();
        let rendered = error.to_string();
        let suffix = format!(" at line {line} column {column}");
        let message = rendered
            .strip_suffix(&suffix)
            .unwrap_or(&rendered)
            .to_owned();
        Self {
            path: path.to_path_buf(),
            issue_path: None,
            line: Some(line),
            column: Some(column),
            message,
        }
    }

    fn validation_path(&self, project_root: &Path) -> String {
        if let Some(path) = &self.issue_path {
            return path.clone();
        }
        let path = self.path.strip_prefix(project_root).unwrap_or(&self.path);
        let path = if path.as_os_str().is_empty() {
            "project".to_owned()
        } else {
            path.display().to_string()
        };
        match (self.line, self.column) {
            (Some(line), Some(column)) => format!("{path}:{line}:{column}"),
            _ => path,
        }
    }
}

impl std::fmt::Display for ProjectLoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(path) = &self.issue_path {
            return write!(formatter, "{path}: {}", self.message);
        }
        match (self.line, self.column) {
            (Some(line), Some(column)) => write!(
                formatter,
                "{}:{line}:{column}: {}",
                self.path.display(),
                self.message
            ),
            _ => write!(formatter, "{}: {}", self.path.display(), self.message),
        }
    }
}

pub(super) fn load_project(directory: &Path) -> Result<ProjectFiles, ProjectLoadError> {
    let root = directory
        .canonicalize()
        .map_err(|error| ProjectLoadError::io(directory, "open project", error))?;
    let manifest_path = root.join("game.json");
    let manifest_json = fs::read_to_string(&manifest_path)
        .map_err(|error| ProjectLoadError::io(&manifest_path, "read file", error))?;
    let manifest: game_format::GameManifest = serde_json::from_str(&manifest_json)
        .map_err(|error| ProjectLoadError::json(&manifest_path, error))?;
    Ok(ProjectFiles { root, manifest })
}

pub(super) fn project_issues(project: &ProjectFiles) -> Vec<game_format::ValidationIssue> {
    let mut issues = project.manifest.validate();
    if !is_source_engine(&project.root) {
        issues.push(game_format::ValidationIssue {
            path: "src/main.rs".to_owned(),
            message: "browser game code is missing".to_owned(),
        });
    }
    issues
}

pub(super) fn validate_command(directory: &Path, json: bool) -> Result<(), String> {
    let project = match load_project(directory) {
        Ok(project) => project,
        Err(error) if json => {
            println!("{}", validation_error_json(directory, &error));
            return Err("project validation failed".to_owned());
        }
        Err(error) => return Err(error.to_string()),
    };
    let issues = project_issues(&project);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "valid": issues.is_empty(),
                "project": project.root,
                "issues": issues,
            }))
            .expect("validation output serializes")
        );
    } else if issues.is_empty() {
        println!("✓ {} is valid", project.manifest.name);
        println!("  {}", project.root.display());
    } else {
        eprintln!("Found {} problem(s):", issues.len());
        for issue in &issues {
            eprintln!("  ✗ {issue}");
        }
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err("project validation failed".to_owned())
    }
}

fn validation_error_json(directory: &Path, error: &ProjectLoadError) -> String {
    let project_root = directory
        .canonicalize()
        .unwrap_or_else(|_| directory.to_path_buf());
    serde_json::to_string_pretty(&serde_json::json!({
        "valid": false,
        "project": project_root,
        "issues": [{
            "path": error.validation_path(&project_root),
            "message": error.message,
        }],
    }))
    .expect("validation output serializes")
}

pub(super) fn doctor_command(json: bool) -> Result<(), String> {
    let platform = platform_id().ok();
    let tools = crate::managed_tools::status();
    let local_build_ready = tools.as_ref().is_some_and(|tools| tools.native_ready());
    let cargo = tools.as_ref().and_then(|tools| tools.cargo.clone());
    let rustup = tools.as_ref().and_then(|tools| tools.rustup.clone());
    let wasm_bindgen = tools.as_ref().and_then(|tools| tools.wasm_bindgen.clone());
    let wasm_target = tools.as_ref().is_some_and(|tools| tools.wasm_target);
    let ready = platform.is_some();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ready": ready,
                "platform": platform,
                "buildMode": "local",
                "localBuildReady": local_build_ready,
                "cargo": cargo,
                "rustup": rustup,
                "wasmTarget": wasm_target,
                "wasmBindgen": wasm_bindgen,
                "expectedRust": crate::managed_tools::RUST_VERSION,
                "expectedWasmBindgen": WASM_BINDGEN_VERSION,
                "rustInstallUrl": "https://rustup.rs",
            }))
            .expect("doctor output serializes")
        );
    } else {
        println!("mach development environment");
        println!("  {} supported platform", mark(platform.is_some()));
        println!(
            "  {} rustup {}",
            mark(rustup.is_some()),
            tool_version(rustup.as_deref(), "rustup ").unwrap_or("not found")
        );
        println!(
            "  {} Cargo {}",
            mark(cargo.is_some()),
            tool_version(cargo.as_deref(), "cargo ").unwrap_or("not installed")
        );
        println!("  {} wasm32-unknown-unknown target", mark(wasm_target));
        println!(
            "  {} wasm-bindgen {}",
            if wasm_bindgen.is_some() {
                "✓ installed"
            } else {
                "○ managed"
            },
            WASM_BINDGEN_VERSION
        );
        if !ready {
            println!(
                "\nThis operating system and architecture does not have a release bundle yet."
            );
        } else if rustup.is_none() {
            println!(
                "\nRust is required for local builds. Install it from https://rustup.rs, then run `mach setup`."
            );
        } else if !local_build_ready {
            println!("\nMissing toolchains and managed tools are installed by `mach setup`.");
        }
    }
    Ok(())
}

fn mark(ok: bool) -> &'static str {
    if ok {
        "✓"
    } else {
        "✗"
    }
}

fn tool_version<'a>(value: Option<&'a str>, prefix: &str) -> Option<&'a str> {
    value.map(|value| value.strip_prefix(prefix).unwrap_or(value))
}

pub(super) fn create_project(directory: &Path) -> Result<(), String> {
    let started = Instant::now();
    if directory.exists()
        && directory
            .read_dir()
            .map_err(|error| format!("cannot inspect {}: {error}", directory.display()))?
            .next()
            .is_some()
    {
        return Err(format!(
            "{} already exists and is not empty",
            directory.display()
        ));
    }

    let project_name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("my-game");
    if !project_name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_')
    {
        return Err(
            "project directory name may only contain letters, numbers, hyphens, and underscores"
                .to_owned(),
        );
    }

    println!("mach: creating multiplayer game...");
    fs::create_dir_all(directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    extract_embedded_starter(directory)?;
    write_project_file(directory, "README.md", STARTER_README.as_bytes())?;
    write_project_file(directory, "AGENTS.md", STARTER_AGENTS.as_bytes())?;
    let manifest = format!(
        "{{\n  \"$schema\": \"./game.schema.json\",\n  \"schemaVersion\": 1,\n  \"name\": \"{project_name}\",\n  \"engineVersion\": \"{ENGINE_VERSION}\"\n}}\n"
    );
    write_project_file(directory, "game.json", manifest.as_bytes())?;
    write_project_file(
        directory,
        "game.schema.json",
        STARTER_GAME_SCHEMA.as_bytes(),
    )?;
    crate::starter_bundle::record_pristine_source(directory)?;

    checked(
        Command::new("git").arg("init").current_dir(directory),
        "failed to initialize the starter repository",
    )?;

    println!(
        "\nCreated {}.\n\n  cd {}\n  mach dev\n",
        project_name,
        directory.display()
    );
    crate::telemetry::project_created(started.elapsed());
    Ok(())
}

pub(super) fn activate_validated_file(candidate: &Path, destination: &Path) -> Result<(), String> {
    match fs::rename(candidate, destination) {
        Ok(()) => return Ok(()),
        Err(error) if !destination.exists() => {
            return Err(format!(
                "cannot activate {}: {error}",
                destination.display()
            ));
        }
        Err(_) => {}
    }

    let backup = destination.with_extension("previous");
    if backup.exists() {
        fs::remove_file(&backup)
            .map_err(|error| format!("cannot clear {}: {error}", backup.display()))?;
    }
    fs::rename(destination, &backup)
        .map_err(|error| format!("cannot preserve {}: {error}", destination.display()))?;
    match fs::rename(candidate, destination) {
        Ok(()) => {
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup, destination);
            Err(format!(
                "cannot activate {}: {error}",
                destination.display()
            ))
        }
    }
}

fn extract_embedded_starter(destination: &Path) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(io::Cursor::new(EMBEDDED_SOURCE_STARTER))
        .map_err(|error| format!("embedded starter is invalid: {error}"))?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("cannot read embedded starter: {error}"))?;
        let relative = entry
            .enclosed_name()
            .ok_or_else(|| "embedded starter contains an invalid path".to_owned())?;
        let path = destination.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        let mut output = fs::File::create(&path)
            .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
        io::copy(&mut entry, &mut output)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }
    let cargo_path = destination.join("Cargo.toml");
    let cargo_manifest = fs::read_to_string(&cargo_path)
        .map_err(|error| format!("cannot read embedded Cargo.toml: {error}"))?;
    fs::write(&cargo_path, source_game_cargo_manifest(&cargo_manifest)?)
        .map_err(|error| format!("cannot update embedded Cargo.toml: {error}"))?;
    Ok(())
}

#[cfg(test)]
pub(super) fn validate_source_starter_root(root: &Path) -> Result<(), String> {
    for required in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        ".cargo/config.toml",
        "src/main.rs",
        "src/runtime.rs",
        "crates/game-client/Cargo.toml",
        "crates/game-client/src/lib.rs",
        "crates/game-core/Cargo.toml",
        "crates/game-core/src/lib.rs",
        "crates/game-server/Cargo.toml",
        "crates/game-server/src/main.rs",
        "crates/game-server/src/game.rs",
        "crates/game-format/Cargo.toml",
        "crates/render-api/Cargo.toml",
        "crates/render-fn/Cargo.toml",
        "assets",
        "web/index.html",
    ] {
        if !root.join(required).exists() {
            return Err(format!("{} is missing {required}", root.display()));
        }
    }
    Ok(())
}

fn source_game_cargo_manifest(manifest: &str) -> Result<String, String> {
    let without_cli = manifest.replace("  \"cli\",\n", "");
    if without_cli == manifest {
        return Err(
            "engine Cargo.toml does not contain the expected CLI workspace member".to_owned(),
        );
    }
    let without_build_tool_patch = without_cli
        .replace(
            "exclude = [\"vendor/walrus\", \"vendor/wasm-bindgen-cli-support\"]\n",
            "",
        )
        .replace(
            "# Pinned browser build transforms with large-module fast paths.\n[patch.crates-io]\nwalrus = { path = \"vendor/walrus\" }\nwasm-bindgen-cli-support = { path = \"vendor/wasm-bindgen-cli-support\" }\n\n",
            "",
        );
    if without_build_tool_patch == without_cli {
        return Err("engine Cargo.toml has no expected build-tool patch".to_owned());
    }
    let client_default = without_build_tool_patch.replace("default = []", "default = [\"client\"]");
    if client_default == without_build_tool_patch {
        return Err("engine Cargo.toml has no expected default feature set".to_owned());
    }
    Ok(client_default)
}

fn write_project_file(root: &Path, relative: &str, data: &[u8]) -> Result<(), String> {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    fs::write(&path, data).map_err(|error| format!("cannot write {}: {error}", path.display()))
}
