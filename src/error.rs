use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("read out of bounds: offset {offset} + len {len} > size {size}")]
    OutOfBounds { offset: u64, len: usize, size: u64 },

    #[error("not a supported disk image: {0}")]
    UnknownImageFormat(String),

    #[error("qcow2: {0}")]
    Qcow2(String),

    #[error("qcow2 feature not supported yet: {0}")]
    Qcow2Unsupported(String),

    #[error("gpt: {0}")]
    Gpt(String),

    #[error("no partition table found and whole disk is not a filesystem")]
    NoPartitions,

    #[error("partition {0} does not exist")]
    NoSuchPartition(usize),

    #[error("unrecognized filesystem{}", match .0 { Some(name) => format!(" (detected {name}, not supported yet)"), None => String::new() })]
    UnknownFilesystem(Option<&'static str>),

    #[error("ext4: {0}")]
    Ext4(#[from] ext4_view::Ext4Error),

    #[error("vfat: {0}")]
    Vfat(String),

    #[error("file not found: {0}")]
    NotFound(String),

    #[error("not a directory: {0}")]
    NotADirectory(String),

    #[error("no boot artifacts (kernel/initramfs) found in any partition")]
    NoBootArtifacts,
}
