//! Files the host holds open on a guest's behalf.
//!
//! This is the host side of the rule that bulk data never enters guest memory. A
//! block receives a handle and a length, then pulls bounded windows; the host
//! seeks and reads each window straight off disk. Neither side ever holds the
//! whole file, so guest memory tracks the window size a block chose rather than
//! the size of its input.

use cuttlefish_abi::MediaKind;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// What a handle refers to.
///
/// Rendered pages have no file behind them, so a handle is either something on
/// disk or bytes the host produced. Both answer the same commands, which is what
/// lets a rendered page be used anywhere a file-backed image can.
enum Source {
    /// Read from disk on demand, so a large file is never resident.
    File(File),
    /// Held in memory — a rasterized page, which exists nowhere else.
    Memory(Vec<u8>),
}

/// One thing held open for a job.
struct OpenFile {
    source: Source,
    len: u64,
    kind: MediaKind,
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
    /// Open a file, returning its handle, length, and what the host made of it.
    ///
    /// The caller is responsible for having capability-checked `path` first —
    /// this type deliberately knows nothing about capabilities, so that the
    /// check lives in exactly one place rather than being half-enforced here.
    pub fn open(&mut self, path: &Path) -> Result<(u32, u64, MediaKind), HandleError> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();

        // Sniff content rather than trusting the extension: a `.txt` holding a
        // PNG is a file a block should be told about, and an extension is
        // whatever the last program to touch the file decided.
        let mut head = vec![0u8; 4096.min(len as usize)];
        file.read_exact(&mut head)?;
        file.seek(SeekFrom::Start(0))?;
        let kind = classify(&head, len);

        let handle = self.next;
        self.next += 1;
        self.open.insert(
            handle,
            OpenFile {
                source: Source::File(file),
                len,
                kind: kind.clone(),
            },
        );
        Ok((handle, len, kind))
    }

    /// Register bytes the host produced — a rendered page — as a new handle.
    pub fn insert_bytes(&mut self, bytes: Vec<u8>, kind: MediaKind) -> (u32, u64) {
        let len = bytes.len() as u64;
        let handle = self.next;
        self.next += 1;
        self.open.insert(
            handle,
            OpenFile {
                source: Source::Memory(bytes),
                len,
                kind,
            },
        );
        (handle, len)
    }

    /// What a handle refers to.
    pub fn kind(&self, handle: u32) -> Result<MediaKind, HandleError> {
        self.open
            .get(&handle)
            .map(|f| f.kind.clone())
            .ok_or(HandleError::BadHandle(handle))
    }

    /// Every byte behind a handle.
    ///
    /// Used for images headed to a vision model, where the whole thing has to be
    /// sent. Deliberately host-side only — this is the one place a whole file is
    /// materialized, and it never crosses into guest memory.
    pub fn read_all(&mut self, handle: u32) -> Result<Vec<u8>, HandleError> {
        let f = self
            .open
            .get_mut(&handle)
            .ok_or(HandleError::BadHandle(handle))?;
        match &mut f.source {
            Source::Memory(bytes) => Ok(bytes.clone()),
            Source::File(file) => {
                let mut buf = Vec::with_capacity(f.len as usize);
                file.seek(SeekFrom::Start(0))?;
                file.read_to_end(&mut buf)?;
                Ok(buf)
            }
        }
    }

    /// Read one window as raw bytes, with no character-boundary handling.
    pub fn slice_bytes(
        &mut self,
        handle: u32,
        offset: u64,
        len: u64,
    ) -> Result<(Vec<u8>, u64), HandleError> {
        let f = self
            .open
            .get_mut(&handle)
            .ok_or(HandleError::BadHandle(handle))?;
        if offset > f.len {
            return Err(HandleError::OffsetPastEnd { offset, len: f.len });
        }

        let want = len.min(f.len - offset) as usize;
        let mut buf = vec![0u8; want];
        match &mut f.source {
            Source::Memory(bytes) => {
                buf.copy_from_slice(&bytes[offset as usize..offset as usize + want]);
            }
            Source::File(file) => {
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(&mut buf)?;
            }
        }
        Ok((buf, offset + want as u64))
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
        match &mut f.source {
            Source::Memory(bytes) => {
                buf.copy_from_slice(&bytes[offset as usize..offset as usize + want]);
            }
            Source::File(file) => {
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(&mut buf)?;
            }
        }

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

/// Work out what a file holds from its leading bytes.
///
/// Magic numbers, not extensions. An extension records whatever the last program
/// to touch the file believed; the bytes record what it is.
fn classify(head: &[u8], len: u64) -> MediaKind {
    if head.starts_with(b"%PDF-") {
        // Page count and text-layer detection need the whole file, so they are
        // filled in by the document layer; this is the fallback when it cannot.
        return MediaKind::Document {
            pages: 0,
            has_text_layer: false,
        };
    }
    if head.starts_with(&[0x89, b'P', b'N', b'G']) {
        return MediaKind::Image {
            format: "png".into(),
        };
    }
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return MediaKind::Image {
            format: "jpeg".into(),
        };
    }
    if head.starts_with(b"GIF8") {
        return MediaKind::Image {
            format: "gif".into(),
        };
    }
    if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        return MediaKind::Image {
            format: "webp".into(),
        };
    }

    // An empty file is text: there is nothing in it to be anything else, and
    // calling it binary would make a block reach for the wrong commands.
    if len == 0 {
        return MediaKind::Text;
    }

    // Valid UTF-8 in the first window is good evidence of text. It can be wrong
    // for a binary file that happens to begin with valid UTF-8, which is why
    // `Slice` still fails cleanly on bytes it cannot decode rather than trusting
    // this.
    match std::str::from_utf8(head) {
        Ok(_) => MediaKind::Text,
        // A trailing partial character means the window cut a multi-byte
        // sequence, which is what text looks like — not a reason to call it
        // binary.
        Err(e) if e.error_len().is_none() && e.valid_up_to() > 0 => MediaKind::Text,
        Err(_) => MediaKind::Binary,
    }
}
