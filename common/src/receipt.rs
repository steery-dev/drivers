//! A receipt read from a transcript the harness only appends to: where the transcript ends
//! before words go ([`end`]), and whether exactly one whole record it gained since is a
//! person's input carrying the words' delivery marker whole ([`found`], [`marked`]). What
//! a person's input is, each driver says of its own harness's records.

use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use serde_json::Value;

/// The most a receipt reads past its mark.
const RECEIPT_MOST: u64 = 64 * 1024 * 1024;

/// The most of a transcript's end read back for its last whole record.
const BOUNDARY_MOST: u64 = 1024 * 1024;

/// Steery's marker for the reference it puts in the words it delivers.
pub const DELIVERED: &str = "steery-delivery:";

/// Where a transcript ends: the file's identity and the end of its last whole record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct End {
    pub dev: u64,
    pub ino: u64,
    pub bytes: u64,
}

/// Where the transcript at `path` ends now.
pub fn end(path: &Path) -> io::Result<End> {
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    Ok(End {
        dev: meta.dev(),
        ino: meta.ino(),
        bytes: boundary(&mut file, meta.len())?,
    })
}

/// The end of the last whole record of the first `len` bytes of `file`.
fn boundary(file: &mut File, len: u64) -> io::Result<u64> {
    let mut end = len;
    while end > 0 {
        if len - end >= BOUNDARY_MOST {
            return Err(io::Error::other(
                "the transcript ends in an oversized unfinished record",
            ));
        }
        let take = end.min(8192);
        let start = end - take;
        file.seek(SeekFrom::Start(start))?;
        let mut block = vec![0u8; usize::try_from(take).map_err(io::Error::other)?];
        file.read_exact(&mut block)?;
        if let Some(last) = block.iter().rposition(|byte| *byte == b'\n') {
            return Ok(start + last as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

/// Whether exactly one whole record the transcript at `path` gained past `end` is a
/// person's input carrying `steery-delivery:<delivery>`, as `input` reads a record and the
/// marker. The line says why there is no answer: the file replaced or shrunk since, grown
/// past what a receipt reads, unreadable, a malformed record, or two inputs carrying it.
pub fn found(
    path: &Path,
    end: &End,
    delivery: &str,
    input: impl Fn(&Value, &str) -> bool,
) -> Result<bool, String> {
    let unread = |e: io::Error| format!("{}: {e}", path.display());
    let mut file = File::open(path).map_err(unread)?;
    let meta = file.metadata().map_err(unread)?;
    if meta.dev() != end.dev || meta.ino() != end.ino || meta.len() < end.bytes {
        return Err("the transcript was replaced since the mark".into());
    }
    let gained = meta.len() - end.bytes;
    if gained > RECEIPT_MOST {
        return Err("the transcript grew past what a receipt reads".into());
    }
    let mut delta = Vec::new();
    file.seek(SeekFrom::Start(end.bytes))
        .and_then(|_| file.take(gained).read_to_end(&mut delta))
        .map_err(unread)?;
    carried(&delta, delivery, input)
}

/// Whether exactly one whole record of `delta` is a person's input carrying the marker
/// `steery-delivery:<delivery>`, as `input` reads one; two are an error. A record not yet
/// ended is left for the next look.
pub fn carried(
    delta: &[u8],
    delivery: &str,
    input: impl Fn(&Value, &str) -> bool,
) -> Result<bool, String> {
    let whole = delta
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(&[][..], |last| &delta[..=last]);
    let marker = format!("{DELIVERED}{delivery}");
    let mut matches = 0;
    for line in whole
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: Value = serde_json::from_slice(line)
            .map_err(|_| "the transcript has a malformed record".to_owned())?;
        if input(&record, &marker) {
            matches += 1;
        }
    }
    match matches {
        0 | 1 => Ok(matches == 1),
        _ => Err("two inputs carry the delivery marker".into()),
    }
}

/// Whether `text` holds the delivery marker whole: the character after it is no letter,
/// digit, `.`, `-` or `_` (PROTOCOL.md §3.3), so `aim.<throw>.1` is not `aim.<throw>.10`.
pub fn marked(text: &str, marker: &str) -> bool {
    text.match_indices(marker).any(|(at, _)| {
        text[at + marker.len()..]
            .chars()
            .next()
            .is_none_or(|next| !(next.is_alphanumeric() || matches!(next, '.' | '-' | '_')))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delivery_marker_matches_only_whole() {
        assert!(marked("words steery-delivery:abc", "steery-delivery:abc"));
        assert!(marked("steery-delivery:abc) more", "steery-delivery:abc"));
        assert!(!marked("steery-delivery:abcd", "steery-delivery:abc"));
        assert!(!marked("steery-delivery:abc.1", "steery-delivery:abc"));
        assert!(marked(
            "steery-delivery:abc-x steery-delivery:abc",
            "steery-delivery:abc"
        ));
    }

    #[test]
    fn a_mark_ends_at_the_last_whole_record() {
        let path =
            std::env::temp_dir().join(format!("steery-common-receipt-{}", std::process::id()));
        std::fs::write(&path, "{\"a\":1}\n{\"b\":2}\n{\"c\":").unwrap();
        let mut open = File::open(&path).unwrap();
        let len = open.metadata().unwrap().len();
        assert_eq!(boundary(&mut open, len).unwrap(), 16);
        assert_eq!(boundary(&mut open, 0).unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }
}
