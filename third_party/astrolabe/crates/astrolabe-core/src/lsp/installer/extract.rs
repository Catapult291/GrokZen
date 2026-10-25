//! Archive extraction for installed language-server packages.

use std::fs::{self, File};
use std::io::{self, Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;

/// Supported download payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// `.tar.gz` / `.tgz`
    TarGz,
    /// `.zip` (also VSIX, which is zip)
    Zip,
    /// Raw single-file binary (no archive wrapper).
    Raw,
}

/// Extract `bytes` into `dest_dir`. Ensures `binary_relative` exists afterwards
/// (creating parent dirs as needed). Sets the Unix executable bit on the binary.
pub fn extract(
    format: ArchiveFormat,
    bytes: &[u8],
    dest_dir: &Path,
    binary_relative: &Path,
) -> Result<(), String> {
    match format {
        ArchiveFormat::TarGz => extract_tar_gz(bytes, dest_dir)?,
        ArchiveFormat::Zip => extract_zip(bytes, dest_dir)?,
        ArchiveFormat::Raw => {
            let target = dest_dir.join(binary_relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut f = File::create(&target).map_err(|e| e.to_string())?;
            f.write_all(bytes).map_err(|e| e.to_string())?;
        }
    }
    let bin = dest_dir.join(binary_relative);
    if !bin.is_file() {
        return Err(format!(
            "archive did not contain expected binary {}",
            binary_relative.display()
        ));
    }
    set_executable(&bin).map_err(|e| e.to_string())?;
    Ok(())
}

fn extract_tar_gz(bytes: &[u8], dest_dir: &Path) -> Result<(), String> {
    let dec = GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(dec);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        let safe = safe_join(dest_dir, &path)?;
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&safe).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = safe.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        entry.unpack(&safe).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn extract_zip(bytes: &[u8], dest_dir: &Path) -> Result<(), String> {
    let cursor = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).map_err(|e| e.to_string())?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| e.to_string())?;
        let Some(enclosed) = file.enclosed_name() else {
            continue;
        };
        let enclosed = enclosed.to_path_buf();
        let out = safe_join(dest_dir, &enclosed)?;
        if file.is_dir() {
            fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut outfile = File::create(&out).map_err(|e| e.to_string())?;
        io::copy(&mut file, &mut outfile).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = file.unix_mode() {
                fs::set_permissions(&out, fs::Permissions::from_mode(mode))
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

fn safe_join(base: &Path, rel: &Path) -> Result<PathBuf, String> {
    let mut out = base.to_path_buf();
    for c in rel.components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "archive path escapes destination: {}",
                    rel.display()
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("archive path is absolute: {}", rel.display()));
            }
        }
    }
    Ok(out)
}

fn set_executable(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        let mode = perms.mode();
        if mode & 0o111 == 0 {
            perms.set_mode(mode | 0o755);
            fs::set_permissions(path, perms)?;
        }
    }
    let _ = path;
    Ok(())
}

// Silence unused import on non-unix when Read is only used via io::copy path.
#[allow(dead_code)]
fn _read_trait_used<R: Read>(_: &mut R) {}
