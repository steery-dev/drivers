//! A transcript's lines, read within bounds: one record is never held past [`RECORD`]
//! bytes, and one read of `sessions/read` says at most about [`BATCH`] bytes of records,
//! so it answers inside its bound (PROTOCOL.md §9) however large the transcript.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read as _, Seek as _, SeekFrom, Take};

/// The longest record read: one longer is passed over, said as a record that does not read.
pub const RECORD: usize = 8 << 20;
/// About how many bytes of records one read says; past it the read says `more`.
pub const BATCH: u64 = 4 << 20;

/// How a line read.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    /// Ended by its newline, and kept.
    Whole,
    /// Ended by its newline, and longer than kept.
    Long,
    /// No newline before the end: nothing whole.
    Partial,
}

/// The next line into `line`, without its newline, kept only up to `most` bytes.
pub fn capped<R: BufRead>(reader: &mut R, line: &mut Vec<u8>, most: usize) -> io::Result<Line> {
    line.clear();
    let mut long = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(Line::Partial);
        }
        let (taken, ended) = match buffer.iter().position(|&byte| byte == b'\n') {
            Some(at) => (at, true),
            None => (buffer.len(), false),
        };
        if !long && line.len() + taken <= most {
            line.extend_from_slice(&buffer[..taken]);
        } else {
            long = true;
            line.clear();
        }
        reader.consume(taken + usize::from(ended));
        if ended {
            return Ok(if long { Line::Long } else { Line::Whole });
        }
    }
}

/// How many bytes a reader over `total` bytes has consumed.
pub fn consumed<R: io::Read>(reader: &BufReader<Take<R>>, total: u64) -> u64 {
    total - reader.get_ref().limit() - reader.buffer().len() as u64
}

/// The offset of the first whole record at or after `start` in a file `length` bytes
/// long: `start` itself where a record begins there, else just past the next newline,
/// else `length`. Where a tail of the file begins.
pub fn first_whole(mut open: &File, start: u64, length: u64) -> io::Result<u64> {
    if start == 0 {
        return Ok(0);
    }
    open.seek(SeekFrom::Start(start - 1))?;
    let total = length - (start - 1);
    let mut reader = BufReader::new(open.take(total));
    let mut skipped = Vec::new();
    Ok(match capped(&mut reader, &mut skipped, 0)? {
        Line::Partial => length,
        Line::Whole | Line::Long => start - 1 + consumed(&reader, total),
    })
}

/// The bytes of the file from `start` to `end`.
pub fn bytes(mut open: &File, start: u64, end: u64) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; usize::try_from(end - start).map_err(io::Error::other)?];
    open.seek(SeekFrom::Start(start))?;
    open.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_kept_whole_passed_over_long_or_partial() {
        let mut reader = io::Cursor::new(b"short\nmuch longer\nhalf".to_vec());
        let mut line = Vec::new();
        assert_eq!(capped(&mut reader, &mut line, 6).unwrap(), Line::Whole);
        assert_eq!(line, b"short");
        assert_eq!(capped(&mut reader, &mut line, 6).unwrap(), Line::Long);
        assert!(line.is_empty());
        assert_eq!(capped(&mut reader, &mut line, 6).unwrap(), Line::Partial);
    }

    #[test]
    fn a_tail_begins_at_a_whole_record() {
        let path = std::env::temp_dir().join(format!("steery-common-lines-{}", std::process::id()));
        std::fs::write(&path, b"one\ntwo\nthree").unwrap();
        let open = File::open(&path).unwrap();
        assert_eq!(first_whole(&open, 0, 13).unwrap(), 0);
        assert_eq!(
            first_whole(&open, 4, 13).unwrap(),
            4,
            "a record begins there"
        );
        assert_eq!(first_whole(&open, 5, 13).unwrap(), 8);
        assert_eq!(first_whole(&open, 9, 13).unwrap(), 13, "no newline after");
        assert_eq!(bytes(&open, 4, 7).unwrap(), b"two");
        let _ = std::fs::remove_file(&path);
    }
}
