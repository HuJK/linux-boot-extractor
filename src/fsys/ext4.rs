use super::{DirEntry, FileKind, FileSystem};
use crate::blockdev::ReadAt;
use crate::Result;
use ext4_view::{Ext4, Ext4Read};

/// Bridge our `ReadAt` to `ext4_view::Ext4Read`.
struct Reader(Box<dyn ReadAt>);

impl Ext4Read for Reader {
    fn read(
        &mut self,
        start_byte: u64,
        dst: &mut [u8],
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0.read_at(start_byte, dst).map_err(|e| e.to_string().into())
    }
}

pub struct Ext4Fs {
    fs: Ext4,
}

impl Ext4Fs {
    pub fn open(dev: Box<dyn ReadAt>) -> Result<Ext4Fs> {
        let fs = Ext4::load(Box::new(Reader(dev)))?;
        Ok(Ext4Fs { fs })
    }
}

impl FileSystem for Ext4Fs {
    fn fs_type(&self) -> &'static str {
        "ext4"
    }

    fn label(&self) -> Option<String> {
        match self.fs.label().to_str() {
            Ok(s) if !s.is_empty() => Some(s.to_string()),
            _ => None,
        }
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        for entry in self.fs.read_dir(path)? {
            let entry = entry?;
            let name = String::from_utf8_lossy(entry.file_name().as_ref()).into_owned();
            if name == "." || name == ".." {
                continue;
            }
            let kind = match entry.file_type() {
                Ok(t) if t.is_regular_file() => FileKind::File,
                Ok(t) if t.is_dir() => FileKind::Dir,
                Ok(t) if t.is_symlink() => FileKind::Symlink,
                _ => FileKind::Other,
            };
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push(DirEntry { name, kind, size });
        }
        Ok(out)
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        Ok(self.fs.read(path)?)
    }

    fn exists(&self, path: &str) -> bool {
        self.fs.exists(path).unwrap_or(false)
    }

    fn read_link(&self, path: &str) -> Option<String> {
        let target = self.fs.read_link(path).ok()?;
        Some(String::from_utf8_lossy(target.as_ref()).into_owned())
    }

    fn file_size(&self, path: &str) -> Result<u64> {
        Ok(self.fs.metadata(path)?.len())
    }
}
