use crate::cache::mach_cache_root;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

const MAX_STARTER_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_STARTER_EXTRACTED_BYTES: u64 = 256 * 1024 * 1024;
const SOURCE_MARKER: &str = ".mach/starter-source.sha256";
const ARTIFACT_MARKER: &str = ".mach/starter-artifact.sha256";

pub(crate) fn record_pristine_source(project_root: &Path) -> Result<(), String> {
    let checksum = source_checksum(project_root)?;
    let marker = project_root.join(SOURCE_MARKER);
    fs::create_dir_all(marker.parent().expect("starter marker has parent"))
        .map_err(|error| format!("cannot create starter state: {error}"))?;
    fs::write(&marker, format!("{checksum}\n"))
        .map_err(|error| format!("cannot record starter source: {error}"))
}

pub(crate) fn install_if_pristine(project_root: &Path) -> Result<bool, String> {
    if !source_is_pristine(project_root)? {
        return Ok(false);
    }

    let (archive, checksum) = ensure_cached()?;
    let installed = project_root.join(ARTIFACT_MARKER);
    if fs::read_to_string(&installed)
        .ok()
        .is_some_and(|value| value.trim() == checksum)
        && starter_outputs_exist(project_root)
    {
        return Ok(true);
    }
    install_archive(&archive, project_root)?;
    fs::write(&installed, format!("{checksum}\n"))
        .map_err(|error| format!("cannot record starter artifacts: {error}"))?;
    Ok(true)
}

fn source_is_pristine(project_root: &Path) -> Result<bool, String> {
    let marker = project_root.join(SOURCE_MARKER);
    let expected_source = match fs::read_to_string(&marker) {
        Ok(value) => value.trim().to_owned(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("cannot read {}: {error}", marker.display())),
    };
    if expected_source != source_checksum(project_root)? {
        return Ok(false);
    }
    Ok(true)
}

pub(crate) fn ensure_cached() -> Result<(PathBuf, String), String> {
    let platform = crate::dev::platform_id()?;
    let name = format!("mach-starter-{platform}.zip");
    let base = format!(
        "{}/v{}/{name}",
        crate::releases_url(),
        crate::ENGINE_VERSION
    );
    let expected = download_checksum(&format!("{base}.sha256"))?;
    let root = mach_cache_root()
        .ok_or_else(|| "cannot locate the mach cache directory".to_owned())?
        .join("starters")
        .join(crate::ENGINE_VERSION)
        .join(platform);
    fs::create_dir_all(&root).map_err(|error| format!("cannot create starter cache: {error}"))?;
    let archive = root.join(&name);
    if archive.is_file() && file_sha256(&archive)? == expected {
        return Ok((archive, expected));
    }

    println!(
        "mach: downloading starter world {}...",
        crate::ENGINE_VERSION
    );
    let candidate = root.join(format!("{name}.next-{}", std::process::id()));
    let result = (|| {
        download_file(&base, &candidate)?;
        let actual = file_sha256(&candidate)?;
        if actual != expected {
            return Err(format!(
                "starter checksum mismatch: expected {expected}, received {actual}"
            ));
        }
        activate_file(&candidate, &archive)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&candidate);
    }
    result.map(|()| (archive, expected))
}

fn starter_outputs_exist(project_root: &Path) -> bool {
    [".mach/bin/mach-client", ".mach/bin/mach-server"]
        .iter()
        .all(|relative| project_root.join(relative).is_file())
}

fn source_checksum(project_root: &Path) -> Result<String, String> {
    let mut inputs = Vec::new();
    for relative in [
        ".cargo",
        "Cargo.lock",
        "Cargo.toml",
        "crates",
        "rust-toolchain.toml",
        "src",
    ] {
        collect_source_files(project_root, &project_root.join(relative), &mut inputs)?;
    }
    inputs.sort();
    let mut digest = Sha256::new();
    for path in inputs {
        let relative = path
            .strip_prefix(project_root)
            .map_err(|_| "starter source escaped the project".to_owned())?;
        let relative = relative.to_string_lossy();
        let data = fs::read(&path)
            .map_err(|error| format!("cannot read starter source {}: {error}", path.display()))?;
        digest.update((relative.len() as u64).to_le_bytes());
        digest.update(relative.as_bytes());
        digest.update((data.len() as u64).to_le_bytes());
        digest.update(data);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn collect_source_files(root: &Path, path: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "starter source cannot contain symlink {}",
            path.display()
        ));
    }
    if metadata.is_file() {
        if !path.starts_with(root) {
            return Err("starter source escaped the project".to_owned());
        }
        files.push(path.to_path_buf());
        return Ok(());
    }
    let mut entries = fs::read_dir(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        collect_source_files(root, &entry.path(), files)?;
    }
    Ok(())
}

fn install_archive(archive_path: &Path, project_root: &Path) -> Result<(), String> {
    let file = fs::File::open(archive_path)
        .map_err(|error| format!("cannot open starter archive: {error}"))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| format!("starter archive is invalid: {error}"))?;
    let staging = project_root
        .join(".mach")
        .join(format!("starter-next-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging)
            .map_err(|error| format!("cannot reset starter staging: {error}"))?;
    }
    fs::create_dir_all(&staging)
        .map_err(|error| format!("cannot create starter staging: {error}"))?;
    let result = (|| {
        let mut extracted = 0_u64;
        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .map_err(|error| format!("cannot read starter archive: {error}"))?;
            let relative = entry
                .enclosed_name()
                .ok_or_else(|| "starter archive contains an unsafe path".to_owned())?;
            if !valid_artifact_path(&relative) {
                return Err(format!(
                    "starter archive contains unexpected file {}",
                    relative.display()
                ));
            }
            extracted = extracted
                .checked_add(entry.size())
                .ok_or_else(|| "starter archive is too large".to_owned())?;
            if extracted > MAX_STARTER_EXTRACTED_BYTES {
                return Err("starter archive is too large".to_owned());
            }
            let destination = staging.join(&relative);
            if entry.is_dir() {
                fs::create_dir_all(&destination)
                    .map_err(|error| format!("cannot create starter directory: {error}"))?;
                continue;
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create starter directory: {error}"))?;
            }
            let mut output = fs::File::create(&destination)
                .map_err(|error| format!("cannot create starter artifact: {error}"))?;
            io::copy(&mut entry, &mut output)
                .map_err(|error| format!("cannot extract starter artifact: {error}"))?;
        }
        for relative in ["bin/mach-client", "bin/mach-server"] {
            if !staging.join(relative).is_file() {
                return Err(format!("starter archive is missing {relative}"));
            }
        }
        let mach = project_root.join(".mach");
        {
            let name = "bin";
            let source = staging.join(name);
            let destination = mach.join(name);
            if destination.exists() {
                fs::remove_dir_all(&destination)
                    .map_err(|error| format!("cannot replace starter {name}: {error}"))?;
            }
            fs::rename(&source, &destination)
                .map_err(|error| format!("cannot install starter {name}: {error}"))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in ["mach-client", "mach-server"] {
                let executable = mach.join("bin").join(name);
                fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
                    .map_err(|error| format!("cannot make starter executable: {error}"))?;
            }
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(staging);
    result
}

fn valid_artifact_path(path: &Path) -> bool {
    let safe = path
        .components()
        .all(|component| matches!(component, Component::Normal(_)));
    safe && (path == Path::new("bin")
        || path == Path::new("bin/mach-client")
        || path == Path::new("bin/mach-server"))
}

fn download_checksum(url: &str) -> Result<String, String> {
    let response = ureq::get(url)
        .call()
        .map_err(|error| format!("cannot download starter checksum: {error}"))?;
    let mut value = String::new();
    response
        .into_reader()
        .take(4097)
        .read_to_string(&mut value)
        .map_err(|error| format!("cannot read starter checksum: {error}"))?;
    let checksum = value.split_whitespace().next().unwrap_or_default();
    if checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("starter checksum is invalid".to_owned());
    }
    Ok(checksum.to_ascii_lowercase())
}

fn download_file(url: &str, destination: &Path) -> Result<(), String> {
    let response = ureq::get(url)
        .call()
        .map_err(|error| format!("cannot download starter world: {error}"))?;
    let mut input = response.into_reader().take(MAX_STARTER_ARCHIVE_BYTES + 1);
    let mut output = fs::File::create(destination)
        .map_err(|error| format!("cannot create starter download: {error}"))?;
    let bytes = io::copy(&mut input, &mut output)
        .map_err(|error| format!("cannot save starter download: {error}"))?;
    if bytes > MAX_STARTER_ARCHIVE_BYTES {
        return Err("starter download is too large".to_owned());
    }
    output
        .flush()
        .map_err(|error| format!("cannot finish starter download: {error}"))
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

fn activate_file(candidate: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() {
        fs::remove_file(destination)
            .map_err(|error| format!("cannot replace {}: {error}", destination.display()))?;
    }
    fs::rename(candidate, destination)
        .map_err(|error| format!("cannot install {}: {error}", destination.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn pristine_marker_changes_with_compiled_source() {
        let root = std::env::temp_dir().join(format!(
            "mach-starter-source-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).expect("create source");
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"game\"\n").expect("write manifest");
        fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("write source");

        assert!(!source_is_pristine(&root).expect("missing marker"));
        record_pristine_source(&root).expect("record source");
        assert!(source_is_pristine(&root).expect("matching marker"));

        fs::write(
            root.join("src/main.rs"),
            "fn main() { println!(\"changed\"); }\n",
        )
        .expect("change source");
        assert!(!source_is_pristine(&root).expect("changed source"));
        fs::remove_dir_all(root).expect("remove test project");
    }
}
