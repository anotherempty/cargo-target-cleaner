use std::fs::{self, File};
use std::path::PathBuf;

use crate::error::{Error, FileOperation};

// Own the open file for the entire cleanup pass. Closing it releases the lock,
// including on errors. Never unlink it: contenders must lock the same file.
pub(super) struct CleanupLock {
    _file: File,
}

impl CleanupLock {
    const FILE_NAME: &str = "cleanup.lock";

    pub(super) fn try_acquire() -> Result<Option<Self>, Error> {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .ok_or(Error::MissingCleanupHome)?;
        let directory = PathBuf::from(home).join(super::STATE_DIRECTORY_NAME);
        fs::create_dir_all(&directory)
            .map_err(|source| Error::file(FileOperation::Create, &directory, source))?;
        let path = directory.join(Self::FILE_NAME);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| Error::file(FileOperation::Create, &path, source))?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(error) if error.kind() == fs2::lock_contended_error().kind() => Ok(None),
            Err(source) => Err(Error::file(FileOperation::Lock, &path, source)),
        }
    }
}
