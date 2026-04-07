use anyhow::{Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::RngCore;
use haven_protocol::{TranscriptSearchMatch, TranscriptSearchResults};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub type SearchMatch = TranscriptSearchMatch;
pub type SearchResults = TranscriptSearchResults;

/// Strip ANSI CSI/OSC escape sequences from a line so previews are readable.
/// Keeps printable characters and tabs.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b && i + 1 < bytes.len() {
            // ESC sequence
            let next = bytes[i + 1];
            if next == b'[' {
                // CSI: ESC [ ... final byte in 0x40..=0x7e
                i += 2;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
                continue;
            } else if next == b']' {
                // OSC: ESC ] ... BEL or ESC \
                i += 2;
                while i < bytes.len() && bytes[i] != 0x07 {
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == 0x07 {
                    i += 1;
                }
                continue;
            } else {
                // Other 2-byte escape
                i += 2;
                continue;
            }
        }
        if b == b'\t' || b >= 0x20 {
            // Push one UTF-8 code point starting at i.
            let ch_end = utf8_char_end(bytes, i);
            out.push_str(&s[i..ch_end]);
            i = ch_end;
        } else {
            i += 1;
        }
    }
    out
}

fn utf8_char_end(bytes: &[u8], i: usize) -> usize {
    let b = bytes[i];
    let len = if b < 0x80 {
        1
    } else if b < 0xc0 {
        1 // continuation byte, shouldn't start here
    } else if b < 0xe0 {
        2
    } else if b < 0xf0 {
        3
    } else {
        4
    };
    (i + len).min(bytes.len())
}

/// Maximum transcript size before truncation (100 MB of ciphertext).
/// ML workloads can produce large amounts of output (training logs, nvidia-smi,
/// long-running inference) — 10 MB was too aggressive and lost history on
/// reattach. 100 MB at ~1 KB/line is roughly 100k lines per session.
const MAX_TRANSCRIPT_SIZE: u64 = 100 * 1024 * 1024;

/// Each encrypted chunk: 4-byte length prefix (big-endian) + 12-byte nonce + ciphertext + 16-byte tag.
/// We encrypt in chunks to allow append-only writes and partial reads.
const NONCE_SIZE: usize = 12;
const TAG_SIZE: usize = 16;
const LEN_PREFIX: usize = 4;

/// Append-only encrypted transcript writer for a session.
/// Each write produces one encrypted chunk. Chunks are self-contained
/// so partial reads and truncation work without re-encrypting everything.
pub struct TranscriptWriter {
    file: File,
    path: PathBuf,
    key_path: PathBuf,
    cipher: ChaCha20Poly1305,
    bytes_written: u64,
}

impl TranscriptWriter {
    /// Create a new transcript writer for the given session directory.
    /// Generates a new encryption key if one doesn't exist, or loads the existing one.
    pub fn new(session_dir: &Path) -> Result<Self> {
        fs::create_dir_all(session_dir)
            .with_context(|| format!("Failed to create session dir: {}", session_dir.display()))?;

        let path = session_dir.join("transcript.bin");
        let key_path = session_dir.join("transcript.key");

        // Load or generate encryption key
        let key = if key_path.exists() {
            let key_bytes = fs::read(&key_path)
                .with_context(|| format!("Failed to read transcript key: {}", key_path.display()))?;
            if key_bytes.len() != 32 {
                // Key file corrupted — regenerate and start fresh transcript
                let key = Self::generate_key(&key_path)?;
                // Truncate existing transcript since we can't decrypt it
                File::create(&path).context("Failed to reset transcript")?;
                key
            } else {
                *Key::from_slice(&key_bytes)
            }
        } else {
            Self::generate_key(&key_path)?
        };

        let cipher = ChaCha20Poly1305::new(&key);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("Failed to open transcript: {}", path.display()))?;

        let bytes_written = file.metadata()?.len();

        Ok(TranscriptWriter {
            file,
            path,
            key_path,
            cipher,
            bytes_written,
        })
    }

    /// Generate a random 256-bit key and write it to disk with restricted permissions.
    fn generate_key(key_path: &Path) -> Result<Key> {
        let mut key_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut key_bytes);

        // Write key with restricted permissions (0600)
        fs::write(key_path, &key_bytes)
            .with_context(|| format!("Failed to write transcript key: {}", key_path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(key_path, fs::Permissions::from_mode(0o600))?;
        }

        Ok(*Key::from_slice(&key_bytes))
    }

    /// Append data to the transcript as an encrypted chunk.
    /// Format: [4-byte chunk length (big-endian)] [12-byte nonce] [ciphertext + 16-byte tag]
    pub fn append(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        // Generate random nonce
        let mut nonce_bytes = [0u8; NONCE_SIZE];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        // Encrypt
        let ciphertext = self.cipher
            .encrypt(nonce, data)
            .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

        // Write chunk: length prefix + nonce + ciphertext
        let chunk_len = (NONCE_SIZE + ciphertext.len()) as u32;
        self.file.write_all(&chunk_len.to_be_bytes())?;
        self.file.write_all(&nonce_bytes)?;
        self.file.write_all(&ciphertext)?;

        self.bytes_written += (LEN_PREFIX + NONCE_SIZE + ciphertext.len()) as u64;

        // Truncate if too large
        if self.bytes_written > MAX_TRANSCRIPT_SIZE {
            self.truncate()?;
        }

        Ok(())
    }

    /// Read and decrypt the last `n` bytes of plaintext from the transcript.
    pub fn read_last(&self, n: u64) -> Result<Vec<u8>> {
        let chunks = self.read_all_chunks()?;

        // Collect from the end until we have enough plaintext
        let mut result: Vec<u8> = Vec::new();
        for chunk in chunks.iter().rev() {
            result.splice(0..0, chunk.iter().cloned());
            if result.len() as u64 >= n {
                break;
            }
        }

        // Trim to requested size
        if result.len() as u64 > n {
            let skip = result.len() - n as usize;
            result = result[skip..].to_vec();
        }

        Ok(result)
    }

    /// Read and decrypt a range from the transcript.
    pub fn read_range(&self, offset: u64, length: u64) -> Result<Vec<u8>> {
        let all = self.read_all_plaintext()?;
        let total = all.len() as u64;

        if offset >= total {
            return Ok(vec![]);
        }

        let end = std::cmp::min(offset + length, total) as usize;
        Ok(all[offset as usize..end].to_vec())
    }

    /// Get total bytes written (ciphertext, for size tracking).
    pub fn total_bytes(&self) -> u64 {
        self.bytes_written
    }

    /// Search the decrypted transcript for `pattern`. Returns up to `limit`
    /// matches, each with its byte offset in the plaintext stream and a short
    /// preview line (the containing line, clipped).
    ///
    /// `regex = false`: plain substring search (fast, cheap).
    /// `regex = true`: `pattern` is compiled as a Rust regex.
    /// `case_insensitive`: applies to both modes.
    pub fn search(
        &self,
        pattern: &str,
        case_insensitive: bool,
        regex: bool,
        limit: usize,
    ) -> Result<SearchResults> {
        if pattern.is_empty() {
            return Ok(TranscriptSearchResults { matches: vec![], total: 0, truncated: false });
        }

        let all = self.read_all_plaintext()?;
        // Scrub ANSI escape sequences for matching purposes only — we still
        // return offsets into the raw stream so the client can replay around
        // them, but matches on color codes would be useless noise.
        let text = String::from_utf8_lossy(&all);

        let mut matches: Vec<TranscriptSearchMatch> = Vec::new();
        let mut total = 0usize;

        let push_match = |matches: &mut Vec<TranscriptSearchMatch>, total: &mut usize, start: usize, end: usize| {
            *total += 1;
            if matches.len() >= limit {
                return;
            }
            // Find line boundaries around the match.
            let line_start = text[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let line_end = text[end..]
                .find('\n')
                .map(|i| end + i)
                .unwrap_or(text.len());
            let raw_line = &text[line_start..line_end];
            let preview = strip_ansi(raw_line);
            // Line number = count of '\n' before line_start.
            let line_number = text[..line_start].bytes().filter(|&b| b == b'\n').count() + 1;
            matches.push(TranscriptSearchMatch {
                offset: start as u64,
                line_number: line_number as u64,
                preview,
            });
        };

        if regex {
            let pat = if case_insensitive {
                format!("(?i){pattern}")
            } else {
                pattern.to_string()
            };
            let re = regex::Regex::new(&pat)
                .map_err(|e| anyhow::anyhow!("Invalid regex: {e}"))?;
            for m in re.find_iter(&text) {
                push_match(&mut matches, &mut total, m.start(), m.end());
                if total > limit * 4 && matches.len() >= limit {
                    break;
                }
            }
        } else if case_insensitive {
            let needle = pattern.to_lowercase();
            let hay = text.to_lowercase();
            let mut start = 0usize;
            while let Some(idx) = hay[start..].find(&needle) {
                let abs = start + idx;
                push_match(&mut matches, &mut total, abs, abs + needle.len());
                start = abs + needle.len().max(1);
                if total > limit * 4 && matches.len() >= limit {
                    break;
                }
            }
        } else {
            let mut start = 0usize;
            while let Some(idx) = text[start..].find(pattern) {
                let abs = start + idx;
                push_match(&mut matches, &mut total, abs, abs + pattern.len());
                start = abs + pattern.len().max(1);
                if total > limit * 4 && matches.len() >= limit {
                    break;
                }
            }
        }

        Ok(TranscriptSearchResults {
            truncated: total > matches.len(),
            matches,
            total,
        })
    }

    /// Read all chunks from the transcript file and decrypt them.
    fn read_all_chunks(&self) -> Result<Vec<Vec<u8>>> {
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return Ok(vec![]),
        };
        let file_size = file.metadata()?.len();
        if file_size == 0 {
            return Ok(vec![]);
        }

        file.seek(SeekFrom::Start(0))?;
        let mut chunks = Vec::new();

        loop {
            // Read length prefix
            let mut len_buf = [0u8; LEN_PREFIX];
            match file.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
            let chunk_len = u32::from_be_bytes(len_buf) as usize;

            if chunk_len < NONCE_SIZE + TAG_SIZE {
                // Corrupted chunk — stop reading
                tracing::warn!("Corrupted transcript chunk (too small), stopping read");
                break;
            }

            // Read nonce + ciphertext
            let mut chunk_data = vec![0u8; chunk_len];
            match file.read_exact(&mut chunk_data) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }

            let nonce = Nonce::from_slice(&chunk_data[..NONCE_SIZE]);
            let ciphertext = &chunk_data[NONCE_SIZE..];

            match self.cipher.decrypt(nonce, ciphertext) {
                Ok(plaintext) => chunks.push(plaintext),
                Err(_) => {
                    // Corrupted or wrong-key chunk — skip
                    tracing::warn!("Failed to decrypt transcript chunk, skipping");
                }
            }
        }

        Ok(chunks)
    }

    /// Read all plaintext from the transcript.
    fn read_all_plaintext(&self) -> Result<Vec<u8>> {
        let chunks = self.read_all_chunks()?;
        let mut all = Vec::new();
        for chunk in chunks {
            all.extend_from_slice(&chunk);
        }
        Ok(all)
    }

    /// Truncate the transcript, keeping approximately the last half of data.
    fn truncate(&mut self) -> Result<()> {
        let chunks = self.read_all_chunks()?;
        if chunks.is_empty() {
            return Ok(());
        }

        // Keep roughly the last half of plaintext
        let total_plain: usize = chunks.iter().map(|c| c.len()).sum();
        let keep = total_plain / 2;

        let mut kept_plain = 0usize;
        let mut start_idx = chunks.len();
        for (i, chunk) in chunks.iter().enumerate().rev() {
            kept_plain += chunk.len();
            start_idx = i;
            if kept_plain >= keep {
                break;
            }
        }

        // Re-encrypt kept chunks into a fresh file
        let tmp_path = self.path.with_extension("bin.tmp");
        {
            let mut tmp = File::create(&tmp_path)?;
            let mut new_size = 0u64;

            for chunk in &chunks[start_idx..] {
                let mut nonce_bytes = [0u8; NONCE_SIZE];
                OsRng.fill_bytes(&mut nonce_bytes);
                let nonce = Nonce::from_slice(&nonce_bytes);

                let ciphertext = self.cipher
                    .encrypt(nonce, chunk.as_slice())
                    .map_err(|e| anyhow::anyhow!("Re-encryption failed: {}", e))?;

                let chunk_len = (NONCE_SIZE + ciphertext.len()) as u32;
                tmp.write_all(&chunk_len.to_be_bytes())?;
                tmp.write_all(&nonce_bytes)?;
                tmp.write_all(&ciphertext)?;
                new_size += (LEN_PREFIX + NONCE_SIZE + ciphertext.len()) as u64;
            }

            self.bytes_written = new_size;
        }

        // Atomic replace
        fs::rename(&tmp_path, &self.path)?;

        // Reopen in append mode
        self.file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .context("Failed to reopen transcript after truncate")?;

        tracing::info!("Truncated transcript to {} bytes", self.bytes_written);
        Ok(())
    }
}
