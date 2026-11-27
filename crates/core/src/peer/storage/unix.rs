use super::*;
use rustix::{
    fs::{AtFlags, Mode, OFlags, mkdirat, openat, renameat, unlinkat},
    io::Errno,
    process::geteuid,
};
use std::{
    fs::{File, TryLockError},
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path},
};
pub type PrivateDir = File;
const PRIVATE_FILE: Mode = Mode::RUSR.union(Mode::WUSR);
const LOCK_FLAGS: OFlags = OFlags::RDWR
    .union(OFlags::CREATE)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);
pub(super) const READ_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

/// Read a bounded JSON file from an already checked private directory.
pub fn read_private_json<T: serde::de::DeserializeOwned>(
    dir: &File,
    name: &str,
    limit: usize,
) -> Result<Option<T>, StorageError> {
    read_private(dir, name, limit)?
        .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| StorageError::Invalid))
        .transpose()
}

/// Atomically replace a bounded JSON file inside an already checked private directory.
pub fn write_private_json(
    dir: &File,
    name: &str,
    value: &impl Serialize,
    limit: usize,
) -> Result<(), StorageError> {
    write_private(
        dir,
        name,
        &serde_json::to_vec(value).map_err(|_| StorageError::Invalid)?,
        limit,
    )
}

fn read_private(dir: &File, name: &str, limit: usize) -> Result<Option<Vec<u8>>, StorageError> {
    let fd = match openat(dir, name, READ_FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let file = File::from(fd);
    check_private_file(&file)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(StorageError::Invalid);
    }
    Ok(Some(bytes))
}

pub(super) fn write_private(
    dir: &File,
    name: &str,
    bytes: &[u8],
    limit: usize,
) -> Result<(), StorageError> {
    if bytes.len() > limit {
        return Err(StorageError::Invalid);
    }
    let mut suffix = [0; 16];
    getrandom::fill(&mut suffix).map_err(|_| CredentialError::EntropyUnavailable)?;
    let temporary = format!(".{:032x}.tmp", u128::from_ne_bytes(suffix));
    let fd = openat(
        dir,
        temporary.as_str(),
        LOCK_FLAGS | OFlags::EXCL,
        PRIVATE_FILE,
    )?;
    let mut file = File::from(fd);
    let result = (|| {
        check_private_file(&file)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        renameat(dir, temporary.as_str(), dir, name)?;
        dir.sync_all().map_err(|_| StorageError::WriteUncertain)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = unlinkat(dir, temporary.as_str(), AtFlags::empty());
    }
    result
}

/// Open or create a private absolute directory without following symlinks.
pub fn open_private_dir(path: &Path) -> Result<File, StorageError> {
    if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
        return Err(StorageError::UnsafePath);
    }
    let uid = geteuid().as_raw();
    let mut dir = File::open("/")?;
    for part in path.components().skip(1) {
        let parent = dir.metadata()?;
        if (parent.uid() != uid && parent.uid() != 0)
            || (parent.mode() & 0o022 != 0 && parent.mode() & 0o1000 == 0)
        {
            return Err(StorageError::UnsafePath);
        }
        let Component::Normal(name) = part else {
            return Err(StorageError::UnsafePath);
        };
        let flags = READ_FLAGS | OFlags::DIRECTORY;
        let next = match openat(&dir, name, flags, Mode::empty()) {
            Err(Errno::NOENT) => {
                match mkdirat(&dir, name, Mode::RWXU) {
                    Ok(()) => dir.sync_all()?,
                    Err(Errno::EXIST) => (),
                    Err(error) => return Err(error.into()),
                }
                openat(&dir, name, flags, Mode::empty())?
            }
            other => other?,
        };
        dir = File::from(next);
    }
    check_private_dir(&dir)?;
    Ok(dir)
}

pub(super) fn check_private_dir(dir: &File) -> Result<(), StorageError> {
    let metadata = dir.metadata()?;
    if !metadata.is_dir()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(StorageError::UnsafePath);
    }
    Ok(())
}

/// Exclusively lock a private file inside a previously checked directory.
pub fn lock_named(dir: &File, name: &str) -> Result<File, StorageError> {
    let lock = File::from(openat(dir, name, LOCK_FLAGS, PRIVATE_FILE)?);
    check_private_file(&lock)?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(TryLockError::WouldBlock) => Err(StorageError::InUse),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Require a private regular file owned by the current effective user.
pub fn check_private_file(file: &File) -> Result<(), StorageError> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(StorageError::UnsafePath);
    }
    Ok(())
}

impl From<Errno> for StorageError {
    fn from(e: Errno) -> Self {
        Self::Io(e.into())
    }
}
