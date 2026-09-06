use crate::{Error, ErrorCode, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

pub const MAX_LOCAL_JSON_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_CREDENTIAL_BYTES: usize = 2 * 1024 * 1024;

pub fn expand_home(path: &Path) -> Result<PathBuf> {
    let raw = path.as_os_str().to_string_lossy();
    if raw == "~" || raw.starts_with("~/") {
        let home = dirs::home_dir().ok_or_else(|| {
            Error::new(
                ErrorCode::MixLocalFailure,
                "the user home directory is unavailable",
            )
        })?;
        return Ok(if raw == "~" {
            home
        } else {
            home.join(&raw[2..])
        });
    }
    Ok(path.to_path_buf())
}

pub fn private_dir(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Error::invalid(
                "a Mix private directory is not a regular directory",
            ));
        }
    }
    fs::create_dir_all(path)
        .map_err(|error| Error::io("cannot create private directory", error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| Error::io("cannot protect private directory", error))?;
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| Error::io("cannot inspect private directory", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::invalid(
            "a Mix private directory is not a regular directory",
        ));
    }
    Ok(())
}

pub fn private_scoped_dir(path: &Path, root: &Path) -> Result<()> {
    validate_scoped_path(path, root)?;
    private_dir(path)
}

pub fn validate_scoped_path(path: &Path, root: &Path) -> Result<()> {
    if !path.is_absolute() || !root.is_absolute() || !path.starts_with(root) {
        return Err(Error::invalid(
            "Mix private path is outside its controlled root",
        ));
    }
    for ancestor in path.ancestors() {
        if !ancestor.starts_with(root) {
            break;
        }
        if let Ok(metadata) = fs::symlink_metadata(ancestor) {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(Error::invalid(format!(
                    "Mix private path crosses a non-directory path: {}",
                    ancestor.display()
                )));
            }
        }
        if ancestor == root {
            break;
        }
    }
    Ok(())
}

pub fn safe_relative(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(Error::invalid(format!(
            "expected a non-empty relative path: {value}"
        )));
    }
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(Error::invalid(format!("unsafe relative path: {value}")));
    }
    Ok(path.to_path_buf())
}

pub fn safe_child(root: &Path, relative: &str) -> Result<PathBuf> {
    let root = expand_home(root)?;
    if let Ok(metadata) = fs::symlink_metadata(&root) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Error::invalid(
                "a managed path root is not a regular directory",
            ));
        }
    }
    let path = root.join(safe_relative(relative)?);
    for ancestor in path.ancestors().skip(1) {
        if ancestor == root {
            break;
        }
        if let Ok(metadata) = fs::symlink_metadata(ancestor) {
            if metadata.file_type().is_symlink() {
                return Err(Error::invalid(format!(
                    "a managed path traverses a symbolic link: {}",
                    ancestor.display()
                )));
            }
        }
    }
    Ok(path)
}

pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_@%+=:,./-".contains(character))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

pub fn read_bounded(path: &Path, max_bytes: u64, label: &'static str) -> Result<Vec<u8>> {
    let file = open_regular_file(path, label)?;
    let metadata = file.metadata().map_err(|error| Error::io(label, error))?;
    if metadata.len() > max_bytes {
        return Err(Error::new(
            ErrorCode::MixValidationError,
            format!("{label} exceeds the size limit"),
        ));
    }
    let mut value = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes + 1)
        .read_to_end(&mut value)
        .map_err(|error| Error::io(label, error))?;
    if value.len() as u64 > max_bytes {
        return Err(Error::invalid(format!("{label} exceeds the size limit")));
    }
    Ok(value)
}

fn open_regular_file(path: &Path, label: &'static str) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|error| Error::io(label, error))?;
    let metadata = file.metadata().map_err(|error| Error::io(label, error))?;
    if !metadata.is_file() {
        return Err(Error::invalid(format!("{label} is not a regular file")));
    }
    Ok(file)
}

pub fn atomic_write(path: &Path, value: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::invalid("managed file has no parent"))?;
    private_dir(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| Error::io("cannot create temporary file", error))?;
    temp.write_all(value)
        .map_err(|error| Error::io("cannot write temporary file", error))?;
    temp.as_file()
        .sync_all()
        .map_err(|error| Error::io("cannot flush temporary file", error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| Error::io("cannot protect temporary file", error))?;
    }
    temp.persist(path)
        .map_err(|error| Error::io("cannot commit managed file", error.error))?;
    sync_directory(parent)?;
    Ok(())
}

pub fn atomic_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut payload = serde_json::to_vec_pretty(value).map_err(Error::from)?;
    payload.push(b'\n');
    atomic_write(path, &payload)
}

pub fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| Error::io("cannot flush directory", error))?;
    Ok(())
}

pub fn remove_durable(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io("cannot remove managed file", error)),
    }
}

pub fn open_lock(path: &Path) -> Result<File> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::invalid("lock file has no parent"))?;
    private_dir(parent)?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(Error::invalid("Mix lock path must not be a symbolic link"));
        }
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|error| Error::io("cannot open Mix lock", error))?;
    let metadata = file
        .metadata()
        .map_err(|error| Error::io("cannot inspect Mix lock", error))?;
    if !metadata.is_file() {
        return Err(Error::invalid("Mix lock path is not a regular file"));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal() {
        for value in ["", "../secret", "a/../b", "/absolute"] {
            assert!(safe_relative(value).is_err(), "{value}");
        }
        assert_eq!(
            safe_relative("sessions/a.jsonl").unwrap(),
            Path::new("sessions/a.jsonl")
        );
    }

    #[test]
    fn atomic_write_replaces_content() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested/value.json");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(fs::read(path).unwrap(), b"second");
    }

    #[test]
    fn shell_quote_never_allows_command_substitution() {
        assert_eq!(shell_quote("$(touch /tmp/pwn)"), "'$(touch /tmp/pwn)'");
        assert_eq!(shell_quote("safe/path"), "safe/path");
    }

    #[cfg(unix)]
    #[test]
    fn bounded_read_rejects_a_symbolic_link() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let link = root.path().join("link");
        fs::write(&target, b"secret").unwrap();
        symlink(&target, &link).unwrap();

        assert!(read_bounded(&link, 1024, "test file").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn scoped_private_directory_rejects_symlinked_mix_ancestor() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let controlled = root.path().join("controlled");
        let outside = root.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, &controlled).unwrap();
        assert!(private_scoped_dir(&controlled.join("child"), &controlled).is_err());
    }
}
