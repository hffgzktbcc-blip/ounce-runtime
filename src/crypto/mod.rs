#![allow(dead_code)]

//! FIPS-197 AES-128 primitives (ECB and CTR) used by the synthetic test containers.
//!
//! This module deliberately has no key discovery, key-file parsing, or XTS support: the runtime does not
//! handle retail keys.

const SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

const INV_SBOX: [u8; 256] = {
    let mut inv = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        inv[SBOX[i] as usize] = i as u8;
        i += 1;
    }
    inv
};

const RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1B, 0x36];

#[derive(Debug, Clone)]
pub struct Aes128 {
    round_keys: [[u8; 16]; 11],
}

impl Aes128 {
    pub fn new(key: &[u8; 16]) -> Self {
        let mut round_keys = [[0u8; 16]; 11];
        round_keys[0].copy_from_slice(key);

        for r in 1..=10 {
            let prev = round_keys[r - 1];
            let mut temp = [prev[13], prev[14], prev[15], prev[12]];
            for b in &mut temp {
                *b = SBOX[*b as usize];
            }
            temp[0] ^= RCON[r - 1];

            let mut next = [0u8; 16];
            for i in 0..4 {
                next[i] = prev[i] ^ temp[i];
                next[4 + i] = prev[4 + i] ^ next[i];
                next[8 + i] = prev[8 + i] ^ next[4 + i];
                next[12 + i] = prev[12 + i] ^ next[8 + i];
            }
            round_keys[r] = next;
        }

        Self { round_keys }
    }

    /// Encrypts a single 16-byte block using AES-128.
    pub fn encrypt_block(&self, input: &[u8; 16]) -> [u8; 16] {
        let mut s = *input;
        add_round_key(&mut s, &self.round_keys[0]);

        for r in 1..10 {
            sub_bytes(&mut s);
            shift_rows(&mut s);
            mix_columns(&mut s);
            add_round_key(&mut s, &self.round_keys[r]);
        }

        sub_bytes(&mut s);
        shift_rows(&mut s);
        add_round_key(&mut s, &self.round_keys[10]);
        s
    }

    /// Decrypts a single 16-byte block using AES-128.
    pub fn decrypt_block(&self, input: &[u8; 16]) -> [u8; 16] {
        let mut s = *input;
        add_round_key(&mut s, &self.round_keys[10]);

        for r in (1..10).rev() {
            inv_shift_rows(&mut s);
            inv_sub_bytes(&mut s);
            add_round_key(&mut s, &self.round_keys[r]);
            inv_mix_columns(&mut s);
        }

        inv_shift_rows(&mut s);
        inv_sub_bytes(&mut s);
        add_round_key(&mut s, &self.round_keys[0]);
        s
    }

    /// AES-128-ECB in-place decryption (used for NCA `key_area` and `titlekek`).
    pub fn decrypt_ecb(&self, data: &mut [u8]) {
        for chunk in data.chunks_exact_mut(16) {
            let block: [u8; 16] = chunk.try_into().unwrap();
            let plain = self.decrypt_block(&block);
            chunk.copy_from_slice(&plain);
        }
    }

    /// AES-128-ECB in-place encryption.
    pub fn encrypt_ecb(&self, data: &mut [u8]) {
        for chunk in data.chunks_exact_mut(16) {
            let block: [u8; 16] = chunk.try_into().unwrap();
            let enc = self.encrypt_block(&block);
            chunk.copy_from_slice(&enc);
        }
    }

    /// AES-128-CTR encryption / decryption in-place starting at block 0 of `iv`.
    pub fn crypt_ctr(&self, iv: &[u8; 16], data: &mut [u8]) {
        self.crypt_ctr_at_offset(iv, 0, data);
    }

    /// AES-128-CTR encryption / decryption at an arbitrary byte offset within the section/NCA.
    ///
    /// In Nintendo `NCA3` containers, each section's 64-bit upper nonce sits in `iv[0..8]` and the
    /// 64-bit big-endian block counter (`byte_offset >> 4`) sits in `iv[8..16]` when `base_offset > 0`,
    /// or `u128::from_be_bytes(*iv) + (byte_offset >> 4)` when `iv` already holds the starting counter.
    pub fn crypt_ctr_at_offset(&self, base_iv: &[u8; 16], byte_offset: u64, data: &mut [u8]) {
        let block_index = (byte_offset / 16) as u128;
        let intra_block = (byte_offset % 16) as usize;
        let mut counter = u128::from_be_bytes(*base_iv).wrapping_add(block_index);

        let mut pos = 0usize;
        if intra_block != 0 && !data.is_empty() {
            let keystream = self.encrypt_block(&counter.to_be_bytes());
            let take = (16 - intra_block).min(data.len());
            for i in 0..take {
                data[i] ^= keystream[intra_block + i];
            }
            pos += take;
            counter = counter.wrapping_add(1);
        }

        for chunk in data[pos..].chunks_mut(16) {
            let keystream = self.encrypt_block(&counter.to_be_bytes());
            for (b, k) in chunk.iter_mut().zip(keystream.iter()) {
                *b ^= *k;
            }
            counter = counter.wrapping_add(1);
        }
    }

    /// Builds a Nintendo NCA section CTR IV from the 8-byte section CTR header (`fs_header[0x140..0x148]`)
    /// and a physical NCA byte offset. Notice Nintendo stores `section_ctr` bytes reversed at `0x140..0x148`.
    pub fn make_nca_section_iv(section_ctr_le: &[u8; 8], physical_offset: u64) -> [u8; 16] {
        let mut iv = [0u8; 16];
        for i in 0..8 {
            iv[i] = section_ctr_le[7 - i];
        }
        iv[8..16].copy_from_slice(&(physical_offset >> 4).to_be_bytes());
        iv
    }
}

#[inline(always)]
fn add_round_key(state: &mut [u8; 16], rk: &[u8; 16]) {
    for i in 0..16 {
        state[i] ^= rk[i];
    }
}

#[inline(always)]
fn sub_bytes(s: &mut [u8; 16]) {
    for b in s.iter_mut() {
        *b = SBOX[*b as usize];
    }
}

#[inline(always)]
fn inv_sub_bytes(s: &mut [u8; 16]) {
    for b in s.iter_mut() {
        *b = INV_SBOX[*b as usize];
    }
}

#[inline(always)]
fn shift_rows(s: &mut [u8; 16]) {
    let t1 = s[1];
    s[1] = s[5];
    s[5] = s[9];
    s[9] = s[13];
    s[13] = t1;

    let t2 = s[2];
    let t6 = s[6];
    s[2] = s[10];
    s[6] = s[14];
    s[10] = t2;
    s[14] = t6;

    let t15 = s[15];
    s[15] = s[11];
    s[11] = s[7];
    s[7] = s[3];
    s[3] = t15;
}

#[inline(always)]
fn inv_shift_rows(s: &mut [u8; 16]) {
    let t13 = s[13];
    s[13] = s[9];
    s[9] = s[5];
    s[5] = s[1];
    s[1] = t13;

    let t2 = s[2];
    let t6 = s[6];
    s[2] = s[10];
    s[6] = s[14];
    s[10] = t2;
    s[14] = t6;

    let t3 = s[3];
    s[3] = s[7];
    s[7] = s[11];
    s[11] = s[15];
    s[15] = t3;
}

#[inline(always)]
fn gmul2(a: u8) -> u8 {
    if (a & 0x80) != 0 {
        (a << 1) ^ 0x1B
    } else {
        a << 1
    }
}

#[inline(always)]
fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if (b & 1) != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1B;
        }
        b >>= 1;
    }
    p
}

#[inline(always)]
fn mix_columns(s: &mut [u8; 16]) {
    for c in 0..4 {
        let idx = c * 4;
        let a0 = s[idx];
        let a1 = s[idx + 1];
        let a2 = s[idx + 2];
        let a3 = s[idx + 3];
        let all = a0 ^ a1 ^ a2 ^ a3;

        s[idx] ^= all ^ gmul2(a0 ^ a1);
        s[idx + 1] ^= all ^ gmul2(a1 ^ a2);
        s[idx + 2] ^= all ^ gmul2(a2 ^ a3);
        s[idx + 3] ^= all ^ gmul2(a3 ^ a0);
    }
}

#[inline(always)]
fn inv_mix_columns(s: &mut [u8; 16]) {
    for c in 0..4 {
        let idx = c * 4;
        let a0 = s[idx];
        let a1 = s[idx + 1];
        let a2 = s[idx + 2];
        let a3 = s[idx + 3];

        s[idx] = gmul(a0, 0x0e) ^ gmul(a1, 0x0b) ^ gmul(a2, 0x0d) ^ gmul(a3, 0x09);
        s[idx + 1] = gmul(a0, 0x09) ^ gmul(a1, 0x0e) ^ gmul(a2, 0x0b) ^ gmul(a3, 0x0d);
        s[idx + 2] = gmul(a0, 0x0d) ^ gmul(a1, 0x09) ^ gmul(a2, 0x0e) ^ gmul(a3, 0x0b);
        s[idx + 3] = gmul(a0, 0x0b) ^ gmul(a1, 0x0d) ^ gmul(a2, 0x09) ^ gmul(a3, 0x0e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aes128_nist_fips197_encrypt_and_decrypt() {
        let key: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        ];
        let plaintext: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        ];
        let expected: [u8; 16] = [
            0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30,
            0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4, 0xc5, 0x5a,
        ];

        let aes = Aes128::new(&key);
        let enc = aes.encrypt_block(&plaintext);
        assert_eq!(enc, expected);
        assert_eq!(aes.decrypt_block(&enc), plaintext);
    }

    #[test]
    fn test_aes128_ctr_roundtrip() {
        let key: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        ];
        let aes = Aes128::new(&key);
        let iv = [0xAAu8; 16];
        let original = b"Synthetic encrypted section payload (test vector)";
        let mut buf = original.to_vec();

        aes.crypt_ctr(&iv, &mut buf);
        assert_ne!(&buf[..], &original[..]);

        aes.crypt_ctr(&iv, &mut buf);
        assert_eq!(&buf[..], &original[..]);
    }
}
