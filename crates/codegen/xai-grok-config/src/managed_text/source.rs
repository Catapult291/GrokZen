use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use super::{ManagedConfigError, ManagedConfigPlan};

pub(super) const MAX_SYMLINKS: usize = 40;
pub(super) const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

/// Identity of the filesystem entry a capture was taken from, compared on
/// every revalidation to catch a path whose entry was replaced in between.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FileIdentity {
    dev: u64,
    ino: u64,
}

/// Windows identity. The filesystem's own record for the entry — volume serial
/// number plus file id — is the only identity that survives a rename and
/// differs for a replacement created at the same path; directory length and
/// mtime are useless here because creating, renaming, or deleting an entry
/// inside a directory flips both. [`FileIdentity::Proxy`] is the fallback for
/// filesystems that report no file id at all.
#[cfg(not(unix))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FileIdentity {
    /// `high_res` distinguishes the 128-bit `FILE_ID_INFO` form from the
    /// legacy 64-bit `BY_HANDLE_FILE_INFORMATION` one, so the same entry
    /// captured through either form never compares equal.
    Id {
        volume: u64,
        id: u128,
        high_res: bool,
    },
    Proxy {
        is_dir: bool,
        len: u64,
        modified: Option<std::time::SystemTime>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SourceState {
    pub bytes: Option<Vec<u8>>,
    pub hash: String,
    pub mode: Option<u32>,
    pub identity: Option<FileIdentity>,
}

impl SourceState {
    pub fn text<'a>(&'a self, path: &Path) -> Result<&'a str, ManagedConfigError> {
        match self.bytes.as_deref() {
            Some(bytes) => std::str::from_utf8(bytes).map_err(|_| ManagedConfigError::UnsafePath {
                path: path.to_path_buf(),
                reason: "file is not valid UTF-8".to_owned(),
            }),
            None => Ok(""),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ParentPlan {
    parent: PathBuf,
    existing_chain: Vec<PathIdentity>,
    first_missing: Option<PathBuf>,
}

impl ParentPlan {
    pub fn capture(parent: &Path) -> Result<Self, ManagedConfigError> {
        let mut chain = Vec::new();
        let mut current = PathBuf::new();
        let mut first_missing = None;
        for component in parent.components() {
            current.push(component.as_os_str());
            if matches!(component, Component::Prefix(_) | Component::RootDir) {
                continue;
            }
            match fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() {
                        return Err(ManagedConfigError::UnsafePath {
                            path: current,
                            reason: "symlinked parent directory is not allowed".to_owned(),
                        });
                    }
                    if !metadata.is_dir() {
                        return Err(ManagedConfigError::UnsafePath {
                            path: current,
                            reason: "parent component is not a directory".to_owned(),
                        });
                    }
                    chain.push(PathIdentity {
                        path: current.clone(),
                        identity: FileIdentity::from_metadata(&current, &metadata),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    first_missing = Some(current.clone());
                    break;
                }
                Err(source) => {
                    return Err(ManagedConfigError::Read {
                        path: current,
                        source,
                    });
                }
            }
        }
        Ok(Self {
            parent: parent.to_path_buf(),
            existing_chain: chain,
            first_missing,
        })
    }

    pub fn ensure_and_anchor(&self) -> Result<ParentAnchor, ManagedConfigError> {
        self.revalidate_existing()?;
        fs::create_dir_all(&self.parent).map_err(|source| ManagedConfigError::Write {
            path: self.parent.clone(),
            source,
        })?;
        self.revalidate_existing()?;
        let current = Self::capture(&self.parent)?;
        if current.first_missing.is_some()
            || !current.existing_chain.starts_with(&self.existing_chain)
        {
            return Err(ManagedConfigError::ParentChanged(self.parent.clone()));
        }
        ParentAnchor::capture(&self.parent)
    }

    pub fn revalidate_planned(&self) -> Result<(), ManagedConfigError> {
        self.revalidate_existing()?;
        if self.first_missing.is_none() {
            let current = Self::capture(&self.parent)?;
            if current.existing_chain != self.existing_chain {
                return Err(ManagedConfigError::ParentChanged(self.parent.clone()));
            }
        }
        Ok(())
    }

    fn revalidate_existing(&self) -> Result<(), ManagedConfigError> {
        for expected in &self.existing_chain {
            let metadata = fs::symlink_metadata(&expected.path)
                .map_err(|_| ManagedConfigError::ParentChanged(expected.path.clone()))?;
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || FileIdentity::from_metadata(&expected.path, &metadata) != expected.identity
            {
                return Err(ManagedConfigError::ParentChanged(expected.path.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(super) struct ParentAnchor {
    path: PathBuf,
    identity: FileIdentity,
    /// Directory handle for the unix fsync path. Non-unix has no directory
    /// fsync here (`sync()` is a no-op below), and `File::open` on a
    /// directory is unreliable there (NotFound/PermissionDenied depending on
    /// the directory), so the handle only exists where it is used.
    #[cfg(unix)]
    directory: fs::File,
}

impl ParentAnchor {
    fn capture(path: &Path) -> Result<Self, ManagedConfigError> {
        let metadata = fs::symlink_metadata(path).map_err(|source| ManagedConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ManagedConfigError::ParentChanged(path.to_path_buf()));
        }
        #[cfg(unix)]
        let directory = fs::File::open(path).map_err(|source| ManagedConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            identity: FileIdentity::from_metadata(path, &metadata),
            #[cfg(unix)]
            directory,
        })
    }

    pub fn revalidate(&self) -> Result<(), ManagedConfigError> {
        let current = Self::capture(&self.path)?;
        if current.identity != self.identity {
            return Err(ManagedConfigError::ParentChanged(self.path.clone()));
        }
        Ok(())
    }

    pub fn sync(&self) -> Result<(), ManagedConfigError> {
        #[cfg(unix)]
        {
            self.directory
                .sync_all()
                .map_err(|source| ManagedConfigError::Sync {
                    path: self.path.clone(),
                    source,
                })
        }
        #[cfg(not(unix))]
        {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PathIdentity {
    path: PathBuf,
    identity: FileIdentity,
}

impl FileIdentity {
    /// Identity of the entry at `path`, which `metadata` was taken from. The
    /// path is only needed where the identity has to be read through a handle
    /// (Windows).
    #[cfg(unix)]
    fn from_metadata(_path: &Path, metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }

    #[cfg(not(unix))]
    fn from_metadata(path: &Path, metadata: &fs::Metadata) -> Self {
        match file_id::get_file_id(path) {
            Ok(file_id::FileId::HighRes {
                volume_serial_number,
                file_id,
            }) => Self::Id {
                volume: volume_serial_number,
                id: file_id,
                high_res: true,
            },
            Ok(file_id::FileId::LowRes {
                volume_serial_number,
                file_index,
            }) => Self::Id {
                volume: u64::from(volume_serial_number),
                id: u128::from(file_index),
                high_res: false,
            },
            // No filesystem id for this entry (or the probe failed): fall back
            // to the weaker proxy rather than pretending the entry is gone.
            _ => Self::proxy(metadata),
        }
    }

    #[cfg(not(unix))]
    fn proxy(metadata: &fs::Metadata) -> Self {
        let is_dir = metadata.is_dir();
        Self::Proxy {
            is_dir,
            // A directory's `len` tracks its index allocation, not its content,
            // and moves as entries come and go — the same volatility that rules
            // out mtime above. Kind-only identity is all this fallback can
            // offer without refusing legitimate writes.
            len: if is_dir { 0 } else { metadata.len() },
            modified: (!is_dir).then(|| metadata.modified().ok()).flatten(),
        }
    }
}

pub(super) fn absolute_lexical(path: &Path) -> Result<PathBuf, ManagedConfigError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| ManagedConfigError::Read {
                path: path.to_path_buf(),
                source,
            })?
            .join(path)
    };
    Ok(normalize_lexically(&absolute))
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

pub(super) fn resolve_final_symlink(path: &Path) -> Result<PathBuf, ManagedConfigError> {
    let mut current = physicalize_parent(path)?;
    let mut followed = false;
    let mut seen = HashSet::new();
    for _ in 0..MAX_SYMLINKS {
        if !seen.insert(current.clone()) {
            return Err(ManagedConfigError::UnsafePath {
                path: path.to_path_buf(),
                reason: "symlink cycle detected".to_owned(),
            });
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                followed = true;
                let link = fs::read_link(&current).map_err(|source| ManagedConfigError::Read {
                    path: current.clone(),
                    source,
                })?;
                current = if link.is_absolute() {
                    normalize_lexically(&link)
                } else {
                    normalize_lexically(
                        &current
                            .parent()
                            .unwrap_or_else(|| Path::new("/"))
                            .join(link),
                    )
                };
            }
            Ok(_) => return Ok(current),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !followed => {
                return Ok(current);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ManagedConfigError::UnsafePath {
                    path: path.to_path_buf(),
                    reason: "symlink target does not exist".to_owned(),
                });
            }
            Err(source) => {
                return Err(ManagedConfigError::Read {
                    path: current,
                    source,
                });
            }
        }
    }
    Err(ManagedConfigError::UnsafePath {
        path: path.to_path_buf(),
        reason: format!("symlink chain exceeds {MAX_SYMLINKS} links"),
    })
}

fn physicalize_parent(path: &Path) -> Result<PathBuf, ManagedConfigError> {
    let Some(parent) = path.parent() else {
        return Ok(path.to_path_buf());
    };
    let mut probe = parent;
    let mut missing = Vec::new();
    loop {
        match dunce::canonicalize(probe) {
            Ok(canonical) => {
                let mut physical = canonical;
                for component in missing.iter().rev() {
                    physical.push(component);
                }
                if let Some(name) = path.file_name() {
                    physical.push(name);
                }
                return Ok(physical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = probe
                    .file_name()
                    .ok_or_else(|| ManagedConfigError::UnsafePath {
                        path: path.to_path_buf(),
                        reason: "could not resolve config parent".to_owned(),
                    })?;
                missing.push(name.to_os_string());
                probe = probe
                    .parent()
                    .ok_or_else(|| ManagedConfigError::UnsafePath {
                        path: path.to_path_buf(),
                        reason: "could not resolve config parent".to_owned(),
                    })?;
            }
            Err(source) => {
                return Err(ManagedConfigError::Read {
                    path: probe.to_path_buf(),
                    source,
                });
            }
        }
    }
}

pub(super) fn read_source(path: &Path) -> Result<SourceState, ManagedConfigError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SourceState {
                bytes: None,
                hash: blake3::hash(&[]).to_hex().to_string(),
                mode: default_mode(),
                identity: None,
            });
        }
        Err(source) => {
            return Err(ManagedConfigError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if !metadata.file_type().is_file() {
        return Err(ManagedConfigError::UnsafePath {
            path: path.to_path_buf(),
            reason: "target is not a regular file".to_owned(),
        });
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(ManagedConfigError::UnsafePath {
            path: path.to_path_buf(),
            reason: format!("file exceeds {MAX_CONFIG_BYTES} bytes"),
        });
    }
    let bytes = fs::read(path).map_err(|source| ManagedConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    if bytes.contains(&0) {
        return Err(ManagedConfigError::UnsafePath {
            path: path.to_path_buf(),
            reason: "file contains NUL bytes".to_owned(),
        });
    }
    Ok(SourceState {
        hash: blake3::hash(&bytes).to_hex().to_string(),
        bytes: Some(bytes),
        mode: file_mode(&metadata),
        identity: Some(FileIdentity::from_metadata(path, &metadata)),
    })
}

pub(super) fn revalidate(plan: &ManagedConfigPlan) -> Result<(), ManagedConfigError> {
    plan.parent_plan.revalidate_planned()?;
    let target = resolve_final_symlink(&plan.requested_path)?;
    if target != plan.target_path {
        return Err(ManagedConfigError::StalePlan(plan.requested_path.clone()));
    }
    let current = read_source(&target)?;
    if current != plan.original {
        return Err(ManagedConfigError::StalePlan(plan.requested_path.clone()));
    }
    Ok(())
}

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt as _;
    Some(metadata.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn file_mode(_: &fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn default_mode() -> Option<u32> {
    Some(0o644)
}

#[cfg(not(unix))]
fn default_mode() -> Option<u32> {
    None
}
