use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_CLI_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CLI_BYTES: u64 = 128 * 1024 * 1024;

pub(crate) fn update_and_reexec() -> Result<(), String> {
    if !official_release() || std::env::var_os("MACH_SKIP_UPDATE").is_some() {
        return Ok(());
    }

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(1))
        .timeout_read(Duration::from_secs(2))
        .timeout_write(Duration::from_secs(2))
        .build();
    let Some(latest) = fetch_latest_version(&agent) else {
        return Ok(());
    };
    if !is_newer(&latest, env!("CARGO_PKG_VERSION"))? {
        return Ok(());
    }
    let update_started = Instant::now();

    let platform = crate::dev::platform_id()?;
    let archive_name = format!("mach-cli-{platform}.zip");
    let base = format!("{}/v{latest}/{archive_name}", crate::releases_url());
    let expected = download_checksum(&agent, &format!("{base}.sha256"))?;
    let executable = std::env::current_exe()
        .map_err(|error| format!("cannot locate the current executable: {error}"))?;
    let install_dir = executable
        .parent()
        .ok_or_else(|| "cannot locate the CLI install directory".to_owned())?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let staging = install_dir.join(format!(".mach-update-{}-{nonce}", std::process::id()));
    fs::create_dir(&staging)
        .map_err(|error| format!("cannot create update staging directory: {error}"))?;
    let result = (|| {
        let archive = staging.join(&archive_name);
        download_file(&agent, &base, &archive)?;
        let actual = file_sha256(&archive)?;
        if actual != expected {
            return Err(format!(
                "CLI checksum mismatch: expected {expected}, received {actual}"
            ));
        }
        let candidate = staging.join("mach");
        extract_cli(&archive, &candidate)?;
        fs::rename(&candidate, &executable)
            .map_err(|error| format!("cannot replace {}: {error}", executable.display()))?;
        Ok(())
    })();
    let _ = fs::remove_dir_all(&staging);
    result?;

    eprintln!("mach: updated {} to {latest}", env!("CARGO_PKG_VERSION"));
    crate::telemetry::update_summary(env!("CARGO_PKG_VERSION"), &latest, update_started.elapsed());
    crate::telemetry::flush();
    reexec(&executable)
}

fn official_release() -> bool {
    option_env!("MACH_OFFICIAL_RELEASE") == Some("1")
}

fn fetch_latest_version(agent: &ureq::Agent) -> Option<String> {
    let response = agent
        .get(&format!("{}/latest/version", crate::releases_url()))
        .call()
        .ok()?;
    let mut value = String::new();
    response
        .into_reader()
        .take(129)
        .read_to_string(&mut value)
        .ok()?;
    let value = value.trim();
    parse_version(value).map(|_| value.to_owned())
}

fn is_newer(candidate: &str, current: &str) -> Result<bool, String> {
    let candidate = parse_version(candidate)
        .ok_or_else(|| format!("latest CLI version is invalid: {candidate}"))?;
    let current = parse_version(current)
        .ok_or_else(|| format!("current CLI version is invalid: {current}"))?;
    Ok(candidate > current)
}

fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let mut parts = value.split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(version)
}

fn download_checksum(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    let response = agent
        .get(url)
        .call()
        .map_err(|error| format!("cannot download CLI checksum: {error}"))?;
    let mut value = String::new();
    response
        .into_reader()
        .take(4097)
        .read_to_string(&mut value)
        .map_err(|error| format!("cannot read CLI checksum: {error}"))?;
    let checksum = value.split_whitespace().next().unwrap_or_default();
    if checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("CLI checksum is invalid".to_owned());
    }
    Ok(checksum.to_ascii_lowercase())
}

fn download_file(agent: &ureq::Agent, url: &str, destination: &Path) -> Result<(), String> {
    let response = agent
        .get(url)
        .call()
        .map_err(|error| format!("cannot download the latest CLI: {error}"))?;
    let mut input = response.into_reader().take(MAX_CLI_ARCHIVE_BYTES + 1);
    let mut output = fs::File::create(destination)
        .map_err(|error| format!("cannot create CLI download: {error}"))?;
    let bytes = io::copy(&mut input, &mut output)
        .map_err(|error| format!("cannot save CLI download: {error}"))?;
    if bytes > MAX_CLI_ARCHIVE_BYTES {
        return Err("CLI download is too large".to_owned());
    }
    output
        .flush()
        .map_err(|error| format!("cannot finish CLI download: {error}"))
}

fn extract_cli(archive_path: &Path, destination: &Path) -> Result<(), String> {
    let file = fs::File::open(archive_path)
        .map_err(|error| format!("cannot open CLI archive: {error}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|error| format!("CLI archive is invalid: {error}"))?;
    if archive.len() != 1 {
        return Err("CLI archive contains unexpected files".to_owned());
    }
    let mut entry = archive
        .by_index(0)
        .map_err(|error| format!("cannot read CLI archive: {error}"))?;
    let path = entry
        .enclosed_name()
        .ok_or_else(|| "CLI archive contains an unsafe path".to_owned())?;
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
        || path != Path::new("mach")
        || entry.is_dir()
        || entry.size() > MAX_CLI_BYTES
    {
        return Err("CLI archive contains an unexpected file".to_owned());
    }
    let mut output = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(|error| format!("cannot create updated CLI: {error}"))?;
    io::copy(&mut entry, &mut output)
        .map_err(|error| format!("cannot extract updated CLI: {error}"))?;
    output
        .flush()
        .map_err(|error| format!("cannot finish updated CLI: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("cannot make updated CLI executable: {error}"))?;
    }
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String, String> {
    let mut input =
        fs::File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(unix)]
fn reexec(executable: &Path) -> Result<(), String> {
    use std::os::unix::process::CommandExt;

    let error = Command::new(executable)
        .args(std::env::args_os().skip(1))
        .env("MACH_SKIP_UPDATE", "1")
        .exec();
    Err(format!("cannot restart the updated CLI: {error}"))
}

#[cfg(not(unix))]
fn reexec(_executable: &Path) -> Result<(), String> {
    Err("automatic restart is not supported on this platform".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_release_versions_numerically() {
        assert!(is_newer("0.1.15", "0.1.14").unwrap());
        assert!(is_newer("0.10.0", "0.9.99").unwrap());
        assert!(!is_newer("0.1.14", "0.1.14").unwrap());
        assert!(!is_newer("0.1.13", "0.1.14").unwrap());
        assert!(is_newer("0.1", "0.1.14").is_err());
        assert!(is_newer("latest", "0.1.14").is_err());
    }
}
