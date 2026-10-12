//! A preflighted GGUF, pinned by its open descriptor: loads read these bytes, never the path again.
use crate::gguf::GgufModel;
use same_file::Handle;
use std::fs::{File, Metadata};
use std::io::{self, Seek};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::SystemTime;

#[derive(Clone, PartialEq, Eq)]
struct Snapshot {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl Snapshot {
    fn read(metadata: &Metadata) -> io::Result<Self> {
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "GGUF source must be a regular file",
            ));
        }
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

fn changed() -> io::Error {
    io::Error::other(
        "GGUF source changed since preflight; keep model files immutable while serving",
    )
}

pub(super) struct Pinned {
    file: File,
    snapshot: Snapshot,
}

impl Pinned {
    /// The file and its identity; symlinks and hardlinks to one file share an identity.
    pub(super) fn open(path: &Path) -> io::Result<(Self, Handle)> {
        let file = File::open(path)?;
        let snapshot = Snapshot::read(&file.metadata()?)?;
        let handle = Handle::from_file(file.try_clone()?)?;
        Ok((Self { file, snapshot }, handle))
    }

    pub(super) const fn len(&self) -> u64 {
        self.snapshot.len
    }

    /// Another opening of the same file must see the same bytes.
    pub(super) fn same_as(&self, other: &Self) -> io::Result<()> {
        if self.snapshot != other.snapshot {
            return Err(changed());
        }
        self.check_unchanged()
    }

    pub(super) fn check_unchanged(&self) -> io::Result<()> {
        if Snapshot::read(&self.file.metadata()?)? != self.snapshot {
            return Err(changed());
        }
        Ok(())
    }

    /// The pinned bytes from the start. Shares the descriptor's offset, so one reader at a time.
    pub(super) fn reader(&self) -> io::Result<File> {
        let mut file = self.file.try_clone()?;
        file.rewind()?;
        Ok(file)
    }

    /// The pinned bytes' artifact digest.
    pub(super) fn hash(&self) -> io::Result<crate::artifact::ArtifactId> {
        self.check_unchanged()?;
        crate::artifact::hash_at(&self.file)
    }

    pub(super) fn gguf(&self) -> crate::gguf::Result<GgufModel> {
        let model = GgufModel::from_file(self.reader()?)?;
        self.check_unchanged()?;
        Ok(model)
    }
}
