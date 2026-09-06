use clap::Subcommand;
use flate2::{Compression, GzBuilder};
use std::ffi::OsStr;
use std::fs::{File, Metadata};
use std::io;
use std::path::{Component, Path, PathBuf};
use tar::{Builder, EntryType, Header, HeaderMode};
use walkdir::WalkDir;

#[derive(Subcommand)]
pub enum Command {
    /// Create a deterministic tar.gz container for one signed app bundle.
    Create {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Verify deterministic metadata and safe paths in an app archive.
    Verify {
        #[arg(long)]
        archive: PathBuf,
        #[arg(long)]
        root: String,
    },
}

pub fn run(command: Command) -> Result<(), String> {
    match command {
        Command::Create { input, output } => create(&input, &output),
        Command::Verify { archive, root } => verify(&archive, &root),
    }
}

fn verify(path: &Path, expected_root: &str) -> Result<(), String> {
    if !expected_root.ends_with(".app")
        || expected_root.contains('/')
        || expected_root.contains('\\')
    {
        return Err("archive root must be one .app directory name".into());
    }
    let file = File::open(path)
        .map_err(|error| format!("cannot read archive {}: {error}", path.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let gzip_header = decoder
        .header()
        .ok_or_else(|| "archive has no gzip header".to_string())?;
    if gzip_header.mtime() != 0
        || gzip_header.filename().is_some()
        || gzip_header.comment().is_some()
    {
        return Err("archive gzip header is not deterministic".into());
    }
    let mut archive = tar::Archive::new(decoder);
    let mut previous: Option<PathBuf> = None;
    let mut saw_root = false;
    for entry in archive
        .entries()
        .map_err(|error| format!("cannot enumerate archive: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("invalid archive entry: {error}"))?;
        let entry_path = entry
            .path()
            .map_err(|error| format!("invalid archive path: {error}"))?
            .into_owned();
        validate_archive_path(&entry_path, expected_root)?;
        if previous.as_ref().is_some_and(|value| value >= &entry_path) {
            return Err("archive entries are not uniquely sorted".into());
        }
        previous = Some(entry_path.clone());
        let header = entry.header();
        let entry_type = header.entry_type();
        if entry_path == Path::new(expected_root) {
            if !entry_type.is_dir() {
                return Err("archive root is not a directory".into());
            }
            saw_root = true;
        }
        let uid = header.uid().map_err(|error| error.to_string())?;
        let gid = header.gid().map_err(|error| error.to_string())?;
        let mtime = header.mtime().map_err(|error| error.to_string())?;
        let mode = header.mode().map_err(|error| error.to_string())? & 0o777;
        if uid != 0 || gid != 0 || mtime != 0 {
            return Err(format!(
                "archive metadata is not deterministic: {}",
                entry_path.display()
            ));
        }
        match entry_type {
            kind if kind.is_dir() && mode == 0o755 => {}
            kind if kind.is_file() && matches!(mode, 0o644 | 0o755) => {}
            kind if kind.is_symlink() && mode == 0o777 => {
                let target = entry
                    .link_name()
                    .map_err(|error| format!("invalid archive symlink: {error}"))?
                    .ok_or_else(|| "archive symlink has no target".to_string())?;
                validate_link(&entry_path, &target)?;
            }
            _ => {
                return Err(format!(
                    "archive entry type or mode is invalid: {}",
                    entry_path.display()
                ))
            }
        }
        io::copy(&mut entry, &mut io::sink())
            .map_err(|error| format!("cannot read archive entry: {error}"))?;
    }
    if !saw_root {
        return Err("archive is missing its root app directory".into());
    }
    println!("verified deterministic app archive: {}", path.display());
    Ok(())
}

fn validate_archive_path(path: &Path, expected_root: &str) -> Result<(), String> {
    path.to_str()
        .ok_or_else(|| format!("archive path is not UTF-8: {}", path.display()))?;
    let mut components = path.components();
    if components.next() != Some(Component::Normal(OsStr::new(expected_root)))
        || components.any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "archive path escapes its app root: {}",
            path.display()
        ));
    }
    Ok(())
}

fn create(input: &Path, output: &Path) -> Result<(), String> {
    let input_metadata = std::fs::symlink_metadata(input)
        .map_err(|error| format!("cannot inspect {}: {error}", input.display()))?;
    if !input_metadata.is_dir() || input_metadata.file_type().is_symlink() {
        return Err(format!(
            "archive input is not a regular directory: {}",
            input.display()
        ));
    }
    let root_name = input
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| name.ends_with(".app") && !name.contains('/'))
        .ok_or_else(|| "archive input must be a UTF-8 .app bundle".to_string())?;
    let input = input
        .canonicalize()
        .map_err(|error| format!("cannot resolve archive input: {error}"))?;
    let output_parent = output
        .parent()
        .ok_or_else(|| "archive output has no parent".to_string())?;
    std::fs::create_dir_all(output_parent)
        .map_err(|error| format!("cannot create {}: {error}", output_parent.display()))?;
    let output_parent = output_parent
        .canonicalize()
        .map_err(|error| format!("cannot resolve archive output directory: {error}"))?;
    if output_parent.starts_with(&input) {
        return Err("archive output cannot be inside the input bundle".into());
    }

    let mut entries = Vec::new();
    for entry in WalkDir::new(&input).follow_links(false) {
        let entry = entry.map_err(|error| format!("cannot enumerate app bundle: {error}"))?;
        let relative = entry
            .path()
            .strip_prefix(&input)
            .map_err(|_| "archive entry escaped its input root".to_string())?;
        let archive_path = if relative.as_os_str().is_empty() {
            PathBuf::from(root_name)
        } else {
            Path::new(root_name).join(relative)
        };
        archive_path
            .to_str()
            .ok_or_else(|| format!("archive path is not UTF-8: {}", archive_path.display()))?;
        entries.push((archive_path, entry.path().to_path_buf()));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    let output_name = output
        .file_name()
        .ok_or_else(|| "archive output has no filename".to_string())?;
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{}.", output_name.to_string_lossy()))
        .tempfile_in(&output_parent)
        .map_err(|error| format!("cannot create archive output: {error}"))?;
    {
        let encoder = GzBuilder::new()
            .mtime(0)
            .write(temporary.as_file_mut(), Compression::best());
        let mut archive = Builder::new(encoder);
        archive.mode(HeaderMode::Deterministic);
        for (archive_path, source_path) in entries {
            append(&mut archive, &archive_path, &source_path)?;
        }
        let encoder = archive
            .into_inner()
            .map_err(|error| format!("cannot finish tar stream: {error}"))?;
        encoder
            .finish()
            .map_err(|error| format!("cannot finish gzip stream: {error}"))?;
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("cannot sync archive output: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o644))
            .map_err(|error| format!("cannot set archive permissions: {error}"))?;
    }
    temporary
        .persist(output)
        .map_err(|error| format!("cannot commit {}: {}", output.display(), error.error))?;
    println!("created deterministic app archive: {}", output.display());
    Ok(())
}

fn append<W: io::Write>(
    archive: &mut Builder<W>,
    archive_path: &Path,
    source_path: &Path,
) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(source_path)
        .map_err(|error| format!("cannot inspect {}: {error}", source_path.display()))?;
    let mut header = Header::new_ustar();
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    if metadata.is_dir() {
        header.set_entry_type(EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        header.set_cksum();
        archive
            .append_data(&mut header, archive_path, io::empty())
            .map_err(|error| format!("cannot archive {}: {error}", source_path.display()))?;
    } else if metadata.is_file() {
        header.set_entry_type(EntryType::Regular);
        header.set_mode(file_mode(&metadata));
        header.set_size(metadata.len());
        header.set_cksum();
        let file = File::open(source_path)
            .map_err(|error| format!("cannot read {}: {error}", source_path.display()))?;
        archive
            .append_data(&mut header, archive_path, file)
            .map_err(|error| format!("cannot archive {}: {error}", source_path.display()))?;
    } else if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(source_path)
            .map_err(|error| format!("cannot read symlink {}: {error}", source_path.display()))?;
        validate_link(archive_path, &target)?;
        header.set_entry_type(EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        header
            .set_link_name(&target)
            .map_err(|error| format!("invalid symlink target {}: {error}", target.display()))?;
        header.set_cksum();
        archive
            .append_data(&mut header, archive_path, io::empty())
            .map_err(|error| format!("cannot archive {}: {error}", source_path.display()))?;
    } else {
        return Err(format!(
            "unsupported app-bundle entry: {}",
            source_path.display()
        ));
    }
    Ok(())
}

fn validate_link(archive_path: &Path, target: &Path) -> Result<(), String> {
    if target.as_os_str().is_empty() || target.is_absolute() {
        return Err(format!(
            "unsafe symlink in archive: {}",
            archive_path.display()
        ));
    }
    let mut depth = archive_path
        .parent()
        .map(|parent| parent.components().count())
        .unwrap_or(0);
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 1 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "symlink escapes app bundle: {}",
                    archive_path.display()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn file_mode(metadata: &Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 == 0 {
        0o644
    } else {
        0o755
    }
}

#[cfg(not(unix))]
fn file_mode(_: &Metadata) -> u32 {
    0o644
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn archive_is_byte_reproducible_across_source_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first/mix.app");
        let second = directory.path().join("second/mix.app");
        make_fixture(&first);
        make_fixture(&second);
        let first_archive = directory.path().join("first.tar.gz");
        let second_archive = directory.path().join("second.tar.gz");
        create(&first, &first_archive).unwrap();
        create(&second, &second_archive).unwrap();
        verify(&first_archive, "mix.app").unwrap();
        verify(&second_archive, "mix.app").unwrap();
        assert_eq!(digest(&first_archive), digest(&second_archive));
    }

    #[test]
    fn archive_rejects_a_symlink_that_escapes_the_bundle() {
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join("mix.app");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("../../outside", bundle.join("Contents/escape")).unwrap();
        #[cfg(unix)]
        assert!(create(&bundle, &directory.path().join("mix.tar.gz")).is_err());
    }

    fn make_fixture(bundle: &Path) {
        std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        let executable = bundle.join("Contents/MacOS/mix");
        std::fs::write(&executable, b"binary").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::symlink("MacOS/mix", bundle.join("Contents/current")).unwrap();
        }
    }

    fn digest(path: &Path) -> Vec<u8> {
        Sha256::digest(std::fs::read(path).unwrap()).to_vec()
    }
}
