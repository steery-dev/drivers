//! A file its harness only appends to, read on from where the last read left it.

use std::fs::File;
use std::io::{self, BufRead as _, BufReader, Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::MetadataExt as _;

use crate::lines::bytes;

/// How much of what was read last is read again to know the file was only appended to.
const SEAL: u64 = 64;
/// How much is read at once: a file read from its start may be hundreds of megabytes, and
/// small reads made each one a system call per 8 KiB.
const CHUNK: usize = 1 << 20;

/// How far one file has been read: which file it was, how long, and the last bytes read.
#[derive(Default)]
pub struct Followed {
    /// Its device and inode: a file replaced under the same name is another file.
    file: Option<(u64, u64)>,
    /// How long the file was when it was last read.
    len: u64,
    /// Just past the newline of the last whole line read.
    through: u64,
    /// The last bytes read, up to [`SEAL`] of them: bytes there that changed are a file
    /// written whole again.
    seal: Vec<u8>,
}

impl Followed {
    /// The file's device and inode, once it is read.
    pub fn file(&self) -> (u64, u64) {
        self.file.unwrap_or_default()
    }

    /// How long the file was when it was last read.
    pub fn length(&self) -> u64 {
        self.len
    }

    /// Whether `open` is not the file as it was read, so that what was kept of it is to be
    /// forgotten: never read, another file under the name, shorter than it was, or written
    /// whole again. It is then read again from its start.
    pub fn restart(&mut self, open: &File) -> io::Result<bool> {
        let meta = open.metadata()?;
        let file = (meta.dev(), meta.ino());
        let rewritten = self.file != Some(file)
            || meta.len() < self.len
            || bytes(open, self.through - self.seal.len() as u64, self.through)? != self.seal;
        if rewritten {
            *self = Self {
                file: Some(file),
                ..Self::default()
            };
        }
        Ok(rewritten)
    }

    /// Each whole line appended since the last read, handed to `line` without its newline
    /// with where it starts and ends; a half-written last line waits for the next read.
    /// `line` answers whether to go on: the line it stops at is read, and so is the file
    /// as far as it.
    pub fn read_on(
        &mut self,
        mut open: &File,
        mut line: impl FnMut(&[u8], u64, u64) -> bool,
    ) -> io::Result<()> {
        let len = open.metadata()?.len();
        if len == self.len {
            return Ok(());
        }
        self.len = len;
        open.seek(SeekFrom::Start(self.through))?;
        let mut lines = BufReader::with_capacity(CHUNK, open.take(len - self.through));
        let mut read = Vec::new();
        loop {
            read.clear();
            if lines.read_until(b'\n', &mut read)? == 0 || read.last() != Some(&b'\n') {
                break;
            }
            let start = self.through;
            self.through += read.len() as u64;
            if !line(&read[..read.len() - 1], start, self.through - 1) {
                break;
            }
        }
        self.seal = bytes(open, self.through.saturating_sub(SEAL), self.through)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(followed: &mut Followed, open: &File) -> Vec<String> {
        let mut out = Vec::new();
        followed.restart(open).unwrap();
        followed
            .read_on(open, |line, _, _| {
                out.push(String::from_utf8_lossy(line).into_owned());
                true
            })
            .unwrap();
        out
    }

    #[test]
    fn a_file_is_read_on_and_again_where_it_was_written_whole() {
        let path =
            std::env::temp_dir().join(format!("steery-common-follow-{}", std::process::id()));
        std::fs::write(&path, "a\nb\nha").unwrap();
        let mut followed = Followed::default();
        assert_eq!(
            lines(&mut followed, &File::open(&path).unwrap()),
            ["a", "b"]
        );
        std::fs::write(&path, "a\nb\nhalf\n").unwrap();
        assert_eq!(lines(&mut followed, &File::open(&path).unwrap()), ["half"]);
        std::fs::write(&path, "x\ny\nhalf\nmore\n").unwrap();
        let open = File::open(&path).unwrap();
        assert!(followed.restart(&open).unwrap(), "written whole again");
        assert_eq!(lines(&mut followed, &open), ["x", "y", "half", "more"]);
        let _ = std::fs::remove_file(&path);
    }
}
