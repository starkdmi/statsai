//! JSONL logs a provider compressed in place.
//!
//! Codex compresses older rollouts where they lie: `rollout-….jsonl` becomes
//! `rollout-….jsonl.zst`, a zstd stream of the same lines, and the plain file is
//! removed. The `.jsonl` path stays the file's identity -- its scan cache key, the
//! file hash its records carry, the session id derived from it -- so compressing a
//! file changes how its lines are stored, not which file the store holds them for.
//! Discovery lists a compressed file under its `.jsonl` path, and the readers here
//! open whichever of the two exists.

use anyhow::{Context, Result};
use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};
use statsai_core::canonical_display;
use std::borrow::Cow;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

const JSONL_EXTENSION: &str = "jsonl";
const ZSTD_EXTENSION: &str = "zst";

/// The `.jsonl` files under `root`, sorted by path, with each `X.jsonl.zst` listed
/// as `X.jsonl`.
///
/// While both exist, compression is still under way and the plain file is the
/// whole one, so it is listed and the compressed copy is not. A compressed file is
/// listed under its canonical `.jsonl` path: that is the cache key the plain file
/// had, which a path that does not exist could not otherwise be canonicalized to.
pub(crate) fn collect_jsonl_files_with_compressed(root: &Path) -> Result<Vec<PathBuf>> {
    let (plain, compressed): (Vec<_>, Vec<_>) = crate::collect_files(root, |path| {
        is_plain_jsonl(path) || uncompressed_name(path).is_some()
    })?
    .into_iter()
    .partition(|path| is_plain_jsonl(path));

    let listed = plain
        .iter()
        .map(|path| path.as_os_str().to_owned())
        .collect::<HashSet<_>>();
    let mut files = plain;
    for path in compressed {
        let Some(name) = uncompressed_name(&path) else {
            continue;
        };
        if listed.contains(path.with_file_name(name).as_os_str()) {
            continue;
        }
        let canonical = canonical_display(&path);
        let logical = canonical
            .strip_suffix(".zst")
            .map_or_else(|| path.with_file_name(name), PathBuf::from);
        files.push(logical);
    }
    files.sort_by_cached_key(|path| path.to_string_lossy().into_owned());
    Ok(files)
}

fn is_plain_jsonl(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some(JSONL_EXTENSION)
}

/// `X.jsonl` for a file named `X.jsonl.zst`.
fn uncompressed_name(path: &Path) -> Option<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".zst"))
        .filter(|name| {
            Path::new(name).extension().and_then(|ext| ext.to_str()) == Some(JSONL_EXTENSION)
        })
}

/// `path` with `.zst` appended.
fn compressed_path(path: &Path) -> PathBuf {
    let mut compressed = path.as_os_str().to_owned();
    compressed.push(".zst");
    PathBuf::from(compressed)
}

/// The file `path`'s lines are in: `path` itself, or its compressed copy once
/// only that exists. A file compressed between discovery and reading is still
/// found.
pub fn readable_jsonl_path(path: &Path) -> Cow<'_, Path> {
    if path.exists() {
        return Cow::Borrowed(path);
    }
    let compressed = compressed_path(path);
    if compressed.is_file() {
        Cow::Owned(compressed)
    } else {
        Cow::Borrowed(path)
    }
}

/// Opens `path`, or its compressed copy, for reading lines.
pub(crate) fn open_jsonl(path: &Path) -> Result<Box<dyn BufRead + Send>> {
    let readable = readable_jsonl_path(path);
    let file = File::open(&readable).with_context(|| format!("read {}", readable.display()))?;
    let reader = BufReader::new(file);
    if readable.extension().and_then(|ext| ext.to_str()) == Some(ZSTD_EXTENSION) {
        Ok(Box::new(BufReader::new(ZstdFrames::new(reader))))
    } else {
        Ok(Box::new(reader))
    }
}

/// Decompresses every frame of a zstd stream, skipping skippable frames.
///
/// `ruzstd`'s `StreamingDecoder` stops after the first frame, and a zstd file may
/// hold several.
struct ZstdFrames<R> {
    source: R,
    decoder: FrameDecoder,
    in_frame: bool,
}

impl<R: BufRead> ZstdFrames<R> {
    fn new(source: R) -> Self {
        Self {
            source,
            decoder: FrameDecoder::new(),
            in_frame: false,
        }
    }
}

impl<R: BufRead> Read for ZstdFrames<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.in_frame {
                while self.decoder.can_collect() < buf.len() && !self.decoder.is_finished() {
                    let needed = buf.len() - self.decoder.can_collect();
                    self.decoder
                        .decode_blocks(&mut self.source, BlockDecodingStrategy::UptoBytes(needed))
                        .map_err(io::Error::other)?;
                }
                let read = self.decoder.read(buf)?;
                if read > 0 {
                    return Ok(read);
                }
                // The frame is read out, so the checksum of what it held is
                // complete. `ruzstd` computes it but leaves the comparison to
                // the caller; a frame written without one has nothing to check.
                if let Some(stored) = self.decoder.get_checksum_from_data() {
                    if self.decoder.get_calculated_checksum() != Some(stored) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "zstd frame checksum does not match its content",
                        ));
                    }
                }
                self.in_frame = false;
            }
            if self.source.fill_buf()?.is_empty() {
                return Ok(0);
            }
            match self.decoder.reset(&mut self.source) {
                Ok(()) => self.in_frame = true,
                Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                    length,
                    ..
                })) => {
                    let skipped = io::copy(
                        &mut (&mut self.source).take(u64::from(length)),
                        &mut io::sink(),
                    )?;
                    // A stream that ends inside the frame is cut short, not complete.
                    if skipped != u64::from(length) {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "zstd skippable frame is truncated",
                        ));
                    }
                }
                Err(error) => return Err(io::Error::other(error)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruzstd::encoding::{compress_to_vec, CompressionLevel};

    fn compressed(text: &str) -> Vec<u8> {
        compress_to_vec(text.as_bytes(), CompressionLevel::Fastest)
    }

    fn read_all(path: &Path) -> String {
        let mut text = String::new();
        open_jsonl(path)
            .expect("open")
            .read_to_string(&mut text)
            .expect("read");
        text
    }

    #[test]
    fn a_compressed_rollout_is_listed_and_read_under_its_jsonl_path() {
        let root = tempfile::tempdir().expect("root");
        let day = root.path().join("2026/05/28");
        std::fs::create_dir_all(&day).expect("day");
        std::fs::write(day.join("rollout-a.jsonl.zst"), compressed("{\"a\":1}\n")).expect("a");
        std::fs::write(day.join("rollout-b.jsonl"), "{\"b\":1}\n").expect("b");
        // Mid-compression both exist: the plain file is the whole one.
        std::fs::write(day.join("rollout-c.jsonl"), "{\"c\":1}\n").expect("c");
        std::fs::write(day.join("rollout-c.jsonl.zst"), compressed("{\"c\"")).expect("c.zst");
        std::fs::write(day.join("notes.txt.zst"), compressed("not a log")).expect("other");

        let files = collect_jsonl_files_with_compressed(root.path()).expect("list");
        let canonical_day = PathBuf::from(canonical_display(&day));
        assert_eq!(
            files,
            vec![
                canonical_day.join("rollout-a.jsonl"),
                day.join("rollout-b.jsonl"),
                day.join("rollout-c.jsonl"),
            ]
        );
        // The listed path is the cache key the plain file had.
        assert_eq!(
            canonical_display(&files[0]),
            canonical_day.join("rollout-a.jsonl").to_string_lossy()
        );
        assert_eq!(read_all(&files[0]), "{\"a\":1}\n");
        assert_eq!(read_all(&files[2]), "{\"c\":1}\n");
    }

    #[test]
    fn a_rollout_compressed_after_listing_is_still_read() {
        let root = tempfile::tempdir().expect("root");
        let plain = root.path().join("rollout.jsonl");
        std::fs::write(
            root.path().join("rollout.jsonl.zst"),
            compressed("{\"x\":1}\n"),
        )
        .expect("compressed");
        assert_eq!(
            readable_jsonl_path(&plain),
            root.path().join("rollout.jsonl.zst")
        );
        assert_eq!(read_all(&plain), "{\"x\":1}\n");
    }

    #[test]
    fn every_frame_is_read_and_skippable_frames_are_skipped() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("rollout.jsonl.zst");
        let mut bytes = compressed("{\"first\":1}\n");
        // A skippable frame: magic 0x184D2A50, a little-endian length, then that many bytes.
        bytes.extend_from_slice(&0x184D_2A50_u32.to_le_bytes());
        bytes.extend_from_slice(&3_u32.to_le_bytes());
        bytes.extend_from_slice(b"pad");
        bytes.extend(compressed("{\"second\":2}\n"));
        std::fs::write(&path, bytes).expect("write");

        assert_eq!(
            read_all(&root.path().join("rollout.jsonl")),
            "{\"first\":1}\n{\"second\":2}\n"
        );
    }

    /// `{"x":1}\n` as `zstd --check` writes it: one frame with a content checksum
    /// in its last four bytes.
    const CHECKSUMMED_FRAME: [u8; 21] = [
        0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x58, 0x41, 0x00, 0x00, 0x7b, 0x22, 0x78, 0x22, 0x3a, 0x31,
        0x7d, 0x0a, 0x22, 0x03, 0xde, 0x93,
    ];

    fn read_frame(bytes: &[u8]) -> io::Result<String> {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("rollout.jsonl.zst"), bytes).expect("write");
        let mut text = String::new();
        open_jsonl(&root.path().join("rollout.jsonl"))
            .expect("open")
            .read_to_string(&mut text)?;
        Ok(text)
    }

    #[test]
    fn a_frame_checksum_is_verified() {
        assert_eq!(read_frame(&CHECKSUMMED_FRAME).expect("read"), "{\"x\":1}\n");

        let mut wrong_checksum = CHECKSUMMED_FRAME;
        wrong_checksum[20] ^= 0xff;
        let error = read_frame(&wrong_checksum).expect_err("checksum mismatch");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        // A changed byte of content, which the checksum no longer matches.
        let mut wrong_content = CHECKSUMMED_FRAME;
        wrong_content[13] = b'2';
        assert!(read_frame(&wrong_content).is_err());
    }

    #[test]
    fn a_truncated_skippable_frame_fails_to_read() {
        let mut bytes = compressed("{\"first\":1}\n");
        // Declares eight bytes and holds three: the copy was cut short.
        bytes.extend_from_slice(&0x184D_2A50_u32.to_le_bytes());
        bytes.extend_from_slice(&8_u32.to_le_bytes());
        bytes.extend_from_slice(b"pad");
        let error = read_frame(&bytes).expect_err("truncated frame");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_corrupt_compressed_rollout_fails_to_read() {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("rollout.jsonl.zst"), b"not zstd").expect("write");
        let mut text = String::new();
        let read = open_jsonl(&root.path().join("rollout.jsonl"))
            .expect("open")
            .read_to_string(&mut text);
        assert!(read.is_err());
    }
}
