//! Bounded reads of local kit metadata, never pipes/devices or arbitrary streams.
use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    path::Path,
};

pub(super) fn read(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err(io::Error::other(
            "Kit file must be a regular file within the size limit",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file: File = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("Kit file is not regular"));
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::other("Kit file exceeds size limit"));
    }
    Ok(bytes)
}

pub(super) fn text(path: &Path, limit: usize) -> io::Result<String> {
    String::from_utf8(read(path, limit)?).map_err(|_| io::Error::other("Kit file must be UTF-8"))
}
