// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

use ring::digest::{digest, SHA256};

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let d = digest(&SHA256, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

pub fn verify(data: &[u8], expected: &[u8; 32]) -> bool {
    sha256(data) == *expected
}

#[cfg(test)]
mod tests {
    use super::{sha256, verify};

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn matches_fips_vectors() {
        assert_eq!(
            to_hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            to_hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verify_rejects_bit_flip_and_bad_digest() {
        let mut data = vec![0u8; 4096];
        let sum = sha256(&data);
        assert!(verify(&data, &sum));
        let mut bad = sum;
        bad[31] ^= 0x80;
        assert!(!verify(&data, &bad));
        data[2048] ^= 1;
        assert!(!verify(&data, &sum));
    }
}
