use super::{DirEntry, FileKind, FileSystem};
use crate::blockdev::{IoAdapter, ReadAt};
use crate::{Error, Result};
use std::io::Read;

type FatFs = fatfs::FileSystem<IoAdapter<Box<dyn ReadAt>>>;

pub struct VfatFs {
    fs: FatFs,
}

impl VfatFs {
    pub fn open(dev: Box<dyn ReadAt>) -> Result<VfatFs> {
        let fs = fatfs::FileSystem::new(IoAdapter::new(dev), fatfs::FsOptions::new())
            .map_err(|e| Error::Vfat(e.to_string()))?;
        Ok(VfatFs { fs })
    }
}

fn normalize(path: &str) -> &str {
    path.trim_start_matches('/')
}

impl FileSystem for VfatFs {
    fn fs_type(&self) -> &'static str {
        "vfat"
    }

    fn label(&self) -> Option<String> {
        let label = self.fs.volume_label();
        let label = label.trim();
        if label.is_empty() || label == "NO NAME" {
            None
        } else {
            Some(label.to_string())
        }
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>> {
        let root = self.fs.root_dir();
        let dir = match normalize(path) {
            "" => root,
            p => root.open_dir(p).map_err(|_| Error::NotFound(path.to_string()))?,
        };
        let mut out = Vec::new();
        for entry in dir.iter() {
            let entry = entry.map_err(|e| Error::Vfat(e.to_string()))?;
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            out.push(DirEntry {
                name,
                kind: if entry.is_dir() { FileKind::Dir } else { FileKind::File },
                size: entry.len(),
            });
        }
        Ok(out)
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let root = self.fs.root_dir();
        let mut file = root
            .open_file(normalize(path))
            .map_err(|_| Error::NotFound(path.to_string()))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).map_err(|e| Error::Vfat(e.to_string()))?;
        Ok(buf)
    }

    fn read_prefix(&self, path: &str, max: usize) -> Result<Vec<u8>> {
        let root = self.fs.root_dir();
        let mut file = root
            .open_file(normalize(path))
            .map_err(|_| Error::NotFound(path.to_string()))?;
        let mut buf = vec![0u8; max];
        let mut filled = 0;
        while filled < buf.len() {
            match file.read(&mut buf[filled..]).map_err(|e| Error::Vfat(e.to_string()))? {
                0 => break,
                n => filled += n,
            }
        }
        buf.truncate(filled);
        Ok(buf)
    }

    fn exists(&self, path: &str) -> bool {
        let root = self.fs.root_dir();
        let p = normalize(path);
        root.open_file(p).is_ok() || root.open_dir(p).is_ok()
    }

    fn file_size(&self, path: &str) -> Result<u64> {
        let root = self.fs.root_dir();
        let mut file = root
            .open_file(normalize(path))
            .map_err(|_| Error::NotFound(path.to_string()))?;
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::End(0)).map_err(|e| Error::Vfat(e.to_string()))
    }
}
