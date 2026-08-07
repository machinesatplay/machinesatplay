use crate::mach_cache_root;
use flate2::read::GzDecoder;
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub(crate) const RUST_VERSION: &str = "1.96.0";
const MAX_HELPER_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Platform {
    id: &'static str,
    wasm_bindgen_target: &'static str,
    wasm_bindgen_sha256: &'static str,
}

const MACOS_AARCH64: Platform = Platform {
    id: "macos-aarch64",
    wasm_bindgen_target: "aarch64-apple-darwin",
    wasm_bindgen_sha256: "7df536babe345deb68828148dbdc71179118afdab42d83547c7cebfbf1426bd5",
};
const MACOS_X86_64: Platform = Platform {
    id: "macos-x86_64",
    wasm_bindgen_target: "x86_64-apple-darwin",
    wasm_bindgen_sha256: "6014dca993c8bf8a6ec10b6fccfbeabf599842d011f58a4abb7669afef784422",
};
const LINUX_AARCH64: Platform = Platform {
    id: "linux-aarch64",
    wasm_bindgen_target: "aarch64-unknown-linux-musl",
    wasm_bindgen_sha256: "2245120254a9f6c9a9adf3601f3d52bb31309219e9ceab7696e74e24885c440a",
};
const LINUX_X86_64: Platform = Platform {
    id: "linux-x86_64",
    wasm_bindgen_target: "x86_64-unknown-linux-musl",
    wasm_bindgen_sha256: "064948d58e2d6c0a745216477a639ba696216d6309aaa902939d1b865b1d869d",
};
const WINDOWS_X86_64: Platform = Platform {
    id: "windows-x86_64",
    wasm_bindgen_target: "x86_64-pc-windows-msvc",
    wasm_bindgen_sha256: "5a3773c7e69cfb2d865e235e9210de184c8c3af1787720646ec1a8bbe09c6179",
};

pub(crate) struct Status {
    pub(crate) cargo: Option<String>,
    pub(crate) rustup: Option<String>,
    pub(crate) wasm_bindgen: Option<String>,
    pub(crate) wasm_target: bool,
}

impl Status {
    pub(crate) fn native_ready(&self) -> bool {
        self.cargo.is_some() && self.rustup.is_some()
    }

    pub(crate) fn ready(&self) -> bool {
        self.native_ready()
            && self.wasm_target
            && self.wasm_bindgen.as_deref()
                == Some(&format!("wasm-bindgen {}", crate::WASM_BINDGEN_VERSION))
    }
}

pub(crate) fn ensure() -> Result<(), String> {
    let (root, _lock) = locked_tools_root()?;

    if status().is_some_and(|status| status.ready()) {
        return Ok(());
    }

    ensure_rust()?;
    ensure_wasm_bindgen(&root)?;
    let status = status().ok_or_else(|| "cannot inspect local build tools".to_owned())?;
    if status.ready() {
        Ok(())
    } else {
        Err("local build tools did not pass their version checks; run `mach doctor`".to_owned())
    }
}

pub(crate) fn ensure_base() -> Result<(), String> {
    let (root, _lock) = locked_tools_root()?;
    ensure_rust()?;
    ensure_wasm_bindgen(&root)
}

pub(crate) fn ensure_native() -> Result<(), String> {
    let (_root, _lock) = locked_tools_root()?;
    ensure_native_rust()
}

pub(crate) fn ensure_rust_target(target: &str) -> Result<(), String> {
    let (_root, _lock) = locked_tools_root()?;
    ensure_native_rust()?;
    if installed_targets()
        .lines()
        .any(|line| line.trim() == target)
    {
        return Ok(());
    }
    println!("mach: installing Rust target {target}...");
    let status = Command::new(rustup_path())
        .args(["target", "add", target, "--toolchain", RUST_VERSION])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("cannot run rustup: {error}"))?;
    if status.success()
        && installed_targets()
            .lines()
            .any(|line| line.trim() == target)
    {
        Ok(())
    } else {
        Err(format!(
            "rustup could not install {target}; run `rustup target add {target} --toolchain {RUST_VERSION}`"
        ))
    }
}

fn locked_tools_root() -> Result<(PathBuf, fs::File), String> {
    current_platform()?;
    require_rustup()?;
    let root = tools_root()?;
    fs::create_dir_all(&root)
        .map_err(|error| format!("cannot create managed tools directory: {error}"))?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("install.lock"))
        .map_err(|error| format!("cannot open managed tools install lock: {error}"))?;
    lock.lock_exclusive()
        .map_err(|error| format!("cannot lock managed tools installation: {error}"))?;
    Ok((root, lock))
}

pub(crate) fn status() -> Option<Status> {
    current_platform().ok()?;
    let cargo = configured_command(cargo_path())
        .ok()
        .and_then(|mut command| command_version(&mut command, &["--version"]));
    let rustup = command_version(&mut Command::new(rustup_path()), &["--version"]);
    let targets = installed_targets();
    Some(Status {
        cargo,
        rustup,
        wasm_bindgen: command_version(
            &mut configured_command(wasm_bindgen_path().ok()?).ok()?,
            &["--version"],
        ),
        wasm_target: targets
            .lines()
            .any(|line| line.trim() == "wasm32-unknown-unknown"),
    })
}

pub(crate) fn cargo_command() -> Result<Command, String> {
    configured_command(cargo_path())
}

pub(crate) fn configure(command: &mut Command) -> Result<(), String> {
    command
        .env("RUSTUP_TOOLCHAIN", RUST_VERSION)
        .env("PATH", managed_path(&tools_root()?)?);
    Ok(())
}

fn configured_command(program: impl AsRef<Path>) -> Result<Command, String> {
    let mut command = Command::new(program.as_ref());
    configure(&mut command)?;
    Ok(command)
}

fn command_version(command: &mut Command, arguments: &[&str]) -> Option<String> {
    command
        .args(arguments)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn require_rustup() -> Result<(), String> {
    if command_version(&mut Command::new(rustup_path()), &["--version"]).is_some() {
        return Ok(());
    }
    let platform_help = if cfg!(target_os = "windows") {
        " On Windows, use the recommended MSVC installation and allow rustup to install its Visual Studio prerequisites."
    } else {
        ""
    };
    Err(format!(
        "Rust is required for local builds. Install it from https://rustup.rs, then run `mach setup` again.{platform_help}"
    ))
}

fn ensure_rust() -> Result<(), String> {
    if rust_ready() {
        return Ok(());
    }
    println!("mach: installing Rust {RUST_VERSION} and wasm targets...");
    let status = Command::new(rustup_path())
        .args([
            "toolchain",
            "install",
            RUST_VERSION,
            "--profile",
            "minimal",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("cannot run rustup: {error}"))?;
    if !status.success() {
        return Err(format!(
            "rustup could not install Rust {RUST_VERSION}; run `rustup toolchain install {RUST_VERSION} --profile minimal --target wasm32-unknown-unknown`"
        ));
    }
    if !rust_ready() {
        return Err(format!(
            "Rust {RUST_VERSION} is missing required wasm targets; run `mach doctor`"
        ));
    }
    println!("mach: Rust ready");
    Ok(())
}

fn ensure_native_rust() -> Result<(), String> {
    if native_rust_ready() {
        return Ok(());
    }
    println!("mach: installing Rust {RUST_VERSION}...");
    let status = Command::new(rustup_path())
        .args(["toolchain", "install", RUST_VERSION, "--profile", "minimal"])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("cannot run rustup: {error}"))?;
    if !status.success() || !native_rust_ready() {
        return Err(format!(
            "rustup could not install Rust {RUST_VERSION}; run `rustup toolchain install {RUST_VERSION} --profile minimal`"
        ));
    }
    println!("mach: Rust ready");
    Ok(())
}

fn native_rust_ready() -> bool {
    Command::new(rustup_path())
        .args(["run", RUST_VERSION, "rustc", "--version"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| {
            String::from_utf8_lossy(&output.stdout).starts_with(&format!("rustc {RUST_VERSION} "))
        })
}

fn rust_ready() -> bool {
    let targets = installed_targets();
    native_rust_ready()
        && targets
            .lines()
            .any(|line| line.trim() == "wasm32-unknown-unknown")
}

fn installed_targets() -> String {
    Command::new(rustup_path())
        .args(["target", "list", "--installed", "--toolchain", RUST_VERSION])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
}

fn ensure_wasm_bindgen(root: &Path) -> Result<(), String> {
    let platform = current_platform()?;
    let destination = wasm_bindgen_path_at(root)?;
    let expected = format!("wasm-bindgen {}", crate::WASM_BINDGEN_VERSION);
    if command_version(&mut configured_command(&destination)?, &["--version"]).as_deref()
        == Some(expected.as_str())
    {
        return Ok(());
    }
    println!(
        "mach: installing wasm-bindgen {}...",
        crate::WASM_BINDGEN_VERSION
    );
    fs::create_dir_all(destination.parent().expect("tool path has parent"))
        .map_err(|error| format!("cannot create wasm-bindgen directory: {error}"))?;
    let archive =
        destination.with_file_name(format!("wasm-bindgen.tar.gz.next-{}", std::process::id()));
    let url = format!(
        "https://github.com/wasm-bindgen/wasm-bindgen/releases/download/{0}/wasm-bindgen-{0}-{1}.tar.gz",
        crate::WASM_BINDGEN_VERSION,
        platform.wasm_bindgen_target
    );
    download_verified(
        &url,
        &archive,
        platform.wasm_bindgen_sha256,
        MAX_HELPER_BYTES,
    )?;
    let candidate = sibling_candidate(&destination);
    let result = extract_named_tar_gz(&archive, executable_name("wasm-bindgen"), &candidate)
        .and_then(|()| make_executable(&candidate))
        .and_then(|()| activate_file(&candidate, &destination));
    let _ = fs::remove_file(&archive);
    if result.is_err() {
        let _ = fs::remove_file(&candidate);
    }
    result
}

fn download_verified(
    url: &str,
    destination: &Path,
    expected_sha256: &str,
    max_bytes: u64,
) -> Result<(), String> {
    let mut last_error = None;
    for attempt in 0..3 {
        let result = download_verified_once(url, destination, expected_sha256, max_bytes);
        match result {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                let _ = fs::remove_file(destination);
                if attempt < 2 {
                    std::thread::sleep(Duration::from_millis(250 * (attempt + 1) as u64));
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| format!("cannot download {url}")))
}

fn download_verified_once(
    url: &str,
    destination: &Path,
    expected_sha256: &str,
    max_bytes: u64,
) -> Result<(), String> {
    let response = ureq::get(url)
        .call()
        .map_err(|error| format!("cannot download {url}: {error}"))?;
    if response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|size| size > max_bytes)
    {
        return Err(format!("download from {url} exceeds {max_bytes} bytes"));
    }
    let mut reader = response.into_reader();
    let mut output = fs::File::create(destination)
        .map_err(|error| format!("cannot create {}: {error}", destination.display()))?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("cannot read {url}: {error}"))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| format!("download from {url} is too large"))?;
        if total > max_bytes {
            return Err(format!("download from {url} exceeds {max_bytes} bytes"));
        }
        hasher.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .map_err(|error| format!("cannot write {}: {error}", destination.display()))?;
    }
    output
        .sync_all()
        .map_err(|error| format!("cannot sync {}: {error}", destination.display()))?;
    let actual = format!("{:x}", hasher.finalize());
    if actual.eq_ignore_ascii_case(expected_sha256) {
        Ok(())
    } else {
        Err(format!(
            "checksum mismatch for {url}: expected {expected_sha256}, received {actual}"
        ))
    }
}

fn extract_named_tar_gz(archive: &Path, name: &str, destination: &Path) -> Result<(), String> {
    let input = fs::File::open(archive)
        .map_err(|error| format!("cannot open {}: {error}", archive.display()))?;
    let mut archive = tar::Archive::new(GzDecoder::new(input));
    for entry in archive
        .entries()
        .map_err(|error| format!("cannot read helper archive: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("cannot read helper archive: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("cannot read helper archive path: {error}"))?;
        let safe = path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir));
        if safe && path.file_name().is_some_and(|file| file == name) {
            let mut output = fs::File::create(destination)
                .map_err(|error| format!("cannot create {}: {error}", destination.display()))?;
            std::io::copy(&mut entry, &mut output)
                .map_err(|error| format!("cannot extract {name}: {error}"))?;
            return Ok(());
        }
    }
    Err(format!("helper archive does not contain {name}"))
}

fn activate_file(candidate: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() {
        fs::remove_file(destination)
            .map_err(|error| format!("cannot replace {}: {error}", destination.display()))?;
    }
    fs::rename(candidate, destination)
        .map_err(|error| format!("cannot install {}: {error}", destination.display()))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("cannot make {} executable: {error}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn managed_path(root: &Path) -> Result<OsString, String> {
    let mut paths = vec![wasm_bindgen_path_at(root)?
        .parent()
        .expect("tool path has parent")
        .to_path_buf()];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(paths).map_err(|error| format!("cannot configure managed PATH: {error}"))
}

fn tools_root() -> Result<PathBuf, String> {
    Ok(mach_cache_root()
        .ok_or_else(|| "cannot locate the mach cache directory".to_owned())?
        .join("tools"))
}

fn cargo_path() -> PathBuf {
    PathBuf::from(executable_name("cargo"))
}

fn rustup_path() -> PathBuf {
    PathBuf::from(executable_name("rustup"))
}

fn wasm_bindgen_path() -> Result<PathBuf, String> {
    wasm_bindgen_path_at(&tools_root()?)
}

fn wasm_bindgen_path_at(root: &Path) -> Result<PathBuf, String> {
    Ok(root
        .join("wasm-bindgen")
        .join(crate::WASM_BINDGEN_VERSION)
        .join(current_platform()?.id)
        .join(executable_name("wasm-bindgen")))
}

fn executable_name(name: &'static str) -> &'static str {
    match name {
        "cargo" if cfg!(target_os = "windows") => "cargo.exe",
        "rustup" if cfg!(target_os = "windows") => "rustup.exe",
        "wasm-bindgen" if cfg!(target_os = "windows") => "wasm-bindgen.exe",
        _ => name,
    }
}

fn sibling_candidate(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .expect("tool path has a name")
        .to_string_lossy();
    destination.with_file_name(format!("{name}.next-{}", std::process::id()))
}

fn current_platform() -> Result<Platform, String> {
    platform_for(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
        format!(
            "local builds are unavailable on {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })
}

fn platform_for(os: &str, arch: &str) -> Option<Platform> {
    match (os, arch) {
        ("macos", "aarch64") => Some(MACOS_AARCH64),
        ("macos", "x86_64") => Some(MACOS_X86_64),
        ("linux", "aarch64") => Some(LINUX_AARCH64),
        ("linux", "x86_64") => Some(LINUX_X86_64),
        ("windows", "x86_64") => Some(WINDOWS_X86_64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_tool_platforms_match_release_platforms() {
        assert_eq!(platform_for("macos", "aarch64"), Some(MACOS_AARCH64));
        assert_eq!(platform_for("macos", "x86_64"), Some(MACOS_X86_64));
        assert_eq!(platform_for("linux", "aarch64"), Some(LINUX_AARCH64));
        assert_eq!(platform_for("linux", "x86_64"), Some(LINUX_X86_64));
        assert_eq!(platform_for("windows", "x86_64"), Some(WINDOWS_X86_64));
        assert_eq!(platform_for("windows", "aarch64"), None);
    }

    #[test]
    fn managed_tool_paths_are_versioned() {
        let root = Path::new("/tmp/mach/tools");
        assert!(wasm_bindgen_path_at(root).unwrap().ends_with(format!(
            "wasm-bindgen/0.2.126/{}/{}",
            current_platform().unwrap().id,
            executable_name("wasm-bindgen")
        )));
    }
}
