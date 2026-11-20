//! Where files come from and go: [`Source`] reads a file being sent piece by piece, [`Sink`] takes a file being
//! received in order, and [`FileSink`] writes one into a directory, under its cleaned name only once it is whole and
//! verified.

use super::frame::clean_name;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// A file being sent, read one piece at a time: never held whole.
pub trait Source {
    /// Fills `buf` with the bytes from `offset` on. An error (the file shrank, was moved, or can't be read) fails
    /// the transfer.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
}

impl<T: Read + Seek> Source for T {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.seek(SeekFrom::Start(offset))?;
        self.read_exact(buf)
    }
}

/// Where a file being received goes: its pieces in order, each checked against its SHA-256 before it is written.
pub trait Sink {
    /// Writes the next piece.
    fn write(&mut self, data: &[u8]) -> io::Result<()>;
    /// The whole file is in and its SHA-256 matches the sender's: keep it.
    fn finish(&mut self) -> io::Result<()>;
    /// The transfer ended without a verified file: drop what was written.
    fn abort(&mut self);
    /// Where the file was kept, once [`finish`](Sink::finish) did.
    fn saved_as(&self) -> Option<PathBuf> {
        None
    }
}

/// Bytes of a file name on disk at most: under the 255 most file systems take, with room for ` (999)`.
const MAX_NAME_BYTES: usize = 200;

/// Writes a received file into a directory: to a hidden temporary file (`.peerwire-<random>.part`) as pieces
/// arrive, then, once the whole file's SHA-256 matches, under its name (cleaned by the protocol already), or
/// `name (2).ext` and on if that is taken. It never replaces a file: names are claimed with `create_new`.
pub struct FileSink {
    dir: PathBuf,
    name: String,
    part: Option<(PathBuf, File)>,
    saved: Option<PathBuf>,
}

impl FileSink {
    /// A sink for a file named `name` in `dir`, which must exist. `name` is cleaned ([`clean_name`]) whatever it is,
    /// so the file lands in `dir` even for `../x` or a path. An [`Offer`](super::Offer)'s name is clean already: at
    /// most its Unicode normal form changes (NFC may compose marks that a character the first cleaning dropped kept
    /// apart).
    pub fn create(dir: &Path, name: &str) -> io::Result<FileSink> {
        let mut rnd = [0u8; 8];
        rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut rnd);
        let path = dir.join(format!(".peerwire-{}.part", super::frame::hex(&rnd)));
        let file = OpenOptions::new().write(true).create_new(true).open(&path)?;
        let name = fit(&clean_name(name));
        Ok(FileSink { dir: dir.to_path_buf(), name, part: Some((path, file)), saved: None })
    }
}

impl Sink for FileSink {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        match &mut self.part {
            Some((_, f)) => f.write_all(data),
            None => Err(io::Error::other("the file was closed")),
        }
    }

    fn finish(&mut self) -> io::Result<()> {
        let (part, file) = self.part.take().ok_or_else(|| io::Error::other("the file was closed"))?;
        let done = file.sync_all().and_then(|_| {
            drop(file);
            // Claim a free name with create_new (it never follows a link or replaces a file), then move the
            // finished file over our own empty claim.
            let (stem, ext) = split_ext(&self.name);
            for n in 1..1000 {
                let name = if n == 1 { self.name.clone() } else { format!("{stem} ({n}){ext}") };
                let path = self.dir.join(name);
                match OpenOptions::new().write(true).create_new(true).open(&path) {
                    Ok(_) => {
                        if let Err(e) = fs::rename(&part, &path) {
                            let _ = fs::remove_file(&path);
                            return Err(e);
                        }
                        self.saved = Some(path);
                        return Ok(());
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free name for the file"))
        });
        if done.is_err() {
            let _ = fs::remove_file(&part);
        }
        done
    }

    fn abort(&mut self) {
        if let Some((part, file)) = self.part.take() {
            drop(file);
            let _ = fs::remove_file(part);
        }
    }

    fn saved_as(&self) -> Option<PathBuf> {
        self.saved.clone()
    }
}

impl Drop for FileSink {
    fn drop(&mut self) {
        self.abort();
    }
}

// The name, cut to MAX_NAME_BYTES of UTF-8 at a character boundary, its extension (up to 16 bytes) kept.
fn fit(name: &str) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name.to_string();
    }
    let (stem, ext) = match split_ext(name) {
        (stem, ext) if ext.len() <= 16 => (stem, ext),
        _ => (name, ""),
    };
    let mut end = MAX_NAME_BYTES - ext.len();
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ext}", stem[..end].trim_end_matches([' ', '.']))
}

// "a.tar.gz" -> ("a.tar", ".gz"); no extension for a name with no dot past its first character.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(at) if at > 0 => name.split_at(at),
        _ => (name, ""),
    }
}
