use infinitree::ChunkPointer;
use std::{
    fs, io,
    os::raw::c_int,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{SystemTimeError, UNIX_EPOCH},
};

#[cfg(target_os = "linux")]
const NO_SYMLINK: c_int = libc::O_PATH | libc::O_NOFOLLOW;

#[cfg(all(not(target_os = "linux"), target_family = "unix"))]
const NO_SYMLINK: c_int = libc::O_SYMLINK;

macro_rules! if_yes {
    ( $flag:expr, $val:expr ) => {
        if $flag {
            Some($val)
        } else {
            None
        }
    };
}

#[derive(thiserror::Error, Debug)]
pub enum EntryError {
    #[error("Path contains `..` or `.` in a non-prefix position")]
    InvalidInputPath,
    #[error("Time error: {source}")]
    Time {
        #[from]
        source: SystemTimeError,
    },
    #[error("IO error: {source}")]
    IO {
        #[from]
        source: io::Error,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum FileType {
    File,
    Directory,
    Symlink(PathBuf),
}

impl Default for FileType {
    fn default() -> Self {
        Self::File
    }
}

impl FileType {
    pub fn is_symlink(&self) -> bool {
        matches!(self, Self::Symlink(_))
    }

    pub fn is_file(&self) -> bool {
        matches!(self, Self::File)
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Directory)
    }
}

#[derive(clap::Args, Clone, Debug, Default)]
pub struct PreserveMetadata {
    /// Preserve permissions.
    #[clap(
        short = 'p',
        long = "preserve-permissions",
        default_value = "true",
        parse(try_from_str)
    )]
    pub permissions: bool,

    /// Preserve owner/gid information. Requires root to restore.
    #[clap(
        short = 'o',
        long = "preserve-ownership",
        default_value = "true",
        parse(try_from_str)
    )]
    pub ownership: bool,

    /// Preserve owner/gid information. Requires root to restore.
    #[clap(
        short = 't',
        long = "preserve-times",
        default_value = "true",
        parse(try_from_str)
    )]
    pub times: bool,
}

pub(crate) fn normalize_filename(path: &impl AsRef<Path>) -> Result<String, EntryError> {
    let path = path.as_ref();

    Ok(path
        .components()
        .map(|c| match c {
            Component::Normal(val) => Ok(val.to_string_lossy()),
            _ => Err(EntryError::InvalidInputPath),
        })
        // skip leading components that are invalid
        .skip_while(Result::is_err)
        .collect::<Result<Vec<_>, _>>()?
        .join("/"))
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct Entry {
    pub unix_secs: u64,
    pub unix_nanos: u32,
    pub unix_perm: Option<u32>,
    pub unix_uid: Option<u32>,
    pub unix_gid: Option<u32>,
    pub readonly: Option<bool>,
    pub file_type: FileType,

    pub size: u64,
    pub name: String,

    pub chunks: Vec<(u64, Arc<ChunkPointer>)>,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        // ignore chunks in comparison, as they may not be available
        self.unix_gid == other.unix_gid
            && self.unix_uid == other.unix_uid
            && self.unix_secs == other.unix_secs
            && self.unix_nanos == other.unix_nanos
            && self.unix_perm == other.unix_perm
            && self.size == other.size
            && self.readonly == other.readonly
            && self.name == other.name
            && self.file_type == other.file_type
    }
}

impl Entry {
    #[cfg(windows)]
    pub fn from_metadata(
        metadata: fs::Metadata,
        path: &impl AsRef<Path>,
        preserve: &PreserveMetadata,
    ) -> Result<Entry, EntryError> {
        let path = path.as_ref();
        let (unix_secs, unix_nanos) = if preserve.times {
            to_unix_mtime(&metadata)?
        } else {
            (0, 0)
        };

        Ok(Entry {
            unix_secs,
            unix_nanos,
            unix_perm: None,
            unix_uid: None,
            unix_gid: None,
            file_type: if metadata.is_symlink() {
                FileType::Symlink(fs::read_link(path)?)
            } else if metadata.is_dir() {
                FileType::Directory
            } else {
                FileType::File
            },

            readonly: if_yes!(preserve.permissions, metadata.permissions().readonly()),

            size: metadata.len(),
            name: normalize_filename(path)?,

            chunks: Vec::new(),
        })
    }

    #[cfg(unix)]
    pub fn from_metadata(
        metadata: fs::Metadata,
        path: &impl AsRef<Path>,
        preserve: &PreserveMetadata,
    ) -> Result<Entry, EntryError> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let perms = metadata.permissions();
        let (unix_secs, unix_nanos) = if preserve.times {
            to_unix_mtime(&metadata)?
        } else {
            (0, 0)
        };

        Ok(Entry {
            unix_secs,
            unix_nanos,

            unix_perm: if_yes!(preserve.permissions, perms.mode()),
            unix_uid: if_yes!(preserve.ownership, metadata.uid()),
            unix_gid: if_yes!(preserve.ownership, metadata.gid()),
            readonly: if_yes!(preserve.permissions, metadata.permissions().readonly()),
            file_type: if metadata.is_symlink() {
                FileType::Symlink(fs::read_link(path)?)
            } else if metadata.is_dir() {
                FileType::Directory
            } else {
                FileType::File
            },

            size: metadata.len(),
            name: normalize_filename(&path)?,

            chunks: Vec::new(),
        })
    }

    #[cfg(windows)]
    pub fn restore_to(
        &self,
        file: &fs::File,
        preserve: &PreserveMetadata,
    ) -> Result<(), EntryError> {
        file.set_len(self.size)?;

        if let Some(readonly) = self.readonly {
            if preserve.permissions {
                let metadata = file.metadata()?;
                let mut permissions = metadata.permissions();
                permissions.set_readonly(readonly);
                file.set_permissions(permissions);
            }
        }

        Ok(())
    }

    #[cfg(unix)]
    pub fn restore_to(
        &self,
        path: &impl AsRef<Path>,
        preserve: &PreserveMetadata,
    ) -> Result<Option<fs::File>, EntryError> {
        use std::{
            os::unix::{fs::PermissionsExt, prelude::AsRawFd},
            time::{Duration, SystemTime},
        };
        use FileType::*;

        let file = match self.file_type {
            Directory => {
                fs::create_dir(path)?;
                fs::File::open(path)?
            }
            File => {
                let file = open_file(path)?;
                file.set_len(self.size)?;
                file
            }
            Symlink(ref pointed_to) => {
                use std::os::unix::fs::OpenOptionsExt;
                std::os::unix::fs::symlink(pointed_to, path)?;
                fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(NO_SYMLINK)
                    .open(path)?
            }
        };

        if preserve.permissions {
            if let Some(perm) = self.unix_perm {
                file.set_permissions(fs::Permissions::from_mode(perm))?;
            }
        }

        if preserve.times {
            let atime = SystemTime::now().duration_since(UNIX_EPOCH)?.into();
            let mtime = Duration::new(self.unix_secs, self.unix_nanos).into();
            nix::sys::stat::futimens(file.as_raw_fd(), &atime, &mtime).unwrap();
        }

        Ok(if self.file_type.is_file() {
            Some(file)
        } else {
            None
        })
    }
}

fn open_file(path: impl AsRef<Path> + Copy) -> Result<fs::File, io::Error> {
    match fs::OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .open(path)
    {
        Ok(file) => Ok(file),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.as_ref().parent() {
                fs::create_dir_all(parent)?;
                open_file(path)
            } else {
                Err(err)
            }
        }
        e @ Err(_) => e,
    }
}

#[inline(always)]
fn to_unix_mtime(m: &fs::Metadata) -> Result<(u64, u32), EntryError> {
    use std::os::unix::fs::MetadataExt;
    Ok((m.mtime() as u64, m.mtime_nsec() as u32))
}
