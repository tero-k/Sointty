use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use sointty_core::{PlayerError, Source};

mod read_ahead;

pub use read_ahead::{ReadAheadSource, StallState};

pub struct FileSource {
    file: File,
    path: PathBuf,
    size: Option<u64>,
}

impl FileSource {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PlayerError> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path).map_err(map_io)?;
        let size = file.metadata().ok().map(|metadata| metadata.len());
        Ok(Self { file, path, size })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Read for FileSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Seek for FileSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

impl Source for FileSource {
    fn size_hint(&self) -> Option<u64> {
        self.size
    }
}

pub fn map_io(error: io::Error) -> PlayerError {
    PlayerError::Io(error.kind())
}

pub fn map_decode(_context: &'static str) -> PlayerError {
    PlayerError::Decode
}
