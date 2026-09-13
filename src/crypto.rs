//! Bytes-only cryptographic primitives for `std.crypto`.
//!
//! The authenticated-encryption format is intentionally self-describing:
//! `MXSE`, format version, algorithm id, 12-byte nonce, and ciphertext plus
//! authentication tag. Keys and messages never cross this boundary as strings.

#![allow(clippy::missing_safety_doc)]

use crate::refcount::mux_rc_alloc;
use crate::std::StdErrorKind;
use crate::Value;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{KeyInit as HmacKeyInit, Mac};
use sha2::Digest as Sha2Digest;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_RANDOM_BYTES: i64 = 16 * 1024 * 1024;
const KEY_LENGTH: usize = 32;
const NONCE_LENGTH: usize = 12;
const MAGIC: &[u8; 4] = b"MXSE";
const VERSION: u8 = 1;
const AES_ALGORITHM: u8 = 1;
const CHACHA_ALGORITHM: u8 = 2;
const FILE_MAGIC: &[u8; 4] = b"MXSF";
const FILE_VERSION: u8 = 2;
const FILE_ID_LENGTH: usize = 16;
const FILE_HEADER_LENGTH: usize = 10 + FILE_ID_LENGTH;
const FILE_CHUNK_SIZE: usize = 1024 * 1024;
const FILE_TEMP_ATTEMPTS: u64 = 128;

static FILE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn error(message: impl Into<String>) -> *mut Value {
    crate::std::crypto_result_err(message.into())
}

fn error_with_kind(kind: StdErrorKind, message: impl Into<String>) -> *mut Value {
    crate::std::crypto_result_err_kind(kind, message.into())
}

struct CryptoFailure {
    kind: StdErrorKind,
    detail: String,
}

impl CryptoFailure {
    fn invalid(detail: impl Into<String>) -> Self {
        Self {
            kind: StdErrorKind::Invalid,
            detail: detail.into(),
        }
    }
}

fn ok(value: Value) -> *mut Value {
    mux_rc_alloc(Value::Result(Ok(Box::new(value))))
}

fn bytes(value: *const Value, name: &str) -> Result<Vec<u8>, String> {
    match unsafe { value.as_ref() } {
        Some(Value::Bytes(value)) => Ok(value.clone()),
        Some(_) | None => Err(format!("expected {name} as bytes")),
    }
}

fn result_bytes(result: Result<Vec<u8>, String>) -> *mut Value {
    match result {
        Ok(value) => ok(Value::Bytes(value)),
        Err(message) => error(message),
    }
}

fn digest<D: Sha2Digest>(input: &[u8]) -> Vec<u8> {
    D::digest(input).to_vec()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_crypto_sha256(value: *const Value) -> *mut Value {
    mux_rc_alloc(Value::Bytes(
        bytes(value, "input").map_or_else(|_| Vec::new(), |input| digest::<sha2::Sha256>(&input)),
    ))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_crypto_sha512(value: *const Value) -> *mut Value {
    mux_rc_alloc(Value::Bytes(
        bytes(value, "input").map_or_else(|_| Vec::new(), |input| digest::<sha2::Sha512>(&input)),
    ))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_crypto_sha3_256(value: *const Value) -> *mut Value {
    mux_rc_alloc(Value::Bytes(bytes(value, "input").map_or_else(
        |_| Vec::new(),
        |input| <sha3::Sha3_256 as sha3::Digest>::digest(&input).to_vec(),
    )))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_crypto_sha3_512(value: *const Value) -> *mut Value {
    mux_rc_alloc(Value::Bytes(bytes(value, "input").map_or_else(
        |_| Vec::new(),
        |input| <sha3::Sha3_512 as sha3::Digest>::digest(&input).to_vec(),
    )))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_crypto_blake3(value: *const Value) -> *mut Value {
    mux_rc_alloc(Value::Bytes(bytes(value, "input").map_or_else(
        |_| Vec::new(),
        |input| blake3::hash(&input).as_bytes().to_vec(),
    )))
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Result<Vec<u8>, String> {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(key)
        .map_err(|_| "invalid HMAC key".to_string())?;
    mac.update(message);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn hmac_sha512(key: &[u8], message: &[u8]) -> Result<Vec<u8>, String> {
    let mut mac = hmac::Hmac::<sha2::Sha512>::new_from_slice(key)
        .map_err(|_| "invalid HMAC key".to_string())?;
    mac.update(message);
    Ok(mac.finalize().into_bytes().to_vec())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_crypto_hmac_sha256(
    key: *const Value,
    message: *const Value,
) -> *mut Value {
    let result = bytes(key, "key")
        .and_then(|key| bytes(message, "message").and_then(|message| hmac_sha256(&key, &message)));
    result_bytes(result)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_crypto_hmac_sha512(
    key: *const Value,
    message: *const Value,
) -> *mut Value {
    let result = bytes(key, "key")
        .and_then(|key| bytes(message, "message").and_then(|message| hmac_sha512(&key, &message)));
    result_bytes(result)
}

fn random(length: i64) -> Result<Vec<u8>, String> {
    if !(0..=MAX_RANDOM_BYTES).contains(&length) {
        return Err(format!(
            "length must be between 0 and {MAX_RANDOM_BYTES} bytes"
        ));
    }
    let mut value = vec![0; usize::try_from(length).map_err(|_| "length is too large")?];
    getrandom::fill(&mut value)
        .map_err(|error| format!("secure random generation failed: {error}"))?;
    Ok(value)
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_crypto_random_bytes(length: i64) -> *mut Value {
    result_bytes(random(length))
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_crypto_random_token(length: i64) -> *mut Value {
    match random(length) {
        Ok(value) => ok(Value::String(URL_SAFE_NO_PAD.encode(value))),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mux_crypto_generate_key() -> *mut Value {
    result_bytes(random(KEY_LENGTH as i64))
}

fn key(value: *const Value) -> Result<Vec<u8>, String> {
    let key = bytes(value, "key")?;
    if key.len() != KEY_LENGTH {
        return Err("key must contain exactly 32 bytes".to_string());
    }
    Ok(key)
}

fn nonce() -> Result<[u8; NONCE_LENGTH], String> {
    let mut nonce = [0; NONCE_LENGTH];
    getrandom::fill(&mut nonce)
        .map_err(|error| format!("secure random generation failed: {error}"))?;
    Ok(nonce)
}

fn encrypt_with_nonce(
    algorithm: u8,
    key: &[u8],
    nonce: &[u8; NONCE_LENGTH],
    plaintext: &[u8],
    associated_data: &[u8],
) -> Result<Vec<u8>, String> {
    match algorithm {
        AES_ALGORITHM => aes_gcm::Aes256Gcm::new_from_slice(key)
            .map_err(|_| "invalid AES-256-GCM key".to_string())?
            .encrypt(
                aes_gcm::Nonce::from_slice(nonce),
                Payload {
                    msg: plaintext,
                    aad: associated_data,
                },
            )
            .map_err(|_| "AES-256-GCM encryption failed".to_string()),
        CHACHA_ALGORITHM => chacha20poly1305::ChaCha20Poly1305::new_from_slice(key)
            .map_err(|_| "invalid ChaCha20-Poly1305 key".to_string())?
            .encrypt(
                chacha20poly1305::Nonce::from_slice(nonce),
                Payload {
                    msg: plaintext,
                    aad: associated_data,
                },
            )
            .map_err(|_| "ChaCha20-Poly1305 encryption failed".to_string()),
        _ => Err("unknown sealed-payload algorithm".to_string()),
    }
}

fn seal_with_algorithm(
    algorithm: u8,
    key: &[u8],
    plaintext: &[u8],
    associated_data: &[u8],
) -> Result<Vec<u8>, String> {
    let nonce = nonce()?;
    let ciphertext = encrypt_with_nonce(algorithm, key, &nonce, plaintext, associated_data)?;
    let mut sealed = Vec::with_capacity(4 + 1 + 1 + NONCE_LENGTH + ciphertext.len());
    sealed.extend_from_slice(MAGIC);
    sealed.push(VERSION);
    sealed.push(algorithm);
    sealed.extend_from_slice(&nonce);
    sealed.extend_from_slice(&ciphertext);
    Ok(sealed)
}

fn open_sealed(
    key: &[u8],
    sealed: &[u8],
    associated_data: &[u8],
) -> Result<Vec<u8>, CryptoFailure> {
    let header = 4 + 1 + 1 + NONCE_LENGTH;
    if sealed.len() < header + 16 {
        return Err(CryptoFailure::invalid("sealed payload is truncated"));
    }
    if &sealed[..4] != MAGIC {
        return Err(CryptoFailure::invalid(
            "sealed payload has an invalid magic header",
        ));
    }
    if sealed[4] != VERSION {
        return Err(CryptoFailure {
            kind: StdErrorKind::Unsupported,
            detail: "sealed payload version is unsupported".to_string(),
        });
    }
    let algorithm = sealed[5];
    let nonce = &sealed[6..header];
    let ciphertext = &sealed[header..];
    let nonce: &[u8; NONCE_LENGTH] = nonce
        .try_into()
        .map_err(|_| CryptoFailure::invalid("sealed payload nonce is malformed"))?;
    match algorithm {
        AES_ALGORITHM => aes_gcm::Aes256Gcm::new_from_slice(key)
            .map_err(|_| CryptoFailure::invalid("invalid AES-256-GCM key"))?
            .decrypt(
                aes_gcm::Nonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: associated_data,
                },
            )
            .map_err(|_| CryptoFailure {
                kind: StdErrorKind::Authentication,
                detail: "sealed payload authentication failed".to_string(),
            }),
        CHACHA_ALGORITHM => chacha20poly1305::ChaCha20Poly1305::new_from_slice(key)
            .map_err(|_| CryptoFailure::invalid("invalid ChaCha20-Poly1305 key"))?
            .decrypt(
                chacha20poly1305::Nonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: associated_data,
                },
            )
            .map_err(|_| CryptoFailure {
                kind: StdErrorKind::Authentication,
                detail: "sealed payload authentication failed".to_string(),
            }),
        _ => Err(CryptoFailure {
            kind: StdErrorKind::Unsupported,
            detail: "sealed payload algorithm is unsupported".to_string(),
        }),
    }
}

fn string(value: *const Value, name: &str) -> Result<String, String> {
    match unsafe { value.as_ref() } {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) | None => Err(format!("expected {name} as string")),
    }
}

fn file_record_aad(
    associated_data: &[u8],
    header: &[u8; FILE_HEADER_LENGTH],
    index: u64,
    length: u32,
) -> Vec<u8> {
    let mut aad = Vec::new();
    aad.extend_from_slice(header);
    aad.extend_from_slice(&index.to_le_bytes());
    aad.extend_from_slice(&length.to_le_bytes());
    aad.extend_from_slice(associated_data);
    aad
}

fn read_exact_or_eof(file: &mut File, buffer: &mut [u8]) -> Result<bool, String> {
    let mut read = 0;
    while read < buffer.len() {
        let count = file
            .read(&mut buffer[read..])
            .map_err(|error| format!("failed to read encrypted file: {error}"))?;
        if count == 0 {
            if read == 0 {
                return Ok(true);
            }
            return Err("encrypted file is truncated".to_string());
        }
        read += count;
    }
    Ok(false)
}

/// Keep file transforms from replacing their source, including path aliases.
fn ensure_distinct_file_paths(input_path: &str, output_path: &str) -> Result<(), String> {
    let input = fs::canonicalize(input_path)
        .map_err(|error| format!("failed to resolve input file '{input_path}': {error}"))?;
    let output = Path::new(output_path);
    if output.exists() {
        let output_metadata = fs::metadata(output_path)
            .map_err(|error| format!("failed to inspect output file '{output_path}': {error}"))?;
        let output = fs::canonicalize(output_path)
            .map_err(|error| format!("failed to resolve output file '{output_path}': {error}"))?;
        let input_metadata = fs::metadata(input_path)
            .map_err(|error| format!("failed to inspect input file '{input_path}': {error}"))?;
        if input == output || same_file_metadata(&input_metadata, &output_metadata) {
            return Err("input and output paths must refer to different files".to_string());
        }
    }
    Ok(())
}

/// Canonical paths catch symlink aliases, but hard links need file identities.
fn same_file_metadata(input: &fs::Metadata, output: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        input.dev() == output.dev() && input.ino() == output.ino()
    }
    #[cfg(windows)]
    {
        let _ = (input, output);
        false
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (input, output);
        false
    }
}

/// Build the file operation's result beside the requested destination and
/// publish it only after every record has been authenticated and flushed.
/// Keeping the temporary file in the destination directory is what makes the
/// final rename atomic on filesystems that provide atomic rename semantics.
struct AtomicFileOutput {
    path: PathBuf,
    file: Option<File>,
}

impl AtomicFileOutput {
    fn create(destination: &str) -> Result<Self, String> {
        let destination = Path::new(destination);
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        let name = destination
            .file_name()
            .ok_or_else(|| format!("output path '{destination:?}' has no file name"))?;

        for _ in 0..FILE_TEMP_ATTEMPTS {
            let counter = FILE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let temporary_name = format!(
                ".{}.mux-crypto-{}-{counter}",
                name.to_string_lossy(),
                std::process::id()
            );
            let path = parent.join(temporary_name);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600))
                        {
                            let _ = fs::remove_file(&path);
                            return Err(format!(
                                "failed to set temporary output permissions beside '{destination:?}': {error}"
                            ));
                        }
                    }
                    return Ok(Self {
                        path,
                        file: Some(file),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "failed to create temporary output beside '{destination:?}': {error}"
                    ));
                }
            }
        }
        Err(format!(
            "could not allocate a temporary output beside '{destination:?}'"
        ))
    }

    fn file_mut(&mut self) -> Result<&mut File, String> {
        self.file
            .as_mut()
            .ok_or_else(|| "atomic output file is no longer open".to_string())
    }

    fn commit(mut self, destination: &Path) -> Result<(), String> {
        let file = self
            .file
            .take()
            .ok_or_else(|| "atomic output file is no longer open".to_string())?;
        file.sync_all()
            .map_err(|error| format!("failed to flush temporary encrypted file: {error}"))?;
        drop(file);
        replace_file_atomically(&self.path, destination).map_err(|error| {
            format!(
                "failed to publish encrypted output '{:?}': {error}",
                destination
            )
        })
    }
}

impl Drop for AtomicFileOutput {
    fn drop(&mut self) {
        // A failed seal/open must not leave a partial or unauthenticated file
        // visible. Ignore cleanup errors because the original operation error
        // is the useful result for the caller.
        let _ = self.file.take();
        let _ = fs::remove_file(&self.path);
    }
}

fn replace_file_atomically(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let source_wide: Vec<u16> = source
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let destination_wide: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let succeeded = unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                source_wide.as_ptr(),
                destination_wide.as_ptr(),
                windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING,
            )
        } != 0;
        if succeeded {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(windows))]
    {
        fs::rename(source, destination)
    }
}

fn seal_file_impl(
    key: &[u8],
    input_path: &str,
    output_path: &str,
    associated_data: &[u8],
) -> Result<(), String> {
    ensure_distinct_file_paths(input_path, output_path)?;
    let mut input = File::open(input_path)
        .map_err(|error| format!("failed to open input file '{input_path}': {error}"))?;
    let mut header = [0u8; FILE_HEADER_LENGTH];
    header[..4].copy_from_slice(FILE_MAGIC);
    header[4] = FILE_VERSION;
    header[5] = AES_ALGORITHM;
    header[6..10].copy_from_slice(&(FILE_CHUNK_SIZE as u32).to_le_bytes());
    getrandom::fill(&mut header[10..])
        .map_err(|error| format!("failed to generate encrypted-file identity: {error}"))?;
    let mut output = AtomicFileOutput::create(output_path)?;
    output
        .file_mut()?
        .write_all(&header)
        .map_err(|error| format!("failed to write encrypted-file header: {error}"))?;

    let mut chunk = vec![0; FILE_CHUNK_SIZE];
    let mut index = 0u64;
    loop {
        let count = input
            .read(&mut chunk)
            .map_err(|error| format!("failed to read input file '{input_path}': {error}"))?;
        let nonce = nonce()?;
        let length = u32::try_from(count).map_err(|_| "file chunk is too large".to_string())?;
        let record_aad = file_record_aad(associated_data, &header, index, length);
        let ciphertext =
            encrypt_with_nonce(AES_ALGORITHM, key, &nonce, &chunk[..count], &record_aad)?;
        output
            .file_mut()?
            .write_all(&length.to_le_bytes())
            .map_err(|error| format!("failed to write encrypted file: {error}"))?;
        output
            .file_mut()?
            .write_all(&nonce)
            .map_err(|error| format!("failed to write encrypted file: {error}"))?;
        output
            .file_mut()?
            .write_all(&ciphertext)
            .map_err(|error| format!("failed to write encrypted file: {error}"))?;
        // An authenticated empty record marks EOF, including for empty files.
        if count == 0 {
            break;
        }
        index = index
            .checked_add(1)
            .ok_or_else(|| "encrypted file has too many chunks".to_string())?;
    }
    output.commit(Path::new(output_path))
}

fn open_file_impl(
    key: &[u8],
    input_path: &str,
    output_path: &str,
    associated_data: &[u8],
) -> Result<(), String> {
    ensure_distinct_file_paths(input_path, output_path)?;
    let mut input = File::open(input_path)
        .map_err(|error| format!("failed to open encrypted file '{input_path}': {error}"))?;
    let mut header = [0u8; FILE_HEADER_LENGTH];
    if read_exact_or_eof(&mut input, &mut header)? {
        return Err("encrypted file is empty".to_string());
    }
    if &header[..4] != FILE_MAGIC || header[4] != FILE_VERSION {
        return Err("encrypted file has an unsupported header".to_string());
    }
    if header[5] != AES_ALGORITHM {
        return Err("encrypted file algorithm is unsupported".to_string());
    }
    let chunk_size = match header[6..10].try_into() {
        Ok(bytes) => u32::from_le_bytes(bytes) as usize,
        Err(_) => return Err("encrypted file header is truncated".to_string()),
    };
    if chunk_size == 0 || chunk_size > FILE_CHUNK_SIZE {
        return Err("encrypted file chunk size is invalid".to_string());
    }
    let mut output = AtomicFileOutput::create(output_path)?;
    let mut length = [0u8; 4];
    let mut nonce = [0u8; NONCE_LENGTH];
    let mut index = 0u64;
    loop {
        if read_exact_or_eof(&mut input, &mut length)? {
            return Err("encrypted file is missing its final record".to_string());
        }
        let record_length = u32::from_le_bytes(length);
        let length = record_length as usize;
        if length > chunk_size {
            return Err("encrypted file record length is invalid".to_string());
        }
        if read_exact_or_eof(&mut input, &mut nonce)? {
            return Err("encrypted file record is truncated".to_string());
        }
        let ciphertext_length = length
            .checked_add(16)
            .ok_or_else(|| "encrypted file record is too large".to_string())?;
        let mut ciphertext = vec![0; ciphertext_length];
        if read_exact_or_eof(&mut input, &mut ciphertext)? {
            return Err("encrypted file record is truncated".to_string());
        }
        let record_aad = file_record_aad(associated_data, &header, index, record_length);
        let Ok(plaintext) = aes_gcm::Aes256Gcm::new_from_slice(key)
            .map_err(|_| "invalid AES-256-GCM key".to_string())?
            .decrypt(
                aes_gcm::Nonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &record_aad,
                },
            )
        else {
            return Err("encrypted file authentication failed".to_string());
        };
        if plaintext.len() != length {
            return Err("encrypted file record length does not match plaintext".to_string());
        }
        if length == 0 {
            if !read_exact_or_eof(&mut input, &mut [0u8; 1])? {
                return Err("encrypted file has data after its final record".to_string());
            }
            break;
        }
        output
            .file_mut()?
            .write_all(&plaintext)
            .map_err(|error| format!("failed to write decrypted file: {error}"))?;
        index = index
            .checked_add(1)
            .ok_or_else(|| "encrypted file has too many chunks".to_string())?;
    }
    output.commit(Path::new(output_path))
}

fn seal_args(
    key_value: *const Value,
    plaintext_value: *const Value,
    aad_value: *const Value,
    algorithm: u8,
) -> *mut Value {
    let result = key(key_value).and_then(|key| {
        bytes(plaintext_value, "plaintext").and_then(|plaintext| {
            bytes(aad_value, "associated data")
                .and_then(|aad| seal_with_algorithm(algorithm, &key, &plaintext, &aad))
        })
    });
    result_bytes(result)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_crypto_seal_aes256_gcm(
    key: *const Value,
    plaintext: *const Value,
    associated_data: *const Value,
) -> *mut Value {
    seal_args(key, plaintext, associated_data, AES_ALGORITHM)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_crypto_seal_chacha20_poly1305(
    key: *const Value,
    plaintext: *const Value,
    associated_data: *const Value,
) -> *mut Value {
    seal_args(key, plaintext, associated_data, CHACHA_ALGORITHM)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_crypto_open(
    key_value: *const Value,
    sealed_value: *const Value,
    associated_data: *const Value,
) -> *mut Value {
    let result = key(key_value)
        .map_err(CryptoFailure::invalid)
        .and_then(|key| {
            bytes(sealed_value, "sealed payload")
                .map_err(CryptoFailure::invalid)
                .and_then(|sealed| {
                    bytes(associated_data, "associated data")
                        .map_err(CryptoFailure::invalid)
                        .and_then(|aad| open_sealed(&key, &sealed, &aad))
                })
        });
    match result {
        Ok(value) => ok(Value::Bytes(value)),
        Err(failure) => error_with_kind(failure.kind, failure.detail),
    }
}

fn file_args(
    key_value: *const Value,
    input_value: *const Value,
    output_value: *const Value,
    aad_value: *const Value,
    open: bool,
) -> *mut Value {
    let result = key(key_value).and_then(|key| {
        string(input_value, "input path").and_then(|input_path| {
            string(output_value, "output path").and_then(|output_path| {
                bytes(aad_value, "associated data").and_then(|aad| {
                    if open {
                        open_file_impl(&key, &input_path, &output_path, &aad)
                    } else {
                        seal_file_impl(&key, &input_path, &output_path, &aad)
                    }
                })
            })
        })
    });
    match result {
        Ok(()) => ok(Value::Unit),
        Err(message) => error(message),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_crypto_seal_file(
    key: *const Value,
    input_path: *const Value,
    output_path: *const Value,
    associated_data: *const Value,
) -> *mut Value {
    file_args(key, input_path, output_path, associated_data, false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mux_crypto_open_file(
    key: *const Value,
    input_path: *const Value,
    output_path: *const Value,
    associated_data: *const Value,
) -> *mut Value {
    file_args(key, input_path, output_path, associated_data, true)
}
