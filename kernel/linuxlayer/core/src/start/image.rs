// SPDX-License-Identifier: MPL-2.0

//! The image root's port on the target: `moruna.imagefs` 3.0's client on the task's
//! `filesystem` channel (SDD 27 d.1). The port mirrors the protocol, so this forwards.

use imagefs_protocol::{Bytes, imagefs};
use moruna_svc::CallError;

use crate::{
    fs::imagefs::{ImageDirent, ImageError, ImageResult, ImageSource, ImageStat},
    prelude::*,
};

/// The task's connection to the image filesystem.
pub(super) struct ImageClient {
    client: Mutex<imagefs::Client>,
}

impl ImageClient {
    pub(super) fn new(client: imagefs::Client) -> Self {
        Self {
            client: Mutex::new(client),
        }
    }
}

/// The port's error for a call's: the protocol's number, or a failed channel.
fn refused(error: CallError<imagefs::Error>) -> ImageError {
    use imagefs::Error as E;
    let CallError::Remote(e) = error else {
        return ImageError::TRANSPORT;
    };
    match e {
        E::NotFound => ImageError::NOT_FOUND,
        E::NotDirectory => ImageError::NOT_DIRECTORY,
        E::IsDirectory => ImageError::IS_DIRECTORY,
        E::ReadOnly => ImageError::READ_ONLY,
        E::NoSpace => ImageError::NO_SPACE,
        E::Exists => ImageError::EXISTS,
        E::NotEmpty => ImageError::NOT_EMPTY,
        E::BadPath => ImageError::BAD_PATH,
        E::Loop => ImageError::LOOP,
        E::BadFile => ImageError::BAD_FILE,
        E::BadRange => ImageError::BAD_RANGE,
        E::TooLarge => ImageError::TOO_LARGE,
        E::Corrupt => ImageError::CORRUPT,
        E::Io => ImageError::IO,
        E::TooManyFiles => ImageError::TOO_MANY_FILES,
        E::Unsupported => ImageError::UNSUPPORTED,
    }
}

fn stat(s: imagefs_protocol::Stat) -> ImageStat {
    ImageStat {
        kind: s.kind,
        mode: s.mode,
        size: s.size,
        ino: s.ino,
    }
}

impl ImageSource for ImageClient {
    fn stat(&self, path: &[u8], follow: bool) -> ImageResult<ImageStat> {
        let mut client = self.client.lock();
        client
            .stat(Bytes(path.to_vec()), follow)
            .map(stat)
            .map_err(refused)
    }

    fn open(&self, path: &[u8], flags: u32) -> ImageResult<(u32, ImageStat)> {
        let mut client = self.client.lock();
        client
            .open(Bytes(path.to_vec()), flags, 0)
            .map(|(file, s)| (file, stat(s)))
            .map_err(refused)
    }

    fn readdir(&self, file: u32, cookie: u32) -> ImageResult<(Vec<ImageDirent>, u32)> {
        let mut client = self.client.lock();
        let (entries, next) = client.readdir(file, cookie).map_err(refused)?;
        Ok((
            entries
                .into_iter()
                .map(|(name, kind, ino)| (name.0, kind, ino))
                .collect(),
            next,
        ))
    }

    fn readlink(&self, path: &[u8]) -> ImageResult<Vec<u8>> {
        let mut client = self.client.lock();
        client
            .readlink(Bytes(path.to_vec()))
            .map(|b| b.0)
            .map_err(refused)
    }

    fn read(&self, file: u32, offset: u64, len: u32) -> ImageResult<Vec<u8>> {
        let mut client = self.client.lock();
        client.read(file, offset, len).map(|b| b.0).map_err(refused)
    }

    fn close(&self, file: u32) -> ImageResult<()> {
        let mut client = self.client.lock();
        client.close(file).map_err(refused)
    }
}
