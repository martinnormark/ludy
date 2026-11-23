//! What a drive looks like as a filesystem, to whatever shows it: the NFS
//! adapter, the app's own browser, the command line.

pub const ROOT_ID: u64 = 1;

#[derive(Clone, Debug, PartialEq)]
pub enum FsError {
    NotFound,
    NotDir,
    IsDir,
    Exists,
    NotEmpty,
    Invalid,
    /// The name is longer than a sealed name can hold.
    NameTooLong,
    Io(String),
}

pub type FsResult<T> = Result<T, FsError>;

#[derive(Clone, Debug, PartialEq)]
pub struct Attr {
    pub id: u64,
    pub is_dir: bool,
    pub size: u64,
    pub mtime_ms: u64,
    /// Bytes of this file held on this machine (cache or journal).
    pub local_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pending {
    /// Files written locally and not yet uploaded.
    pub files: usize,
    pub bytes: u64,
    /// Renames, deletes and new folders not yet sent.
    pub ops: usize,
    pub uploading: Option<String>,
    pub last_error: Option<String>,
}

impl Pending {
    pub fn is_idle(&self) -> bool {
        self.files == 0 && self.ops == 0 && self.uploading.is_none()
    }
}

/// Names Finder and Spotlight create on any volume. They never leave this Mac.
pub fn is_local_only(name: &str) -> bool {
    name.starts_with("._")
        || matches!(
            name,
            ".DS_Store" | ".Spotlight-V100" | ".Trashes" | ".fseventsd" | ".TemporaryItems" | ".apdisk" | ".VolumeIcon.icns"
        )
        || name.starts_with(".metadata_never_index")
}
