use crate::cache::{mach_cache_root, shared_cargo_target_dir};
use crate::RELEASES_URL;
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

const SEED_RETRY_DELAY: Duration = Duration::from_secs(60 * 60);
const MAX_SEED_DOWNLOAD_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_SEED_EXTRACTED_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_RECONSTRUCTED_RMETA_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_SEED_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_SEED_PART_BYTES: u64 = 300 * 1024 * 1024;
const BUILD_CACHE_VERSION: &str = "0.1.24";

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildCacheManifest {
    schema_version: u32,
    sha256: String,
    size: u64,
    parts: Vec<BuildCachePart>,
}

#[derive(serde::Deserialize)]
struct BuildCachePart {
    name: String,
    sha256: String,
    size: u64,
}

pub(crate) struct BuildSeedGuard {
    _lock: fs::File,
}

pub(crate) fn prepare_build_seed() -> Result<Option<BuildSeedGuard>, String> {
    prepare_build_seed_for(false)
}

pub(crate) fn prepare_deploy_build_seed() -> Result<Option<BuildSeedGuard>, String> {
    prepare_build_seed_for(true)
}

fn prepare_build_seed_for(deployment: bool) -> Result<Option<BuildSeedGuard>, String> {
    let Some(platform) = platform() else {
        return Ok(None);
    };
    let root = build_root(platform)?;
    ensure_private_build_root(&root)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("build.lock"))
        .map_err(|error| format!("cannot open build cache lock: {error}"))?;
    lock.lock_exclusive()
        .map_err(|error| format!("cannot lock build cache: {error}"))?;
    install_seed_if_needed(&root, platform);
    if deployment {
        install_deploy_seed_if_needed(&root, platform);
    }
    fs::create_dir_all(root.join("target"))
        .map_err(|error| format!("cannot create Cargo target cache: {error}"))?;
    fs::create_dir_all(root.join("cargo-home"))
        .map_err(|error| format!("cannot create Cargo home: {error}"))?;
    sync_user_cargo_files(&root.join("cargo-home"))?;
    FileExt::unlock(&lock)
        .map_err(|error| format!("cannot release build cache install lock: {error}"))?;
    FileExt::lock_shared(&lock)
        .map_err(|error| format!("cannot lock build cache for use: {error}"))?;
    Ok(Some(BuildSeedGuard { _lock: lock }))
}

pub(crate) fn seed_is_ready() -> Result<bool, String> {
    let Some(platform) = platform() else {
        return Ok(true);
    };
    Ok(build_root(platform)?.join("seed.sha256").is_file())
}

pub(crate) fn cargo_target_dir(project_root: &Path) -> Result<PathBuf, String> {
    match platform() {
        Some(platform) => project_target_dir(&build_root(platform)?, project_root),
        None => shared_cargo_target_dir(),
    }
}

pub(crate) fn configure_cargo_home(command: &mut Command) -> Result<(), String> {
    if let Some(platform) = platform() {
        command.env("CARGO_HOME", build_root(platform)?.join("cargo-home"));
    }
    crate::managed_tools::configure(command)
}

pub(crate) fn external_build_root() -> Result<Option<PathBuf>, String> {
    platform().map(build_root).transpose()
}

pub(crate) fn prepare_project_cache(project_root: &Path, deployment: bool) -> Result<(), String> {
    let Some(platform) = platform() else {
        return Ok(());
    };
    let root = build_root(platform)?;
    let project = project_root
        .canonicalize()
        .map_err(|error| format!("cannot locate project for build cache: {error}"))?;
    let prepared = prepare_project_target(&root, &project)?;
    let mode = if deployment { "deploy" } else { "dev" };
    let marker = prepared.target.join(format!(".mach-project-{mode}"));
    if fs::read_to_string(&marker).ok().as_deref() == Some(prepared.expected_marker.as_str()) {
        fs::write(&marker, prepared.expected_marker.as_bytes())
            .map_err(|error| format!("cannot refresh project build cache age: {error}"))?;
        fs::write(
            prepared.target.join(".mach-overlay"),
            prepared.expected_marker.as_bytes(),
        )
        .map_err(|error| format!("cannot refresh project build cache age: {error}"))?;
        return Ok(());
    }
    let target = &prepared.target;

    clean_project_packages(
        &project.join("Cargo.toml"),
        target,
        "wasm32-unknown-unknown",
        if deployment {
            "mach-deploy"
        } else {
            "mach-dev"
        },
        &[
            "mach",
            "game-client",
            "game-core",
            "game-format",
            "game-server",
            "render-api",
            "render-fn",
        ],
    )?;
    if deployment {
        clean_project_packages(
            &project.join("Cargo.toml"),
            target,
            "x86_64-unknown-linux-musl",
            "mach-deploy",
            &[
                "mach",
                "game-client",
                "game-core",
                "game-format",
                "game-server",
                "render-api",
                "render-fn",
            ],
        )?;
    }
    clean_native_project_packages(
        &project.join("Cargo.toml"),
        target,
        if deployment {
            "mach-deploy"
        } else {
            "mach-dev"
        },
        &[
            "mach",
            "game-client",
            "game-core",
            "game-format",
            "game-server",
            "render-api",
            "render-fn",
        ],
    )?;
    clean_project_packages(
        &project.join("Cargo.toml"),
        target,
        "aarch64-apple-darwin",
        if deployment {
            "mach-deploy"
        } else {
            "mach-dev"
        },
        &[
            "mach",
            "game-client",
            "game-core",
            "game-format",
            "game-server",
            "render-api",
            "render-fn",
        ],
    )?;
    fs::write(&marker, prepared.expected_marker.as_bytes())
        .map_err(|error| format!("cannot record project build cache: {error}"))
}

fn project_target_dir(build_root: &Path, project_root: &Path) -> Result<PathBuf, String> {
    let project = project_root
        .canonicalize()
        .map_err(|error| format!("cannot locate project for build cache: {error}"))?;
    let key = blake3::hash(project.to_string_lossy().as_bytes())
        .to_hex()
        .to_string();
    Ok(build_root.join("projects").join(key).join("target"))
}

fn project_target_marker(build_root: &Path, project_root: &Path) -> String {
    let seed = fs::read_to_string(build_root.join("seed.sha256")).unwrap_or_default();
    let deploy_seed = fs::read_to_string(build_root.join("deploy-seed.sha256")).unwrap_or_default();
    format!(
        "{}\n{}\n{}",
        project_root.display(),
        seed.trim(),
        deploy_seed.trim()
    )
}

struct ProjectTargetPreparation {
    target: PathBuf,
    expected_marker: String,
    _lock: fs::File,
}

fn prepare_project_target(
    build_root: &Path,
    project_root: &Path,
) -> Result<ProjectTargetPreparation, String> {
    let target = project_target_dir(build_root, project_root)?;
    let marker = target.join(".mach-overlay");
    let expected_marker = project_target_marker(build_root, project_root);
    let project_cache = target
        .parent()
        .ok_or_else(|| "project build cache has no parent".to_owned())?;
    fs::create_dir_all(project_cache)
        .map_err(|error| format!("cannot create project build cache: {error}"))?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(project_cache.join("prepare.lock"))
        .map_err(|error| format!("cannot open project build cache lock: {error}"))?;
    lock.lock_exclusive()
        .map_err(|error| format!("cannot lock project build cache: {error}"))?;
    if fs::read_to_string(&marker).ok().as_deref() == Some(expected_marker.as_str()) {
        return Ok(ProjectTargetPreparation {
            target,
            expected_marker,
            _lock: lock,
        });
    }

    let candidate = project_cache.join(format!("target-next-{}", std::process::id()));
    if candidate.exists() {
        fs::remove_dir_all(&candidate)
            .map_err(|error| format!("cannot clear incomplete project build cache: {error}"))?;
    }
    println!("  cache     preparing isolated project artifacts");
    let status = Command::new("/bin/cp")
        .args(["-cR"])
        .arg(build_root.join("target"))
        .arg(&candidate)
        .status()
        .map_err(|error| format!("cannot clone prebuilt project cache: {error}"))?;
    if !status.success() {
        let _ = fs::remove_dir_all(&candidate);
        return Err(format!("cannot clone prebuilt project cache ({status})"));
    }
    fs::write(candidate.join(".mach-overlay"), expected_marker.as_bytes())
        .map_err(|error| format!("cannot mark isolated project cache: {error}"))?;
    if target.exists() {
        fs::remove_dir_all(&target)
            .map_err(|error| format!("cannot replace project build cache: {error}"))?;
    }
    fs::rename(&candidate, &target)
        .map_err(|error| format!("cannot activate project build cache: {error}"))?;
    Ok(ProjectTargetPreparation {
        target,
        expected_marker,
        _lock: lock,
    })
}

fn clean_native_project_packages(
    manifest: &Path,
    target_dir: &Path,
    profile: &str,
    packages: &[&str],
) -> Result<(), String> {
    let mut command = crate::managed_tools::cargo_command()?;
    command
        .arg("clean")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(manifest)
        .args(["--profile", profile])
        .env("CARGO_TARGET_DIR", target_dir);
    for package in packages {
        command.args(["-p", package]);
    }
    configure_cargo_home(&mut command)?;
    let status = command
        .status()
        .map_err(|error| format!("cannot clean project build cache: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cannot clean project build cache ({status})"))
    }
}

fn clean_project_packages(
    manifest: &Path,
    target_dir: &Path,
    target: &str,
    profile: &str,
    packages: &[&str],
) -> Result<(), String> {
    let mut command = crate::managed_tools::cargo_command()?;
    command
        .arg("clean")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(manifest)
        .args(["--target", target, "--profile", profile])
        .env("CARGO_TARGET_DIR", target_dir);
    for package in packages {
        command.args(["-p", package]);
    }
    configure_cargo_home(&mut command)?;
    let status = command
        .status()
        .map_err(|error| format!("cannot clean project build cache: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cannot clean project build cache ({status})"))
    }
}

fn platform() -> Option<&'static str> {
    prebuilt_platform(std::env::consts::OS, std::env::consts::ARCH)
}

fn prebuilt_platform(os: &str, architecture: &str) -> Option<&'static str> {
    match (os, architecture) {
        ("macos", "aarch64") => Some("macos-aarch64"),
        _ => None,
    }
}

fn build_root(platform: &str) -> Result<PathBuf, String> {
    if platform != "macos-aarch64" {
        return Err(format!("unsupported prebuilt cache platform {platform}"));
    }
    Ok(PathBuf::from(format!(
        "/private/tmp/mach-build-{BUILD_CACHE_VERSION}-{platform}"
    )))
}

fn ensure_private_build_root(root: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if let Ok(metadata) = fs::symlink_metadata(root) {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!("refusing unsafe build cache {}", root.display()));
            }
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(format!(
                    "build cache {} belongs to another user",
                    root.display()
                ));
            }
        } else {
            fs::create_dir_all(root)
                .map_err(|error| format!("cannot create build cache: {error}"))?;
        }
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("cannot protect build cache: {error}"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = root;
        Err("prebuilt build caches are only available on apple silicon macs".to_owned())
    }
}

fn sync_user_cargo_files(destination: &Path) -> Result<(), String> {
    let source = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
    let Some(source) = source.filter(|source| source != destination) else {
        return Ok(());
    };
    for name in ["config", "config.toml", "credentials", "credentials.toml"] {
        let source_file = source.join(name);
        if !source_file.is_file() {
            continue;
        }
        fs::copy(&source_file, destination.join(name)).map_err(|error| {
            format!(
                "cannot copy Cargo configuration {}: {error}",
                source_file.display()
            )
        })?;
    }
    Ok(())
}

fn install_seed_if_needed(root: &Path, platform: &str) {
    if root.join("seed.sha256").is_file() || cargo_cache_has_artifacts(root) {
        return;
    }
    let Some(cache_root) = mach_cache_root() else {
        return;
    };
    let failure_marker = cache_root
        .join("build")
        .join(BUILD_CACHE_VERSION)
        .join(format!("{platform}.seed-unavailable"));
    if marker_is_fresh(&failure_marker, SEED_RETRY_DELAY) {
        return;
    }
    println!("mach: downloading {platform} build cache...");
    match download_and_install_seed(root, platform, "mach-build-cache", install_seed_archive) {
        Ok(()) => {
            let _ = fs::remove_file(&failure_marker);
            println!("mach: {platform} build cache ready");
        }
        Err(error) => {
            if let Some(parent) = failure_marker.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(&failure_marker, error.as_bytes());
            eprintln!("mach: build cache unavailable, compiling locally ({error})");
        }
    }
}

fn install_deploy_seed_if_needed(root: &Path, platform: &str) {
    if root.join("deploy-seed.sha256").is_file() || deploy_cache_has_artifacts(root) {
        return;
    }
    let Some(cache_root) = mach_cache_root() else {
        return;
    };
    let failure_marker = cache_root
        .join("build")
        .join(BUILD_CACHE_VERSION)
        .join(format!("{platform}.deploy-seed-unavailable"));
    if marker_is_fresh(&failure_marker, SEED_RETRY_DELAY) {
        return;
    }
    println!("mach: downloading deploy build cache...");
    match download_and_install_seed(
        root,
        platform,
        "mach-deploy-cache",
        install_deploy_seed_archive,
    ) {
        Ok(()) => {
            let _ = fs::remove_file(&failure_marker);
            println!("mach: deploy build cache ready");
        }
        Err(error) => {
            if let Some(parent) = failure_marker.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(&failure_marker, error.as_bytes());
            eprintln!("mach: deploy cache unavailable, compiling locally ({error})");
        }
    }
}

fn cargo_cache_has_artifacts(root: &Path) -> bool {
    root.join("target/wasm32-unknown-unknown").is_dir() || root.join("target/mach-dev").is_dir()
}

fn deploy_cache_has_artifacts(root: &Path) -> bool {
    [
        root.join("target/mach-deploy"),
        root.join("target/wasm32-unknown-unknown/mach-deploy"),
        root.join("target/x86_64-unknown-linux-musl/mach-deploy"),
    ]
    .iter()
    .all(|path| path.is_dir())
}

fn marker_is_fresh(path: &Path, max_age: Duration) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < max_age)
}

fn release_base_url() -> String {
    std::env::var("MACH_RELEASES_URL")
        .unwrap_or_else(|_| RELEASES_URL.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

fn download_and_install_seed(
    root: &Path,
    platform: &str,
    cache_name: &str,
    install: impl FnOnce(&Path, &Path, &str) -> Result<(), String>,
) -> Result<(), String> {
    let cache_root =
        mach_cache_root().ok_or_else(|| "cannot locate the user cache directory".to_owned())?;
    let downloads = cache_root.join("downloads");
    fs::create_dir_all(&downloads)
        .map_err(|error| format!("cannot create download cache: {error}"))?;
    let name = format!("{cache_name}-{platform}.tar.zst");
    let url = format!("{}/v{BUILD_CACHE_VERSION}/{name}", release_base_url());
    let expected = download_checksum(&format!("{url}.sha256"))?;
    let archive_path = downloads.join(format!("{name}.next-{}", std::process::id()));
    let result = (|| {
        download_cache_archive(&url, &archive_path, &expected)?;
        let actual = file_sha256(&archive_path)?;
        if actual != expected {
            return Err(format!(
                "build cache checksum mismatch: expected {expected}, received {actual}"
            ));
        }
        install(&archive_path, root, &expected)
    })();
    let _ = fs::remove_file(&archive_path);
    result
}

fn download_cache_archive(url: &str, destination: &Path, expected: &str) -> Result<(), String> {
    let manifest_url = format!("{url}.parts.json");
    let response = ureq::get(&manifest_url)
        .call()
        .map_err(|error| format!("cannot download build cache manifest: {error}"))?;
    let mut manifest_json = String::new();
    response
        .into_reader()
        .take(MAX_SEED_MANIFEST_BYTES + 1)
        .read_to_string(&mut manifest_json)
        .map_err(|error| format!("cannot read build cache manifest: {error}"))?;
    if manifest_json.len() as u64 > MAX_SEED_MANIFEST_BYTES {
        return Err("build cache manifest is too large".to_owned());
    }
    let manifest: BuildCacheManifest = serde_json::from_str(&manifest_json)
        .map_err(|error| format!("build cache manifest is invalid: {error}"))?;
    if manifest.schema_version != 1 {
        return Err("build cache manifest has an unsupported schema".to_owned());
    }
    if !valid_sha256(&manifest.sha256) || !manifest.sha256.eq_ignore_ascii_case(expected) {
        return Err("build cache manifest checksum does not match".to_owned());
    }
    if manifest.size > MAX_SEED_DOWNLOAD_BYTES || manifest.parts.is_empty() {
        return Err("build cache manifest has an invalid size".to_owned());
    }

    let archive_name = url
        .rsplit('/')
        .next()
        .ok_or_else(|| "build cache URL has no archive name".to_owned())?;
    let base_url = url
        .rsplit_once('/')
        .map(|(base, _)| base)
        .ok_or_else(|| "build cache URL has no base".to_owned())?;
    let mut output = fs::File::create(destination)
        .map_err(|error| format!("cannot create build cache download: {error}"))?;
    let mut archive_digest = Sha256::new();
    let mut archive_bytes = 0_u64;

    for (index, part) in manifest.parts.iter().enumerate() {
        let expected_name = format!("{archive_name}.part-{index:03}");
        if part.name != expected_name
            || !valid_sha256(&part.sha256)
            || part.size == 0
            || part.size > MAX_SEED_PART_BYTES
        {
            return Err(format!("build cache manifest has an invalid part {index}"));
        }
        let part_url = format!("{base_url}/{}", part.name);
        let response = ureq::get(&part_url)
            .call()
            .map_err(|error| format!("cannot download build cache part {index}: {error}"))?;
        if response
            .header("content-length")
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|size| size != part.size)
        {
            return Err(format!("build cache part {index} has an unexpected size"));
        }
        let mut input = response.into_reader().take(part.size + 1);
        let mut part_digest = Sha256::new();
        let mut part_bytes = 0_u64;
        let mut buffer = [0_u8; 1024 * 1024];
        loop {
            let count = input
                .read(&mut buffer)
                .map_err(|error| format!("cannot read build cache part {index}: {error}"))?;
            if count == 0 {
                break;
            }
            part_bytes = part_bytes
                .checked_add(count as u64)
                .ok_or_else(|| "build cache download is too large".to_owned())?;
            archive_bytes = archive_bytes
                .checked_add(count as u64)
                .ok_or_else(|| "build cache download is too large".to_owned())?;
            if part_bytes > part.size || archive_bytes > MAX_SEED_DOWNLOAD_BYTES {
                return Err("build cache download is too large".to_owned());
            }
            part_digest.update(&buffer[..count]);
            archive_digest.update(&buffer[..count]);
            output
                .write_all(&buffer[..count])
                .map_err(|error| format!("cannot save build cache download: {error}"))?;
        }
        if part_bytes != part.size
            || format!("{:x}", part_digest.finalize()) != part.sha256.to_ascii_lowercase()
        {
            return Err(format!("build cache part {index} checksum mismatch"));
        }
    }
    output
        .flush()
        .map_err(|error| format!("cannot finish build cache download: {error}"))?;
    if archive_bytes != manifest.size
        || format!("{:x}", archive_digest.finalize()) != expected.to_ascii_lowercase()
    {
        return Err("build cache archive checksum mismatch".to_owned());
    }
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn download_checksum(url: &str) -> Result<String, String> {
    let response = ureq::get(url)
        .call()
        .map_err(|error| format!("cannot download checksum: {error}"))?;
    let mut checksum = String::new();
    response
        .into_reader()
        .take(4097)
        .read_to_string(&mut checksum)
        .map_err(|error| format!("cannot read checksum: {error}"))?;
    if checksum.len() > 4096 {
        return Err("build cache checksum response is too large".to_owned());
    }
    let checksum = checksum
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !valid_sha256(&checksum) {
        return Err("build cache checksum is invalid".to_owned());
    }
    Ok(checksum)
}

fn file_sha256(path: &Path) -> Result<String, String> {
    let mut input = fs::File::open(path)
        .map_err(|error| format!("cannot open build cache download: {error}"))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|error| format!("cannot verify build cache download: {error}"))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn install_seed_archive(archive_path: &Path, root: &Path, checksum: &str) -> Result<(), String> {
    install_staged_seed_archive(
        archive_path,
        root,
        checksum,
        "seed",
        install_seed_archive_at,
    )
}

fn install_deploy_seed_archive(
    archive_path: &Path,
    root: &Path,
    checksum: &str,
) -> Result<(), String> {
    install_staged_seed_archive(
        archive_path,
        root,
        checksum,
        "deploy-seed",
        install_deploy_seed_archive_at,
    )
}

fn install_staged_seed_archive(
    archive_path: &Path,
    root: &Path,
    checksum: &str,
    name: &str,
    install: impl FnOnce(&Path, &Path, &str, &Path) -> Result<(), String>,
) -> Result<(), String> {
    let candidate = root.join(format!(".{name}-next-{}", std::process::id()));
    if candidate.exists() {
        fs::remove_dir_all(&candidate)
            .map_err(|error| format!("cannot reset build cache staging directory: {error}"))?;
    }
    fs::create_dir(&candidate)
        .map_err(|error| format!("cannot create build cache staging directory: {error}"))?;
    let result = install(archive_path, root, checksum, &candidate);
    let _ = fs::remove_dir_all(&candidate);
    result
}

fn install_seed_archive_at(
    archive_path: &Path,
    root: &Path,
    checksum: &str,
    candidate: &Path,
) -> Result<(), String> {
    extract_seed_archive_at(archive_path, root, candidate)?;
    for name in ["target", "cargo-home"] {
        let source = candidate.join(name);
        let destination = root.join(name);
        if destination.exists() {
            fs::remove_dir_all(&destination)
                .map_err(|error| format!("cannot replace build cache {name}: {error}"))?;
        }
        fs::rename(&source, &destination)
            .map_err(|error| format!("cannot install build cache {name}: {error}"))?;
    }
    fs::write(root.join("seed.sha256"), format!("{checksum}\n"))
        .map_err(|error| format!("cannot record build cache version: {error}"))?;
    Ok(())
}

fn install_deploy_seed_archive_at(
    archive_path: &Path,
    root: &Path,
    checksum: &str,
    candidate: &Path,
) -> Result<(), String> {
    extract_seed_archive_at(archive_path, root, candidate)?;
    for name in ["target", "cargo-home"] {
        validate_seed_merge(&candidate.join(name), &root.join(name))?;
    }
    for name in ["target", "cargo-home"] {
        merge_seed_tree(&candidate.join(name), &root.join(name))?;
    }
    fs::write(root.join("deploy-seed.sha256"), format!("{checksum}\n"))
        .map_err(|error| format!("cannot record deploy build cache version: {error}"))?;
    Ok(())
}

fn extract_seed_archive_at(
    archive_path: &Path,
    root: &Path,
    candidate: &Path,
) -> Result<(), String> {
    let extracted_bytes = inspect_seed_archive(archive_path)?;
    let available = fs2::available_space(root)
        .map_err(|error| format!("cannot read build cache free space: {error}"))?;
    if extracted_bytes > available {
        return Err(format!(
            "build cache needs {} GiB but only {} GiB is free",
            extracted_bytes.div_ceil(1024 * 1024 * 1024),
            available / (1024 * 1024 * 1024)
        ));
    }
    let file = fs::File::open(archive_path)
        .map_err(|error| format!("cannot open build cache archive: {error}"))?;
    let mut decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|error| format!("build cache archive is invalid: {error}"))?;
    decoder
        .window_log_max(28)
        .map_err(|error| format!("build cache archive is invalid: {error}"))?;
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| format!("cannot read build cache archive: {error}"))?;
    let mut hard_links = Vec::new();
    for entry in entries {
        let mut entry =
            entry.map_err(|error| format!("cannot read build cache archive: {error}"))?;
        let relative = checked_seed_entry(&entry)?;
        let destination = candidate.join(relative);
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&destination)
                .map_err(|error| format!("cannot create build cache directory: {error}"))?;
            continue;
        }
        if entry.header().entry_type().is_hard_link() {
            hard_links.push((destination, candidate.join(checked_seed_hard_link(&entry)?)));
            continue;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create build cache directory: {error}"))?;
        }
        let mut output = fs::File::create(&destination)
            .map_err(|error| format!("cannot create build cache file: {error}"))?;
        io::copy(&mut entry, &mut output)
            .map_err(|error| format!("cannot extract build cache file: {error}"))?;
        #[cfg(unix)]
        if let Ok(mode) = entry.header().mode() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&destination, fs::Permissions::from_mode(mode & 0o777))
                .map_err(|error| format!("cannot set build cache permissions: {error}"))?;
        }
    }
    for (destination, source) in hard_links {
        if !source.is_file() {
            return Err("build cache archive contains an invalid hard link".to_owned());
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create build cache directory: {error}"))?;
        }
        fs::hard_link(&source, &destination)
            .map_err(|error| format!("cannot create build cache hard link: {error}"))?;
    }
    reconstruct_seed_rmeta(candidate)?;
    normalize_seed_mtimes(candidate)?;
    for name in ["target", "cargo-home"] {
        let source = candidate.join(name);
        if !source.is_dir() {
            return Err(format!("build cache archive is missing {name}"));
        }
    }
    Ok(())
}

fn validate_seed_merge(source: &Path, destination: &Path) -> Result<(), String> {
    if !destination.exists() {
        return Ok(());
    }
    if source.is_dir() != destination.is_dir() {
        return Err(format!(
            "deploy build cache conflicts with {}",
            destination.display()
        ));
    }
    if source.is_file() {
        if files_equal(source, destination)? {
            return Ok(());
        }
        return Err(format!(
            "deploy build cache conflicts with {}",
            destination.display()
        ));
    }
    for entry in fs::read_dir(source)
        .map_err(|error| format!("cannot inspect deploy build cache: {error}"))?
    {
        let entry = entry.map_err(|error| format!("cannot inspect deploy build cache: {error}"))?;
        validate_seed_merge(&entry.path(), &destination.join(entry.file_name()))?;
    }
    Ok(())
}

fn merge_seed_tree(source: &Path, destination: &Path) -> Result<(), String> {
    if !destination.exists() {
        fs::rename(source, destination)
            .map_err(|error| format!("cannot install deploy build cache: {error}"))?;
        return Ok(());
    }
    if source.is_file() {
        return Ok(());
    }
    for entry in fs::read_dir(source)
        .map_err(|error| format!("cannot inspect deploy build cache: {error}"))?
    {
        let entry = entry.map_err(|error| format!("cannot inspect deploy build cache: {error}"))?;
        merge_seed_tree(&entry.path(), &destination.join(entry.file_name()))?;
    }
    Ok(())
}

fn files_equal(left: &Path, right: &Path) -> Result<bool, String> {
    let left_len = fs::metadata(left)
        .map_err(|error| format!("cannot inspect deploy build cache: {error}"))?
        .len();
    let right_len = fs::metadata(right)
        .map_err(|error| format!("cannot inspect deploy build cache: {error}"))?
        .len();
    if left_len != right_len {
        return Ok(false);
    }
    let mut left = fs::File::open(left)
        .map_err(|error| format!("cannot inspect deploy build cache: {error}"))?;
    let mut right = fs::File::open(right)
        .map_err(|error| format!("cannot inspect deploy build cache: {error}"))?;
    let mut left_buffer = [0_u8; 64 * 1024];
    let mut right_buffer = [0_u8; 64 * 1024];
    loop {
        let left_count = left
            .read(&mut left_buffer)
            .map_err(|error| format!("cannot inspect deploy build cache: {error}"))?;
        let right_count = right
            .read(&mut right_buffer)
            .map_err(|error| format!("cannot inspect deploy build cache: {error}"))?;
        if left_count != right_count || left_buffer[..left_count] != right_buffer[..right_count] {
            return Ok(false);
        }
        if left_count == 0 {
            return Ok(true);
        }
    }
}

fn reconstruct_seed_rmeta(candidate: &Path) -> Result<(), String> {
    fn visit(directory: &Path, reconstructed_bytes: &mut u64) -> Result<(), String> {
        for entry in fs::read_dir(directory)
            .map_err(|error| format!("cannot inspect build cache files: {error}"))?
        {
            let entry =
                entry.map_err(|error| format!("cannot inspect build cache file: {error}"))?;
            let path = entry.path();
            if path.is_dir() {
                visit(&path, reconstructed_bytes)?;
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("rlib") {
                continue;
            }
            let rmeta = path.with_extension("rmeta");
            if rmeta.exists() {
                continue;
            }
            let rlib = fs::read(&path)
                .map_err(|error| format!("cannot read cached rlib {}: {error}", path.display()))?;
            let Some(metadata) = rlib_rmeta(&rlib)? else {
                continue;
            };
            *reconstructed_bytes = reconstructed_bytes
                .checked_add(metadata.len() as u64)
                .ok_or_else(|| "reconstructed build cache metadata is too large".to_owned())?;
            if *reconstructed_bytes > MAX_RECONSTRUCTED_RMETA_BYTES {
                return Err("reconstructed build cache metadata is too large".to_owned());
            }
            fs::write(&rmeta, metadata).map_err(|error| {
                format!(
                    "cannot reconstruct cached metadata {}: {error}",
                    rmeta.display()
                )
            })?;
        }
        Ok(())
    }

    let mut reconstructed_bytes = 0;
    visit(&candidate.join("target"), &mut reconstructed_bytes)
}

fn rlib_rmeta(archive: &[u8]) -> Result<Option<&[u8]>, String> {
    const HEADER_LEN: usize = 60;
    if !archive.starts_with(b"!<arch>\n") {
        return Err("cached rlib has an invalid archive header".to_owned());
    }
    let mut offset = 8;
    let mut long_names = None;
    while offset + HEADER_LEN <= archive.len() {
        let header = &archive[offset..offset + HEADER_LEN];
        if &header[58..] != b"`\n" {
            return Err("cached rlib has an invalid member header".to_owned());
        }
        let size = ascii_usize(&header[48..58])?;
        let data_start = offset + HEADER_LEN;
        let data_end = data_start
            .checked_add(size)
            .filter(|end| *end <= archive.len())
            .ok_or_else(|| "cached rlib member exceeds the archive".to_owned())?;
        let mut member = &archive[data_start..data_end];
        let raw_name = ascii_trim(&header[..16]);
        let name = if raw_name == b"//" {
            long_names = Some(member);
            ""
        } else if let Some(length) = raw_name.strip_prefix(b"#1/") {
            let length = ascii_usize(length)?;
            if length > member.len() {
                return Err("cached rlib has an invalid extended name".to_owned());
            }
            let name = std::str::from_utf8(&member[..length])
                .map_err(|_| "cached rlib member name is invalid".to_owned())?;
            member = &member[length..];
            name.trim_end_matches(['/', '\0'])
        } else if raw_name.starts_with(b"/") && raw_name.get(1).is_some_and(u8::is_ascii_digit) {
            let name_offset = ascii_usize(&raw_name[1..])?;
            let names =
                long_names.ok_or_else(|| "cached rlib is missing its name table".to_owned())?;
            let tail = names
                .get(name_offset..)
                .ok_or_else(|| "cached rlib has an invalid name offset".to_owned())?;
            let end = tail
                .windows(2)
                .position(|bytes| bytes == b"/\n")
                .unwrap_or(tail.len());
            std::str::from_utf8(&tail[..end])
                .map_err(|_| "cached rlib member name is invalid".to_owned())?
        } else {
            std::str::from_utf8(raw_name)
                .map_err(|_| "cached rlib member name is invalid".to_owned())?
                .trim_end_matches('/')
        };
        if name == "lib.rmeta" {
            return embedded_rmeta(member).map(Some);
        }
        offset = data_end
            .checked_add(size % 2)
            .ok_or_else(|| "cached rlib is too large".to_owned())?;
    }
    Ok(None)
}

fn ascii_trim(mut value: &[u8]) -> &[u8] {
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn ascii_usize(value: &[u8]) -> Result<usize, String> {
    std::str::from_utf8(ascii_trim(value))
        .map_err(|_| "cached rlib has an invalid number".to_owned())?
        .parse()
        .map_err(|_| "cached rlib has an invalid number".to_owned())
}

fn embedded_rmeta(member: &[u8]) -> Result<&[u8], String> {
    if member.starts_with(b"rust\0") {
        return Ok(member);
    }
    if member.starts_with(b"\0asm\x01\0\0\0") {
        return wasm_rmeta(member);
    }
    if member.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) {
        return macho_rmeta(member);
    }
    Err("cached rlib metadata has an unsupported object format".to_owned())
}

fn wasm_rmeta(object: &[u8]) -> Result<&[u8], String> {
    let mut offset = 8;
    while offset < object.len() {
        let section_id = object[offset];
        offset += 1;
        let section_size = read_leb_u32(object, &mut offset)? as usize;
        let section_end = offset
            .checked_add(section_size)
            .filter(|end| *end <= object.len())
            .ok_or_else(|| "cached wasm metadata section is invalid".to_owned())?;
        if section_id == 0 {
            let name_size = read_leb_u32(&object[..section_end], &mut offset)? as usize;
            let name_end = offset
                .checked_add(name_size)
                .filter(|end| *end <= section_end)
                .ok_or_else(|| "cached wasm metadata name is invalid".to_owned())?;
            if &object[offset..name_end] == b".rmeta" {
                return Ok(&object[name_end..section_end]);
            }
        }
        offset = section_end;
    }
    Err("cached wasm rlib has no metadata section".to_owned())
}

fn read_leb_u32(data: &[u8], offset: &mut usize) -> Result<u32, String> {
    let mut value = 0_u32;
    for shift in (0..35).step_by(7) {
        let byte = *data
            .get(*offset)
            .ok_or_else(|| "cached wasm metadata is truncated".to_owned())?;
        *offset += 1;
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("cached wasm metadata has an invalid integer".to_owned())
}

fn macho_rmeta(object: &[u8]) -> Result<&[u8], String> {
    const MACHO_HEADER_LEN: usize = 32;
    const SEGMENT_64: u32 = 0x19;
    const SEGMENT_64_LEN: usize = 72;
    const SECTION_64_LEN: usize = 80;
    let command_count = read_u32(object, 16)? as usize;
    let commands_size = read_u32(object, 20)? as usize;
    let commands_end = MACHO_HEADER_LEN
        .checked_add(commands_size)
        .filter(|end| *end <= object.len())
        .ok_or_else(|| "cached Mach-O metadata commands are invalid".to_owned())?;
    let mut command = MACHO_HEADER_LEN;
    for _ in 0..command_count {
        let kind = read_u32(object, command)?;
        let size = read_u32(object, command + 4)? as usize;
        let command_end = command
            .checked_add(size)
            .filter(|end| size >= 8 && *end <= commands_end)
            .ok_or_else(|| "cached Mach-O metadata command is invalid".to_owned())?;
        if kind == SEGMENT_64 {
            if size < SEGMENT_64_LEN {
                return Err("cached Mach-O metadata segment is invalid".to_owned());
            }
            let section_count = read_u32(object, command + 64)? as usize;
            let sections_end = command
                .checked_add(SEGMENT_64_LEN)
                .and_then(|start| {
                    section_count
                        .checked_mul(SECTION_64_LEN)
                        .and_then(|length| start.checked_add(length))
                })
                .filter(|end| *end <= command_end)
                .ok_or_else(|| "cached Mach-O metadata sections are invalid".to_owned())?;
            let mut section = command + SEGMENT_64_LEN;
            while section < sections_end {
                let name = &object[section..section + 16];
                let name_end = name
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(name.len());
                if &name[..name_end] == b".rmeta" {
                    let data_size = read_u64(object, section + 40)? as usize;
                    let data_offset = read_u32(object, section + 48)? as usize;
                    let data_end = data_offset
                        .checked_add(data_size)
                        .filter(|end| *end <= object.len())
                        .ok_or_else(|| "cached Mach-O metadata section is invalid".to_owned())?;
                    return Ok(&object[data_offset..data_end]);
                }
                section += SECTION_64_LEN;
            }
        }
        command = command_end;
    }
    Err("cached Mach-O rlib has no metadata section".to_owned())
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32, String> {
    let bytes: [u8; 4] = data
        .get(offset..offset + 4)
        .ok_or_else(|| "cached Mach-O metadata is truncated".to_owned())?
        .try_into()
        .expect("slice length checked");
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(data: &[u8], offset: usize) -> Result<u64, String> {
    let bytes: [u8; 8] = data
        .get(offset..offset + 8)
        .ok_or_else(|| "cached Mach-O metadata is truncated".to_owned())?
        .try_into()
        .expect("slice length checked");
    Ok(u64::from_le_bytes(bytes))
}

fn inspect_seed_archive(archive_path: &Path) -> Result<u64, String> {
    let file = fs::File::open(archive_path)
        .map_err(|error| format!("cannot open build cache archive: {error}"))?;
    let mut decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|error| format!("build cache archive is invalid: {error}"))?;
    decoder
        .window_log_max(28)
        .map_err(|error| format!("build cache archive is invalid: {error}"))?;
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| format!("cannot read build cache archive: {error}"))?;
    let mut extracted_bytes = 0_u64;
    let mut paths = std::collections::HashSet::new();
    let mut files = std::collections::HashSet::new();
    let mut hard_links = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read build cache archive: {error}"))?;
        let relative = checked_seed_entry(&entry)?;
        if !paths.insert(relative.clone()) {
            return Err("build cache archive contains a duplicate path".to_owned());
        }
        if entry.header().entry_type().is_file() {
            files.insert(relative);
        } else if entry.header().entry_type().is_hard_link() {
            hard_links.push(checked_seed_hard_link(&entry)?);
        }
        extracted_bytes = extracted_bytes
            .checked_add(
                entry
                    .header()
                    .size()
                    .map_err(|error| format!("cannot read build cache archive: {error}"))?,
            )
            .ok_or_else(|| "build cache archive is too large".to_owned())?;
        if extracted_bytes > MAX_SEED_EXTRACTED_BYTES {
            return Err("build cache archive is too large".to_owned());
        }
    }
    if hard_links.iter().any(|target| !files.contains(target)) {
        return Err("build cache archive contains an invalid hard link".to_owned());
    }
    Ok(extracted_bytes)
}

fn checked_seed_entry<R: Read>(entry: &tar::Entry<'_, R>) -> Result<PathBuf, String> {
    let relative = entry
        .path()
        .map_err(|error| format!("cannot read build cache archive path: {error}"))?
        .into_owned();
    let relative = checked_seed_path(&relative)?;
    let entry_type = entry.header().entry_type();
    if !entry_type.is_file() && !entry_type.is_dir() && !entry_type.is_hard_link() {
        return Err("build cache archive contains an unsupported file type".to_owned());
    }
    if entry_type.is_hard_link() {
        checked_seed_hard_link(entry)?;
    }
    Ok(relative)
}

fn checked_seed_hard_link<R: Read>(entry: &tar::Entry<'_, R>) -> Result<PathBuf, String> {
    let target = entry
        .link_name()
        .map_err(|error| format!("cannot read build cache hard link: {error}"))?
        .ok_or_else(|| "build cache archive contains an invalid hard link".to_owned())?;
    checked_seed_path(&target)
}

fn checked_seed_path(path: &Path) -> Result<PathBuf, String> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(component) => relative.push(component),
            std::path::Component::CurDir => {}
            _ => return Err("build cache archive contains an unsafe path".to_owned()),
        }
    }
    let safe = !relative.as_os_str().is_empty()
        && relative
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)));
    if !safe {
        return Err("build cache archive contains an unsafe path".to_owned());
    }
    let allowed = relative.starts_with("target")
        || relative.starts_with("cargo-home")
        || relative == Path::new("seed.json")
        || relative == Path::new("deploy-seed.json");
    if !allowed {
        return Err(format!(
            "build cache archive contains unexpected file {}",
            relative.display()
        ));
    }
    Ok(relative)
}

fn normalize_seed_mtimes(candidate: &Path) -> Result<(), String> {
    let now = SystemTime::now();
    let sources = filetime::FileTime::from_system_time(
        now.checked_sub(Duration::from_secs(60)).unwrap_or(now),
    );
    let artifacts = filetime::FileTime::from_system_time(now);
    set_tree_mtime(&candidate.join("cargo-home"), sources)?;
    set_tree_mtime(&candidate.join("target"), artifacts)
}

fn set_tree_mtime(root: &Path, modified: filetime::FileTime) -> Result<(), String> {
    if root.is_dir() {
        for entry in fs::read_dir(root)
            .map_err(|error| format!("cannot inspect build cache files: {error}"))?
        {
            let entry =
                entry.map_err(|error| format!("cannot inspect build cache file: {error}"))?;
            set_tree_mtime(&entry.path(), modified)?;
        }
    }
    filetime::set_file_mtime(root, modified)
        .map_err(|error| format!("cannot set build cache timestamp: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn build_root_matches_macos_seed_builder_path() {
        assert_eq!(
            build_root("macos-aarch64").unwrap(),
            PathBuf::from("/private/tmp/mach-build-0.1.24-macos-aarch64")
        );
        for platform in [
            "macos-x86_64",
            "linux-x86_64",
            "linux-aarch64",
            "windows-x86_64",
        ] {
            assert!(build_root(platform).is_err());
        }
    }

    #[test]
    fn only_apple_silicon_macs_use_the_prebuilt_cache() {
        assert_eq!(prebuilt_platform("macos", "aarch64"), Some("macos-aarch64"));
        for (os, architecture) in [
            ("macos", "x86_64"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("windows", "x86_64"),
        ] {
            assert_eq!(prebuilt_platform(os, architecture), None);
        }
    }

    #[test]
    fn project_targets_isolate_cargo_locks_and_incremental_outputs() {
        let root = test_root("project-targets");
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(&first).expect("create first project");
        fs::create_dir_all(&second).expect("create second project");

        let first_target = project_target_dir(&root, &first).expect("first target");
        let second_target = project_target_dir(&root, &second).expect("second target");

        assert_ne!(first_target, second_target);
        assert!(first_target.starts_with(root.join("projects")));
        assert_eq!(first_target.file_name().unwrap(), "target");
        fs::remove_dir_all(root).expect("remove project target test");
    }

    fn push_leb(mut value: u32, output: &mut Vec<u8>) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            output.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    fn wasm_rmeta_member(metadata: &[u8]) -> Vec<u8> {
        let mut object = b"\0asm\x01\0\0\0".to_vec();
        let mut section = Vec::new();
        push_leb(6, &mut section);
        section.extend_from_slice(b".rmeta");
        section.extend_from_slice(metadata);
        object.push(0);
        push_leb(section.len() as u32, &mut object);
        object.extend_from_slice(&section);
        object
    }

    fn macho_rmeta_member(metadata: &[u8]) -> Vec<u8> {
        let data_offset = 32 + 72 + 80;
        let mut object = vec![0; data_offset + metadata.len()];
        object[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        object[16..20].copy_from_slice(&1_u32.to_le_bytes());
        object[20..24].copy_from_slice(&152_u32.to_le_bytes());
        object[32..36].copy_from_slice(&0x19_u32.to_le_bytes());
        object[36..40].copy_from_slice(&152_u32.to_le_bytes());
        object[96..100].copy_from_slice(&1_u32.to_le_bytes());
        object[104..110].copy_from_slice(b".rmeta");
        object[120..127].copy_from_slice(b"__DWARF");
        object[144..152].copy_from_slice(&(metadata.len() as u64).to_le_bytes());
        object[152..156].copy_from_slice(&(data_offset as u32).to_le_bytes());
        object[data_offset..].copy_from_slice(metadata);
        object
    }

    fn ar_rmeta(member: &[u8]) -> Vec<u8> {
        let header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            "lib.rmeta/",
            0,
            0,
            0,
            0o100644,
            member.len()
        );
        assert_eq!(header.len(), 60);
        let mut archive = b"!<arch>\n".to_vec();
        archive.extend_from_slice(header.as_bytes());
        archive.extend_from_slice(member);
        if !member.len().is_multiple_of(2) {
            archive.push(b'\n');
        }
        archive
    }

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("mach-{name}-{}-{nonce}", std::process::id()))
    }

    fn write_seed_archive(path: &Path, files: &[(&str, &[u8])], hard_links: &[(&str, &str)]) {
        let file = fs::File::create(path).expect("create seed archive");
        let encoder = zstd::stream::write::Encoder::new(file, 1).expect("create zstd encoder");
        let mut archive = tar::Builder::new(encoder);
        for (path, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            archive
                .append_data(&mut header, path, *contents)
                .expect("append seed file");
        }
        for (path, target) in hard_links {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Link);
            header.set_size(0);
            header.set_mode(0o755);
            header.set_cksum();
            archive
                .append_link(&mut header, path, target)
                .expect("append seed hard link");
        }
        let encoder = archive.into_inner().expect("finish tar archive");
        encoder.finish().expect("finish zstd archive");
    }

    #[test]
    fn seed_archive_installs_only_expected_roots() {
        let root = test_root("seed-install");
        fs::create_dir_all(&root).expect("create test root");
        let archive_path = root.join("seed.tar.zst");
        write_seed_archive(
            &archive_path,
            &[
                ("target/probe", b"target".as_slice()),
                ("cargo-home/probe", b"cargo".as_slice()),
                ("seed.json", b"{}".as_slice()),
            ],
            &[("target/probe-link", "target/probe")],
        );

        install_seed_archive(&archive_path, &root, "abc").expect("install seed");

        assert_eq!(fs::read(root.join("target/probe")).unwrap(), b"target");
        assert_eq!(fs::read(root.join("target/probe-link")).unwrap(), b"target");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(
                fs::metadata(root.join("target/probe"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0
            );
        }
        assert_eq!(fs::read(root.join("cargo-home/probe")).unwrap(), b"cargo");
        assert!(
            fs::metadata(root.join("target/probe"))
                .unwrap()
                .modified()
                .unwrap()
                > fs::metadata(root.join("cargo-home/probe"))
                    .unwrap()
                    .modified()
                    .unwrap()
        );
        assert_eq!(
            fs::read_to_string(root.join("seed.sha256")).unwrap(),
            "abc\n"
        );
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn deploy_seed_archive_merges_with_the_development_cache() {
        let root = test_root("deploy-seed-install");
        fs::create_dir_all(root.join("target/mach-dev")).expect("create dev target");
        fs::create_dir_all(root.join("cargo-home/registry/src/shared")).expect("create Cargo home");
        fs::write(root.join("target/mach-dev/probe"), b"dev").expect("write dev target");
        fs::write(root.join("cargo-home/registry/src/shared/probe"), b"shared")
            .expect("write shared source");
        let archive_path = root.join("deploy-seed.tar.zst");
        write_seed_archive(
            &archive_path,
            &[
                ("target/mach-deploy/probe", b"deploy".as_slice()),
                ("cargo-home/registry/src/shared/probe", b"shared".as_slice()),
                (
                    "cargo-home/registry/src/deploy/probe",
                    b"deploy source".as_slice(),
                ),
                ("deploy-seed.json", b"{}".as_slice()),
            ],
            &[],
        );

        install_deploy_seed_archive(&archive_path, &root, "deploy-abc")
            .expect("install deploy seed");

        assert_eq!(
            fs::read(root.join("target/mach-dev/probe")).unwrap(),
            b"dev"
        );
        assert_eq!(
            fs::read(root.join("target/mach-deploy/probe")).unwrap(),
            b"deploy"
        );
        assert_eq!(
            fs::read(root.join("cargo-home/registry/src/deploy/probe")).unwrap(),
            b"deploy source"
        );
        assert_eq!(
            fs::read_to_string(root.join("deploy-seed.sha256")).unwrap(),
            "deploy-abc\n"
        );
        fs::remove_dir_all(root).expect("remove deploy seed test root");
    }

    #[test]
    fn deploy_seed_archive_rejects_conflicts_before_merging() {
        let root = test_root("deploy-seed-conflict");
        fs::create_dir_all(root.join("target")).expect("create target");
        fs::create_dir_all(root.join("cargo-home")).expect("create Cargo home");
        fs::write(root.join("cargo-home/probe"), b"base").expect("write base file");
        let archive_path = root.join("deploy-seed.tar.zst");
        write_seed_archive(
            &archive_path,
            &[
                ("target/mach-deploy/probe", b"deploy".as_slice()),
                ("cargo-home/probe", b"conflict".as_slice()),
                ("deploy-seed.json", b"{}".as_slice()),
            ],
            &[],
        );

        assert!(install_deploy_seed_archive(&archive_path, &root, "deploy-abc").is_err());
        assert!(!root.join("target/mach-deploy").exists());
        assert!(!root.join("deploy-seed.sha256").exists());
        fs::remove_dir_all(root).expect("remove deploy conflict test root");
    }

    #[test]
    fn seed_archive_rejects_unexpected_roots() {
        let root = test_root("seed-path");
        fs::create_dir_all(&root).expect("create test root");
        let archive_path = root.join("seed.tar.zst");
        write_seed_archive(&archive_path, &[("outside", b"nope")], &[]);

        assert!(install_seed_archive(&archive_path, &root, "abc").is_err());
        assert!(!root.join("outside").exists());
        assert!(!root
            .join(format!(".seed-next-{}", std::process::id()))
            .exists());
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn reconstructs_wasm_and_macho_rmeta_members() {
        let metadata = b"rust\0cached metadata";
        let wasm = ar_rmeta(&wasm_rmeta_member(metadata));
        let macho = ar_rmeta(&macho_rmeta_member(metadata));

        assert_eq!(rlib_rmeta(&wasm).unwrap(), Some(metadata.as_slice()));
        assert_eq!(rlib_rmeta(&macho).unwrap(), Some(metadata.as_slice()));
    }
}
