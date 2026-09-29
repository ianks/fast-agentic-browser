//! Evidence that a login in the password manager is the one fab generated
//! (INTENT I09, I08).
//!
//! After a restart the generated value is gone: only the password manager has
//! it. A reference alone cannot tell fab's own login from one that was already
//! saved for the same site and account, so a [`Verifier`] is recorded next to
//! the workflow reference. It is HMAC-SHA256 over the generated password under
//! a random 32-byte key, so the workflow file proves a candidate without ever
//! holding the password or anything from which it can be recovered: the tag
//! only answers "this exact value", never what the value is, and a generated
//! password (20 characters, 78-symbol alphabet) is not recoverable from a
//! keyed tag without the key in the same 0600 file.
//!
//! The key is per workflow and stored beside the tag, in the file that is only
//! ever written with mode 0600. It buys domain separation and makes a stolen
//! tag useless against a precomputed table; it is not a substitute for the
//! file's permissions.
//!
//! No hash crate is available to fab-core offline (`--locked --offline`: the
//! workspace has no `sha2`/`hmac` dependency, and the transitive `sha1`/`ring`
//! copies belong to other crates), so SHA-256 (FIPS 180-4) and HMAC (RFC 2104)
//! are implemented here and pinned to the published test vectors in the tests
//! below.

use base64::Engine;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD_NO_PAD;

/// Proof that a password value is the generated one: a random key and the
/// keyed tag of the value. Safe to persist: it is not the password, and it
/// cannot be inverted into it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verifier {
    /// base64 of a random 32-byte HMAC key.
    key: String,
    /// base64 of HMAC-SHA256(key, password).
    tag: String,
}

impl Verifier {
    /// A new verifier over `secret` under a fresh random key.
    pub fn of(secret: &str) -> anyhow::Result<Self> {
        let mut key = Zeroizing::new([0u8; 32]);
        getrandom::fill(key.as_mut()).map_err(|_| anyhow::anyhow!("no system randomness for a generated-password verifier"))?;
        let tag = hmac(&key[..], secret.as_bytes());
        Ok(Self { key: B64.encode(&*key), tag: B64.encode(&tag[..]) })
    }

    /// Whether `candidate` is the value this verifier was made for. Compared
    /// in time that does not depend on where the first difference is.
    pub fn matches(&self, candidate: &str) -> bool {
        let (Ok(key), Ok(tag)) = (B64.decode(&self.key), B64.decode(&self.tag)) else { return false };
        if key.len() != 32 || tag.len() != 32 {
            return false;
        }
        equal(&*hmac(&Zeroizing::new(key), candidate.as_bytes()), &tag)
    }
}

/// Byte equality that inspects every byte.
fn equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// SHA-256 of `msg` (FIPS 180-4).
fn sha256(msg: &[u8]) -> Zeroizing<[u8; 32]> {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
        0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut block = Zeroizing::new(Vec::with_capacity(msg.len() + 72));
    block.extend_from_slice(msg);
    block.push(0x80);
    while block.len() % 64 != 56 {
        block.push(0);
    }
    block.extend_from_slice(&((msg.len() as u64) * 8).to_be_bytes());
    for chunk in block.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h2] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h2.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h2 = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (i, v) in [a, b, c, d, e, f, g, h2].into_iter().enumerate() {
            h[i] = h[i].wrapping_add(v);
        }
    }
    let mut out = Zeroizing::new([0u8; 32]);
    for (i, v) in h.into_iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// HMAC-SHA256 (RFC 2104).
fn hmac(key: &[u8], msg: &[u8]) -> Zeroizing<[u8; 32]> {
    const BLOCK: usize = 64;
    let mut k = Zeroizing::new([0u8; BLOCK]);
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&*sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let (mut ipad, mut opad) = ([0x36u8; BLOCK], [0x5cu8; BLOCK]);
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Zeroizing::new(Vec::with_capacity(BLOCK + msg.len()));
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(msg);
    let inner = Zeroizing::new(*sha256(&inner));
    let mut outer = Zeroizing::new(Vec::with_capacity(BLOCK + 32));
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner[..]);
    sha256(&outer)
}

#[cfg(test)]
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 against the FIPS 180-4 / NIST published vectors.
    #[test]
    fn sha256_matches_published_vectors() {
        for (input, want) in [
            ("", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
            ("abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            ("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq", "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"),
            (
                "abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
                "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1",
            ),
        ] {
            assert_eq!(hex(&*sha256(input.as_bytes())), want, "{input:?}");
        }
        // A million 'a's (NIST): the padding path across many blocks.
        assert_eq!(hex(&*sha256(&[b'a'; 1_000_000])), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    /// HMAC-SHA256 against RFC 4231 test cases 1, 2, 3, 6 and 7.
    #[test]
    fn hmac_sha256_matches_rfc4231_vectors() {
        let cases: &[(&[u8], &[u8], &str)] = &[
            (&[0x0b; 20], b"Hi There", "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"),
            (b"Jefe", b"what do ya want for nothing?", "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"),
            (&[0xaa; 20], &[0xdd; 50], "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"),
            (&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First", "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"),
            (&[0xaa; 131], b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm.", "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"),
        ];
        for (key, msg, want) in cases {
            assert_eq!(hex(&*hmac(key, msg)), *want, "{msg:?}");
        }
    }

    /// A verifier accepts its own value, rejects any other, and a record of it
    /// is not the password.
    #[test]
    fn verifier_proves_one_value_only() {
        let secret = "kR7!vQ2z#mL9pXw4$Bn6";
        let v = Verifier::of(secret).unwrap();
        assert!(v.matches(secret));
        assert!(!v.matches("kR7!vQ2z#mL9pXw4$Bn7"));
        assert!(!v.matches(""));
        assert!(!v.matches(&secret.to_lowercase()));
        // A different key over the same value is a different verifier.
        assert_ne!(Verifier::of(secret).unwrap(), v);
        // The serialized record holds neither the value nor any part of it.
        let json = serde_json::to_string(&v).unwrap();
        assert!(!json.contains(secret), "{json}");
        assert_eq!(B64.decode(&v.key).unwrap().len(), 32);
        assert_eq!(B64.decode(&v.tag).unwrap().len(), 32);
        assert!(!json.contains("correct horse"), "{json}");
    }

    /// A record whose key or tag was tampered with (or lost) proves nothing and
    /// is treated as "not this password" rather than panicking.
    #[test]
    fn verifier_refuses_a_damaged_record() {
        let v = Verifier::of("s3cret-value").unwrap();
        for (key, tag) in [(String::new(), v.tag.clone()), (v.key.clone(), "not base64!".into()), (v.key.clone(), String::new())] {
            assert!(!Verifier { key, tag }.matches("s3cret-value"));
        }
    }
}
