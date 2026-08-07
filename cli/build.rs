use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

fn main() {
    let crate_root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let repository = crate_root.parent().expect("cli has a repository parent");
    let output = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("starter.zip");
    let file = fs::File::create(&output).expect("create embedded starter");
    let mut archive = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);

    for relative in [
        "Cargo.toml",
        "rust-toolchain.toml",
        ".cargo/config.toml",
        ".gitignore",
        "src",
        "crates",
        "assets",
        "web/index.html",
    ] {
        let source = repository.join(relative);
        println!("cargo:rerun-if-changed={}", source.display());
        append(&mut archive, repository, &source, options).expect("append embedded starter");
    }
    let starter_lock = crate_root.join("starter/Cargo.lock");
    println!("cargo:rerun-if-changed={}", starter_lock.display());
    append_file_as(&mut archive, &starter_lock, "Cargo.lock", options)
        .expect("append starter lock");
    archive.finish().expect("finish embedded starter");
}

fn append_file_as(
    archive: &mut zip::ZipWriter<fs::File>,
    source: &Path,
    destination: &str,
    options: zip::write::SimpleFileOptions,
) -> io::Result<()> {
    archive.start_file(destination, options)?;
    archive.write_all(&fs::read(source)?)
}

fn append(
    archive: &mut zip::ZipWriter<fs::File>,
    root: &Path,
    source: &Path,
    options: zip::write::SimpleFileOptions,
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("starter cannot contain symlink {}", source.display()),
        ));
    }
    if metadata.is_dir() {
        let mut entries = fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            if entry.file_name() == "AGENTS.md" || entry.file_name() == "target" {
                continue;
            }
            append(archive, root, &entry.path(), options)?;
        }
        return Ok(());
    }
    let relative = source
        .strip_prefix(root)
        .expect("starter input is below repository")
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    archive.start_file(relative, options)?;
    archive.write_all(&fs::read(source)?)
}
