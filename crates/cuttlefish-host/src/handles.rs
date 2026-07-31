//! Files the host holds open on a guest's behalf.
//!
//! This is the host side of the rule that bulk data never enters guest memory. A
//! block receives a handle and a length, then pulls bounded windows; the host
//! seeks and reads each window straight off disk. Neither side ever holds the
//! whole file, so guest memory tracks the window size a block chose rather than
//! the size of its input.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// One file held open for a job.
struct OpenFile {
    file: File,
    len: u64,
}

/// A job's open files.
///
/// Scoping this to a single job is a security property rather than tidiness.
/// Because the table lives and dies with one job, a handle from another job
/// names nothing here — which is what lets [`Handles::slice`] skip a capability
/// check entirely. The check happened once, at [`Handles::open`], and a handle
/// cannot be forged into a reference to someone else's data.
#[derive(Default)]
pub struct Handles {
    next: u32,
    open: HashMap<u32, OpenFile>,
}

/// Why a handle operation failed.
#[derive(Debug, thiserror::Error)]
pub enum HandleError {
    /// The handle does not belong to this job, or never existed.
    #[error("no such handle: {0}")]
    BadHandle(u32),
    /// The requested offset is beyond the end of the file.
    #[error("offset {offset} is past end of file ({len} bytes)")]
    OffsetPastEnd {
        /// The offset that was asked for.
        offset: u64,
        /// The file's actual length.
        len: u64,
    },
    /// The window was too small to contain even one whole character.
    #[error("window of {0} bytes is too small to hold one character")]
    WindowTooSmall(u64),
    /// Underlying I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// One window of a file.
pub struct Window {
    /// The window's contents.
    pub text: String,
    /// Where the returned text actually ended; see [`Handles::slice`].
    pub next_offset: u64,
}

impl Handles {
    /// Open a file, returning its handle and length.
    ///
    /// The caller is responsible for having capability-checked `path` first —
    /// this type deliberately knows nothing about capabilities, so that the
    /// check lives in exactly one place rather than being half-enforced here.
    pub fn open(&mut self, path: &Path) -> Result<(u32, u64), HandleError> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();

        let handle = self.next;
        self.next += 1;
        self.open.insert(handle, OpenFile { file, len });
        Ok((handle, len))
    }

    /// Read one window, truncated to a UTF-8 character boundary.
    ///
    /// The truncation is the subtle part, and the reason [`Window::next_offset`]
    /// exists at all. A caller walking a file picks window sizes with no idea
    /// where characters begin, so a naive read splits a multi-byte character at
    /// nearly every seam and yields mojibake. Instead the window is cut back to
    /// the last complete character and `next_offset` reports where that landed —
    /// so a caller resuming from `next_offset`, rather than advancing by the
    /// length it requested, never observes a split.
    ///
    /// Reading past the end is not an error: the window is clamped, because a
    /// block asking for a full window at the tail of a file is behaving
    /// correctly. Starting past the end *is* an error, since that indicates the
    /// caller has lost track of where it is.
    pub fn slice(&mut self, handle: u32, offset: u64, len: u64) -> Result<Window, HandleError> {
        let f = self
            .open
            .get_mut(&handle)
            .ok_or(HandleError::BadHandle(handle))?;

        if offset > f.len {
            return Err(HandleError::OffsetPastEnd { offset, len: f.len });
        }

        let want = len.min(f.len - offset) as usize;
        let mut buf = vec![0u8; want];
        f.file.seek(SeekFrom::Start(offset))?;
        f.file.read_exact(&mut buf)?;

        let valid = match std::str::from_utf8(&buf) {
            Ok(_) => buf.len(),
            Err(e) => e.valid_up_to(),
        };

        // A window landing entirely inside one character would otherwise return
        // empty forever, and a caller looping until it reaches the end would
        // spin making no progress and reporting no problem. Failing is strictly
        // better than that silence.
        if valid == 0 && !buf.is_empty() {
            return Err(HandleError::WindowTooSmall(len));
        }
        buf.truncate(valid);

        Ok(Window {
            text: String::from_utf8(buf).expect("truncated at a validated boundary"),
            next_offset: offset + valid as u64,
        })
    }
}
