#![allow(missing_docs)]

use std::fs::{self, File, Metadata};
use std::io;
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use flate2::Compression;
use tar::{Builder, Header};

const TEXT_EXTENSIONS: &[&str] = &[
    "md", "toml", "py", "json", "txt", "yaml", "yml", "sha256", "sh",
];

fn should_include(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name == "README.md" || name == "SHA256SUMS")
        || path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| TEXT_EXTENSIONS.contains(&extension))
}

fn sorted_entries(path: &Path) -> io::Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

fn should_skip_directory(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(
            ".git"
                | ".catalog"
                | ".pytest_cache"
                | "__pycache__"
                | "artifacts"
                | "private"
                | "target"
        )
    )
}

fn append_header_path(
    builder: &mut Builder<GzEncoder<File>>,
    source: &Path,
    archive_path: &Path,
    metadata: &Metadata,
) -> io::Result<()> {
    let mut header = Header::new_gnu();
    header.set_path(archive_path)?;
    header.set_mode(if metadata.permissions().readonly() {
        0o444
    } else {
        0o644
    });
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(metadata.len());
    header.set_cksum();

    let mut file = File::open(source)?;
    builder.append(&header, &mut file)
}

fn append_directory(
    builder: &mut Builder<GzEncoder<File>>,
    source: &Path,
    archive_path: &Path,
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("documentation asset is a symlink: {}", source.display()),
        ));
    }
    if metadata.is_dir() {
        if should_skip_directory(source) {
            return Ok(());
        }
        let mut header = Header::new_gnu();
        header.set_path(archive_path)?;
        header.set_mode(0o755);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_size(0);
        header.set_entry_type(tar::EntryType::Directory);
        header.set_cksum();
        builder.append(&header, io::empty())?;

        for entry in sorted_entries(source)? {
            let child = entry.path();
            let child_archive_path = archive_path.join(entry.file_name());
            let child_metadata = fs::symlink_metadata(&child)?;
            if child_metadata.is_dir() {
                append_directory(builder, &child, &child_archive_path)?;
            } else if child_metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("documentation asset is a symlink: {}", child.display()),
                ));
            } else if child_metadata.is_file() && should_include(&child) {
                append_header_path(builder, &child, &child_archive_path, &child_metadata)?;
            }
        }
        Ok(())
    } else if metadata.is_file() && should_include(source) {
        append_header_path(builder, source, archive_path, &metadata)
    } else {
        Ok(())
    }
}

fn main() -> io::Result<()> {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR")
            .expect("Cargo must provide CARGO_MANIFEST_DIR to the build script"),
    );
    let source_root = manifest_dir.join("../..");
    let out_dir = PathBuf::from(
        std::env::var_os("OUT_DIR").expect("Cargo must provide OUT_DIR to the build script"),
    );
    let archive_path = out_dir.join("ygg-documentation.tar.gz");

    let canonical_assets_available = ["README.md", "docs", "examples", "sdk"]
        .iter()
        .all(|path| source_root.join(path).exists());
    let roots = if canonical_assets_available {
        vec![
            (source_root.join("README.md"), PathBuf::from("README.md")),
            (source_root.join("docs"), PathBuf::from("docs")),
            (source_root.join("examples"), PathBuf::from("examples")),
            (source_root.join("sdk"), PathBuf::from("sdk")),
        ]
    } else {
        // A crates.io package cannot contain files outside its package root.
        // Keep that package buildable, while git/path installs use the
        // canonical repository assets above and get the complete bundle.
        println!(
            "cargo:warning=Ygg documentation sources are unavailable; packaged binaries will use the published documentation URL"
        );
        vec![(manifest_dir.join("README.md"), PathBuf::from("README.md"))]
    };
    for (source, _) in &roots {
        println!("cargo:rerun-if-changed={}", source.display());
    }

    let output = File::create(&archive_path)?;
    let encoder = GzEncoder::new(output, Compression::best());
    let mut builder = Builder::new(encoder);
    for (source, archive_path_root) in roots {
        append_directory(&mut builder, &source, &archive_path_root)?;
    }
    let encoder = builder.into_inner()?;
    encoder.finish()?.sync_all()?;

    println!(
        "cargo:rustc-env=YGG_EMBEDDED_DOCS_ARCHIVE={}",
        archive_path.display()
    );
    Ok(())
}
