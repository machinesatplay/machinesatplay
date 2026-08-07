use clap::Subcommand;
use fs2::FileExt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const AUTOMATIC_PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_PROJECT_BUILD_CACHES: usize = 3;

#[derive(Subcommand)]
pub(crate) enum CacheCommand {
    /// Print cache locations and their current sizes.
    Size,
    /// Remove downloaded tools, engines, and build artifacts.
    Clean {
        /// Show what would be removed without changing the cache.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print the active mach cache directory.
    Dir,
}

pub(crate) fn cache_command(command: CacheCommand) -> Result<(), String> {
    match command {
        CacheCommand::Dir => {
            let root = cache_root()?;
            println!("{}", root.display());
            Ok(())
        }
        CacheCommand::Size => cache_size_command(),
        CacheCommand::Clean { dry_run } => cache_clean_command(dry_run),
    }
}

pub(crate) fn lock_cache_shared() -> Result<fs::File, String> {
    let root = cache_root()?;
    let file = open_cache_lock(&root)?;
    FileExt::lock_shared(&file)
        .map_err(|error| format!("cannot lock cache {}: {error}", root.display()))?;
    Ok(file)
}

pub(crate) fn maybe_prune_stale_cache() -> Result<(), String> {
    let root = cache_root()?;
    validate_cache_root(&root)?;
    let marker = root.join("cache-gc-at");
    let recently_pruned = fs::metadata(&marker)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < AUTOMATIC_PRUNE_INTERVAL);
    if recently_pruned {
        return Ok(());
    }
    let Some(_lock) = try_lock_cache_exclusive(&root)? else {
        return Ok(());
    };

    prune_version_directories(&root.join("build"), crate::ENGINE_VERSION)?;
    prune_version_directories(&root.join("starters"), crate::ENGINE_VERSION)?;
    for legacy in ["build-cache", "engines", "hosts"] {
        remove_cache_path(&root.join(legacy))?;
    }
    prune_tool_versions(
        &root.join("tools/wasm-bindgen"),
        crate::WASM_BINDGEN_VERSION,
    )?;
    prune_external_build_roots()?;
    if let Some(build_root) = crate::build_seed::external_build_root()? {
        prune_project_build_caches(&build_root, MAX_PROJECT_BUILD_CACHES)?;
    }
    fs::write(marker, b"automatic cache prune\n")
        .map_err(|error| format!("cannot record cache cleanup: {error}"))
}

fn prune_version_directories(root: &Path, keep: &str) -> Result<(), String> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot read {}: {error}", root.display())),
    };
    for entry in entries {
        let path = entry
            .map_err(|error| format!("cannot read {}: {error}", root.display()))?
            .path();
        if path.file_name().and_then(|name| name.to_str()) != Some(keep) {
            remove_cache_path(&path)?;
        }
    }
    Ok(())
}

fn prune_tool_versions(root: &Path, keep: &str) -> Result<(), String> {
    prune_version_directories(root, keep)
}

fn prune_external_build_roots() -> Result<(), String> {
    let Some(current) = crate::build_seed::external_build_root()? else {
        return Ok(());
    };
    let Some(parent) = current.parent() else {
        return Ok(());
    };
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot read {}: {error}", parent.display())),
    };
    for entry in entries {
        let path = entry
            .map_err(|error| format!("cannot read {}: {error}", parent.display()))?
            .path();
        let stale = path != current
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("mach-build-") && name.ends_with("-macos-aarch64")
                });
        if stale && owned_by_current_user(&path) {
            remove_cache_path(&path)?;
        }
    }
    Ok(())
}

fn prune_project_build_caches(build_root: &Path, keep: usize) -> Result<(), String> {
    let lock_path = build_root.join("build.lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("cannot open {}: {error}", lock_path.display()))?;
    if FileExt::try_lock_exclusive(&lock).is_err() {
        return Ok(());
    }
    let projects = build_root.join("projects");
    let entries = match fs::read_dir(&projects) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot read {}: {error}", projects.display())),
    };
    let mut caches = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| format!("cannot read {}: {error}", projects.display()))?
            .path();
        let marker = path.join("target/.mach-overlay");
        let source_exists = fs::read_to_string(&marker)
            .ok()
            .and_then(|value| value.lines().next().map(PathBuf::from))
            .is_some_and(|source| source.is_dir());
        if !source_exists {
            remove_cache_path(&path)?;
            continue;
        }
        let modified = fs::metadata(marker)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        caches.push((modified, path));
    }
    caches.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, path) in caches.into_iter().skip(keep) {
        remove_cache_path(&path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn owned_by_current_user(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path)
        .ok()
        .is_some_and(|metadata| metadata.uid() == unsafe { libc::geteuid() })
}

#[cfg(not(unix))]
fn owned_by_current_user(_path: &Path) -> bool {
    false
}

pub(crate) fn mach_cache_root() -> Option<PathBuf> {
    std::env::var_os("MACH_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(|| user_home().map(|home| home.join(".mach")))
}

pub(crate) fn shared_cargo_target_dir() -> Result<PathBuf, String> {
    let target = cache_root()?
        .join("build")
        .join(crate::ENGINE_VERSION)
        .join("cargo-target");
    fs::create_dir_all(&target)
        .map_err(|error| format!("cannot create shared build cache: {error}"))?;
    Ok(target)
}

fn cache_root() -> Result<PathBuf, String> {
    mach_cache_root().ok_or_else(|| "cannot locate the user cache directory".to_owned())
}

fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn validate_cache_root(root: &Path) -> Result<(), String> {
    if root.as_os_str().is_empty() || root.parent().is_none() || root.components().count() < 2 {
        return Err(format!("refusing unsafe cache path {}", root.display()));
    }
    if user_home().as_deref() == Some(root) {
        return Err("refusing to use the home directory as a cache".to_owned());
    }
    if std::env::current_dir().ok().as_deref() == Some(root) {
        return Err("refusing to use the project directory as a cache".to_owned());
    }
    if let Ok(resolved) = root.canonicalize() {
        let resolved_home = user_home().and_then(|home| home.canonicalize().ok());
        let resolved_project = std::env::current_dir()
            .ok()
            .and_then(|project| project.canonicalize().ok());
        if resolved.parent().is_none()
            || resolved_home.as_deref() == Some(&resolved)
            || resolved_project.as_deref() == Some(&resolved)
        {
            return Err(format!("refusing unsafe cache path {}", root.display()));
        }
    }
    Ok(())
}

fn open_cache_lock(root: &Path) -> Result<fs::File, String> {
    validate_cache_root(root)?;
    fs::create_dir_all(root)
        .map_err(|error| format!("cannot create cache directory {}: {error}", root.display()))?;
    let path = root.join("cache.lock");
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| format!("cannot open cache lock {}: {error}", path.display()))
}

fn try_lock_cache_exclusive(root: &Path) -> Result<Option<fs::File>, String> {
    let file = open_cache_lock(root)?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(format!("cannot lock cache {}: {error}", root.display())),
    }
}

fn cache_size_command() -> Result<(), String> {
    let root = cache_root()?;
    validate_cache_root(&root)?;
    let _lock = lock_cache_shared()?;
    let root_size = directory_size(&root)?;
    let external = crate::build_seed::external_build_root()?;
    let external_size = external
        .as_deref()
        .map(directory_size)
        .transpose()?
        .unwrap_or(0);
    println!("{}  total", human_bytes(root_size + external_size));
    println!("  {:>9}  {}", human_bytes(root_size), root.display());
    for path in removable_cache_entries(&root)? {
        let size = directory_size(&path)?;
        if size > 0 {
            println!(
                "  {:>9}  {}",
                human_bytes(size),
                path.file_name().unwrap_or_default().to_string_lossy()
            );
        }
    }
    if let Some(path) = external.filter(|path| path.exists()) {
        println!(
            "  {:>9}  macos build cache ({})",
            human_bytes(external_size),
            path.display()
        );
    }
    Ok(())
}

fn cache_clean_command(dry_run: bool) -> Result<(), String> {
    let root = cache_root()?;
    let _lock = try_lock_cache_exclusive(&root)?.ok_or_else(|| {
        "cache is in use by mach dev, mach deploy, or another cache command".to_owned()
    })?;
    let mut reclaimed = clean_cache_root(&root, dry_run)?;
    if let Some(path) = crate::build_seed::external_build_root()?.filter(|path| path.exists()) {
        let size = directory_size(&path)?;
        println!("  {}  {}", human_bytes(size), path.display());
        reclaimed = reclaimed.saturating_add(size);
        if !dry_run {
            remove_cache_path(&path)?;
        }
    }
    let action = if dry_run {
        "would reclaim"
    } else {
        "reclaimed"
    };
    println!("mach: clean {action} {}", human_bytes(reclaimed));
    Ok(())
}

fn clean_cache_root(root: &Path, dry_run: bool) -> Result<u64, String> {
    validate_cache_root(root)?;
    let mut reclaimed = 0u64;
    for path in removable_cache_entries(root)? {
        let size = directory_size(&path)?;
        println!("  {}  {}", human_bytes(size), path.display());
        reclaimed = reclaimed.saturating_add(size);
        if !dry_run {
            remove_cache_path(&path)?;
        }
    }
    Ok(reclaimed)
}

fn removable_cache_entries(root: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("cannot read {}: {error}", root.display())),
    };
    let mut paths = entries
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| format!("cannot read {}: {error}", root.display()))
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|path| {
            !matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("auth.json" | "cache.lock")
            )
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn directory_size(path: &Path) -> Result<u64, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    let mut size = 0u64;
    for entry in
        fs::read_dir(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?
    {
        let entry = entry.map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        size = size.saturating_add(directory_size(&entry.path())?);
    }
    Ok(size)
}

fn remove_cache_path(path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .map_err(|error| format!("cannot remove {}: {error}", path.display()))
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn cache_clean_removes_every_cache_category_and_preserves_auth() {
        let root = unique_test_root("cache-clean");
        let cache_entries = ["build", "engines", "hosts", "tools", "anything-else"];
        for name in cache_entries {
            let path = root.join(name);
            fs::create_dir_all(&path).expect("create cache directory");
            fs::write(path.join("data"), vec![0; 64]).expect("write cache data");
        }
        fs::write(root.join("auth.json"), "saved login").expect("write auth");

        assert_eq!(
            clean_cache_root(&root, true).expect("preview clean"),
            cache_entries.len() as u64 * 64
        );
        assert!(root.join("tools/data").is_file());
        clean_cache_root(&root, false).expect("clean cache");
        for name in cache_entries {
            assert!(!root.join(name).exists());
        }
        assert_eq!(
            fs::read_to_string(root.join("auth.json")).expect("read preserved auth"),
            "saved login"
        );

        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn cache_clean_does_not_take_a_cache_in_active_use() {
        let root = unique_test_root("cache-lock");
        let active = open_cache_lock(&root).expect("open active cache lock");
        FileExt::lock_shared(&active).expect("lock active cache");

        assert!(try_lock_cache_exclusive(&root)
            .expect("try cache cleanup lock")
            .is_none());

        FileExt::unlock(&active).expect("release active cache lock");
        drop(active);
        assert!(try_lock_cache_exclusive(&root)
            .expect("reuse released cache lock")
            .is_some());
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn version_prune_keeps_only_the_active_cache() {
        let root = unique_test_root("cache-versions");
        for version in ["0.1.1", "0.1.2", "0.1.3"] {
            fs::create_dir_all(root.join(version)).expect("create version cache");
            fs::write(root.join(version).join("data"), version).expect("write version cache");
        }

        prune_version_directories(&root, "0.1.3").expect("prune old versions");

        assert!(!root.join("0.1.1").exists());
        assert!(!root.join("0.1.2").exists());
        assert!(root.join("0.1.3/data").is_file());
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn project_prune_caps_valid_caches_and_removes_missing_projects() {
        let root = unique_test_root("project-cache-prune");
        let sources = unique_test_root("project-cache-sources");
        for index in 0..4 {
            let source = sources.join(index.to_string());
            fs::create_dir_all(&source).expect("create source project");
            let marker = root
                .join("projects")
                .join(index.to_string())
                .join("target/.mach-overlay");
            fs::create_dir_all(marker.parent().unwrap()).expect("create project cache");
            fs::write(&marker, source.to_string_lossy().as_bytes()).expect("write project marker");
            filetime::set_file_mtime(&marker, filetime::FileTime::from_unix_time(index as i64, 0))
                .expect("set project cache age");
        }
        let missing = root.join("projects/missing/target/.mach-overlay");
        fs::create_dir_all(missing.parent().unwrap()).expect("create missing project cache");
        fs::write(&missing, "/path/that/does/not/exist").expect("write missing project marker");

        prune_project_build_caches(&root, 2).expect("prune project caches");

        assert!(!root.join("projects/0").exists());
        assert!(!root.join("projects/1").exists());
        assert!(root.join("projects/2").exists());
        assert!(root.join("projects/3").exists());
        assert!(!root.join("projects/missing").exists());
        fs::remove_dir_all(root).expect("remove project caches");
        fs::remove_dir_all(sources).expect("remove source projects");
    }

    #[cfg(unix)]
    #[test]
    fn directory_size_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let root = unique_test_root("cache-size-symlink");
        let outside = unique_test_root("cache-size-outside");
        fs::create_dir_all(&root).expect("create cache fixture");
        fs::create_dir_all(&outside).expect("create outside fixture");
        fs::write(root.join("inside"), vec![0; 64]).expect("write inside fixture");
        fs::write(outside.join("outside"), vec![0; 512]).expect("write outside fixture");
        symlink(&outside, root.join("link")).expect("create cache symlink");

        assert_eq!(directory_size(&root).expect("measure cache"), 64);

        fs::remove_dir_all(&root).expect("remove cache fixture");
        fs::remove_dir_all(&outside).expect("remove outside fixture");
    }

    fn unique_test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mach-cli-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after the Unix epoch")
                .as_nanos()
        ))
    }
}
