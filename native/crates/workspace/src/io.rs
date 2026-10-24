//! File reading with a per-thread reusable buffer.
//!
//! Reading into a fresh `Vec` for every notebook pays for page faults on the
//! new allocation; reusing a per-thread buffer keeps those pages mapped.
//! Buffers above [`KEEP_LIMIT`] are released after use so a stray huge
//! notebook doesn't pin memory on every worker thread. (Splitting big files
//! into concurrent `pread`s measured at best ~25% faster for 50 MB and worse
//! under load, so reads stay sequential.)
//!
//! We deliberately do not `mmap`: a notebook truncated by another process
//! while mapped raises SIGBUS and would take the whole app down.

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

const KEEP_LIMIT: usize = 16 * 1024 * 1024;

thread_local! {
    static BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Reads `path` fully and calls `f` with the bytes.
pub(crate) fn with_file_bytes<T>(path: &Path, f: impl FnOnce(&[u8]) -> T) -> io::Result<T> {
    BUF.with(|cell| match cell.try_borrow_mut() {
        Ok(mut buf) => {
            buf.clear();
            read_limited(path, &mut buf, None)?;
            let out = f(&buf);
            if buf.capacity() > KEEP_LIMIT {
                *buf = Vec::new();
            }
            Ok(out)
        }
        Err(_) => {
            let mut buf = Vec::new();
            read_limited(path, &mut buf, None)?;
            Ok(f(&buf))
        }
    })
}

/// Replaces `buf` with the contents of `path` unless the file is larger than
/// `max` (then returns `Ok(false)` and leaves `buf` empty).
pub(crate) fn read_limited(path: &Path, buf: &mut Vec<u8>, max: Option<u64>) -> io::Result<bool> {
    buf.clear();
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if max.is_some_and(|m| len > m) {
        return Ok(false);
    }
    buf.reserve(len as usize + 1);
    file.read_to_end(buf)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.bin");
        let data: Vec<u8> = (0..(KEEP_LIMIT + 12345)).map(|i| (i * 31 % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let mut buf = vec![1, 2, 3];
        assert!(read_limited(&p, &mut buf, None).unwrap());
        assert!(buf == data);
        assert!(!read_limited(&p, &mut buf, Some(10)).unwrap());
        assert!(buf.is_empty());
        assert_eq!(with_file_bytes(&p, |b| b.len()).unwrap(), data.len());
    }
}
