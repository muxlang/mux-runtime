//! Cryptographic primitive and authenticated-payload coverage.
#![cfg(feature = "crypto")]

use mux_runtime::crypto::*;
use mux_runtime::refcount::{mux_rc_alloc, mux_rc_dec};
use mux_runtime::result::{mux_result_data, mux_result_is_ok};
use mux_runtime::std::{mux_crypto_error_detail, mux_crypto_error_kind};
use mux_runtime::Value;

fn bytes(value: &[u8]) -> *mut Value {
    mux_rc_alloc(Value::Bytes(value.to_vec()))
}

fn string(value: &str) -> *mut Value {
    mux_rc_alloc(Value::String(value.to_string()))
}

fn hex(value: &[u8]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

unsafe fn result_data(result: *mut Value) -> *mut Value {
    assert!(mux_result_is_ok(result), "expected Ok result: {result:?}");
    let value = mux_result_data(result);
    assert!(mux_rc_dec(result));
    value
}

#[test]
fn standard_hash_vectors_are_bytes() {
    unsafe {
        let input = bytes(b"");
        let sha256 = mux_crypto_sha256(input);
        assert!(
            matches!(&*sha256, Value::Bytes(value) if hex(value) == "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        let sha512 = mux_crypto_sha512(input);
        assert!(
            matches!(&*sha512, Value::Bytes(value) if hex(value) == "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e")
        );
        let sha3 = mux_crypto_sha3_256(input);
        assert!(
            matches!(&*sha3, Value::Bytes(value) if hex(value) == "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a")
        );
        let blake = mux_crypto_blake3(input);
        let Value::Bytes(blake_bytes) = &*blake else {
            panic!("expected bytes");
        };
        assert_eq!(
            hex(blake_bytes),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert!(mux_rc_dec(input));
        assert!(mux_rc_dec(sha256));
        assert!(mux_rc_dec(sha512));
        assert!(mux_rc_dec(sha3));
        assert!(mux_rc_dec(blake));
    }
}

#[test]
fn hmac_and_secure_random_outputs_have_expected_shapes() {
    unsafe {
        let key = bytes(b"key");
        let message = bytes(b"The quick brown fox jumps over the lazy dog");
        let mac = result_data(mux_crypto_hmac_sha256(key, message));
        assert!(matches!(&*mac, Value::Bytes(value) if value.len() == 32));
        assert!(mux_rc_dec(mac));
        let token = result_data(mux_crypto_random_token(24));
        assert!(
            matches!(&*token, Value::String(value) if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'))
        );
        assert!(mux_rc_dec(token));
        let generated = result_data(mux_crypto_generate_key());
        assert!(matches!(&*generated, Value::Bytes(value) if value.len() == 32));
        assert!(mux_rc_dec(generated));
        assert!(mux_rc_dec(key));
        assert!(mux_rc_dec(message));
    }
}

#[test]
fn sealed_payloads_round_trip_and_authenticate() {
    unsafe {
        let key = result_data(mux_crypto_generate_key());
        let plaintext = bytes(b"secret message");
        let aad = bytes(b"header");
        let sealed = result_data(mux_crypto_seal_aes256_gcm(key, plaintext, aad));
        assert!(matches!(&*sealed, Value::Bytes(value) if value.starts_with(b"MXSE")));
        let opened = result_data(mux_crypto_open(key, sealed, aad));
        assert!(matches!(&*opened, Value::Bytes(value) if value == b"secret message"));
        assert!(mux_rc_dec(opened));
        let wrong_aad = bytes(b"wrong");
        let failed = mux_crypto_open(key, sealed, wrong_aad);
        assert!(!mux_result_is_ok(failed));
        let error = mux_result_data(failed);
        assert_eq!(direct_i32(mux_crypto_error_kind(error)), 2);
        assert!(direct_string(mux_crypto_error_detail(error)).contains("authentication"));
        assert!(mux_rc_dec(error));
        assert!(mux_rc_dec(failed));
        assert!(mux_rc_dec(wrong_aad));
        let chacha = result_data(mux_crypto_seal_chacha20_poly1305(key, plaintext, aad));
        let opened_chacha = result_data(mux_crypto_open(key, chacha, aad));
        assert!(matches!(&*opened_chacha, Value::Bytes(value) if value == b"secret message"));
        assert!(mux_rc_dec(opened_chacha));
        assert!(mux_rc_dec(chacha));
        assert!(mux_rc_dec(sealed));
        assert!(mux_rc_dec(plaintext));
        assert!(mux_rc_dec(aad));
        assert!(mux_rc_dec(key));
    }
}

fn direct_i32(value: *mut Value) -> i32 {
    unsafe {
        let output = match &*value {
            Value::Opaque(bytes) => i32::from_ne_bytes(bytes.as_ref().try_into().unwrap()),
            other => panic!("expected Opaque, got {other:?}"),
        };
        assert!(mux_rc_dec(value));
        output
    }
}

fn direct_string(value: *mut Value) -> String {
    unsafe {
        let output = match &*value {
            Value::String(value) => value.clone(),
            other => panic!("expected String, got {other:?}"),
        };
        assert!(mux_rc_dec(value));
        output
    }
}

#[test]
fn encrypted_file_helpers_stream_chunks_and_authenticate_metadata() {
    let stem = format!(
        "mux-crypto-file-test-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    );
    let stem: String = stem
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    let input_path = std::env::temp_dir().join(format!("{stem}-input"));
    let hardlink_path = std::env::temp_dir().join(format!("{stem}-hardlink"));
    let sealed_path = std::env::temp_dir().join(format!("{stem}-sealed"));
    let corrupted_path = std::env::temp_dir().join(format!("{stem}-corrupted"));
    let output_path = std::env::temp_dir().join(format!("{stem}-output"));
    let mut input = Vec::with_capacity(1_500_000);
    input.extend((0..1_500_000).map(|index| (index % 251) as u8));
    std::fs::write(&input_path, &input).expect("write input");

    unsafe {
        let key = result_data(mux_crypto_generate_key());
        let input_value = string(input_path.to_str().unwrap());
        let sealed_value = string(sealed_path.to_str().unwrap());
        let output_value = string(output_path.to_str().unwrap());
        let aad = bytes(b"file context");
        let in_place = mux_crypto_seal_file(key, input_value, input_value, aad);
        assert!(!mux_result_is_ok(in_place));
        assert!(mux_rc_dec(in_place));
        assert_eq!(
            std::fs::read(&input_path).expect("read unchanged input"),
            input
        );
        if std::fs::hard_link(&input_path, &hardlink_path).is_ok() {
            let hardlink_value = string(hardlink_path.to_str().unwrap());
            let hardlink_in_place = mux_crypto_seal_file(key, hardlink_value, hardlink_value, aad);
            assert!(!mux_result_is_ok(hardlink_in_place));
            assert!(mux_rc_dec(hardlink_in_place));
            assert!(mux_rc_dec(hardlink_value));
            assert_eq!(
                std::fs::read(&input_path).expect("read unchanged hard-linked input"),
                input
            );
        }
        let sealed = mux_crypto_seal_file(key, input_value, sealed_value, aad);
        assert!(mux_result_is_ok(sealed));
        assert!(mux_rc_dec(sealed));
        let opened = mux_crypto_open_file(key, sealed_value, output_value, aad);
        assert!(mux_result_is_ok(opened));
        assert!(mux_rc_dec(opened));
        assert_eq!(std::fs::read(&output_path).expect("read output"), input);

        // Authentication can fail after earlier records have already been
        // decrypted. The existing destination must remain untouched rather
        // than exposing partial unauthenticated plaintext.
        std::fs::write(&output_path, b"keep this destination").expect("write sentinel");
        let wrong_aad = bytes(b"wrong");
        let failed = mux_crypto_open_file(key, sealed_value, output_value, wrong_aad);
        assert!(!mux_result_is_ok(failed));
        assert!(mux_rc_dec(failed));
        assert!(mux_rc_dec(wrong_aad));
        assert_eq!(
            std::fs::read(&output_path).expect("read unchanged destination"),
            b"keep this destination"
        );

        // Corrupt the second record so the failure occurs after the first
        // record has authenticated and been processed.
        let mut corrupted = std::fs::read(&sealed_path).expect("read sealed file");
        let header_length = 26;
        let first_length = u32::from_le_bytes(
            corrupted[header_length..header_length + 4]
                .try_into()
                .unwrap(),
        ) as usize;
        let second_start = header_length + 4 + 12 + first_length + 16;
        let second_length = u32::from_le_bytes(
            corrupted[second_start..second_start + 4]
                .try_into()
                .unwrap(),
        ) as usize;
        let second_tag = second_start + 4 + 12 + second_length + 15;
        corrupted[second_tag] ^= 1;
        std::fs::write(&corrupted_path, corrupted).expect("write corrupted sealed file");
        let corrupted_value = string(corrupted_path.to_str().unwrap());
        let late_failed = mux_crypto_open_file(key, corrupted_value, output_value, aad);
        assert!(!mux_result_is_ok(late_failed));
        assert!(mux_rc_dec(late_failed));
        assert_eq!(
            std::fs::read(&output_path).expect("read unchanged destination after late failure"),
            b"keep this destination"
        );

        let original = std::fs::read(&sealed_path).expect("read sealed file");
        let mut offset = header_length;
        let mut record_lengths = Vec::new();
        while offset < original.len() {
            let length =
                u32::from_le_bytes(original[offset..offset + 4].try_into().unwrap()) as usize;
            let record_end = offset + 4 + 12 + length + 16;
            record_lengths.push(length);
            offset = record_end;
        }
        assert_eq!(offset, original.len());
        assert!(record_lengths.len() >= 2);
        assert_eq!(record_lengths.last(), Some(&0));
        let final_start = second_start + 4 + 12 + second_length + 16;
        let resealed = mux_crypto_seal_file(key, input_value, corrupted_value, aad);
        assert!(mux_result_is_ok(resealed));
        assert!(mux_rc_dec(resealed));
        let other_file = std::fs::read(&corrupted_path).expect("read independently sealed file");
        let mut spliced = original.clone();
        spliced[second_start..final_start].copy_from_slice(&other_file[second_start..final_start]);
        let mut appended = original.clone();
        appended.push(0);
        let mut changed_header = original.clone();
        changed_header[10] ^= 1;
        for malformed in [
            original[..header_length].to_vec(),
            original[..second_start].to_vec(),
            original[..final_start].to_vec(),
            spliced,
            appended,
            changed_header,
        ] {
            std::fs::write(&corrupted_path, malformed).expect("write malformed file");
            let failed = mux_crypto_open_file(key, corrupted_value, output_value, aad);
            assert!(!mux_result_is_ok(failed));
            assert!(mux_rc_dec(failed));
            assert_eq!(
                std::fs::read(&output_path).expect("read destination after malformed file"),
                b"keep this destination"
            );
        }

        std::fs::write(&input_path, []).expect("empty input");
        let sealed = mux_crypto_seal_file(key, input_value, sealed_value, aad);
        assert!(mux_result_is_ok(sealed));
        assert!(mux_rc_dec(sealed));
        std::fs::write(&output_path, b"keep this destination after wrong key")
            .expect("write wrong-key sentinel");
        let wrong_key = result_data(mux_crypto_generate_key());
        let failed = mux_crypto_open_file(wrong_key, sealed_value, output_value, aad);
        assert!(!mux_result_is_ok(failed));
        assert!(mux_rc_dec(failed));
        assert!(mux_rc_dec(wrong_key));
        assert_eq!(
            std::fs::read(&output_path).expect("read unchanged wrong-key destination"),
            b"keep this destination after wrong key"
        );
        let opened = mux_crypto_open_file(key, sealed_value, output_value, aad);
        assert!(mux_result_is_ok(opened));
        assert!(mux_rc_dec(opened));
        assert!(std::fs::read(&output_path)
            .expect("read empty output")
            .is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&output_path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(mux_rc_dec(corrupted_value));

        assert!(mux_rc_dec(aad));
        assert!(mux_rc_dec(key));
        assert!(mux_rc_dec(input_value));
        assert!(mux_rc_dec(sealed_value));
        assert!(mux_rc_dec(output_value));
    }
    let _ = std::fs::remove_file(input_path);
    let _ = std::fs::remove_file(hardlink_path);
    let _ = std::fs::remove_file(sealed_path);
    let _ = std::fs::remove_file(corrupted_path);
    let _ = std::fs::remove_file(output_path);
}
