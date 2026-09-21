//! Post-processing of assembled DEX bytes.
//!
//! The `smali` crate always assembles with a fixed header (`dex\n039\0`) and
//! regenerates a full `debug_info_item` for every method that carries any
//! `.line`/`.local` directive. Both differ from what the input APK had, so this
//! module puts the output back in line with it:
//!
//! * [`strip_debug_info`] drops the debug-only pseudo ops before assembly. An
//!   R8 release build keeps a dense line table (one entry per instruction); a
//!   round trip through smali re-emits all of it, which for a real app is
//!   *megabytes*. Debug info never affects execution, only stack traces.
//! * [`set_dex_version`] rewrites the header magic back to the version the
//!   input declared, then recomputes the SHA-1 signature and the Adler-32
//!   checksum so the file stays verifiable. ART only accepts a DEX version it
//!   knows about: shipping `039` to `minSdkVersion 26` (Android 8.0, DEX 038)
//!   makes the app fail to load on those devices.

use smali::types::{SmaliClass, SmaliOp};

/// Remove every debug-only pseudo op and parameter name from `classes`.
///
/// After this, `DexFile::from_smali` finds no line/name/debug events for a
/// method and emits no `debug_info_item` at all (see `build_debug_info` in the
/// crate's `dex::builder`), instead of synthesising a line table for it.
pub fn strip_debug_info(classes: &mut [SmaliClass]) {
    for class in classes {
        for method in &mut class.methods {
            method.ops.retain(|op| {
                !matches!(
                    op,
                    SmaliOp::Line(_)
                        | SmaliOp::Prologue
                        | SmaliOp::Epilogue
                        | SmaliOp::Local { .. }
                        | SmaliOp::EndLocal { .. }
                        | SmaliOp::RestartLocal { .. }
                )
            });
            for param in &mut method.params {
                param.name = None;
            }
        }
    }
}

/// Rewrite the DEX header of `bytes` so it reports `version` (35, 37, 38, 39…)
/// and its signature/checksum match again.
///
/// Returns `Ok(false)` when nothing had to change.
pub fn set_dex_version(bytes: &mut [u8], version: u32) -> Result<bool, String> {
    if bytes.len() < 32 || !bytes.starts_with(b"dex\n") {
        return Err("not a dex file".to_string());
    }
    let digits = format!("{version:03}");
    if digits.len() != 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("invalid dex version {version}"));
    }
    if bytes[4..7] == *digits.as_bytes() {
        return Ok(false);
    }
    bytes[4..7].copy_from_slice(digits.as_bytes());
    // signature = SHA-1 over everything after the signature field.
    let digest = sha1(&bytes[32..]);
    bytes[12..32].copy_from_slice(&digest);
    // checksum = Adler-32 over everything after the checksum field.
    let checksum = adler32(&bytes[12..]);
    bytes[8..12].copy_from_slice(&checksum.to_le_bytes());
    Ok(true)
}

/// Minimal SHA-1 (RFC 3174). Used only for the 20-byte DEX signature field.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];

    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for block in message.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Adler-32 (RFC 1950) with zlib's `NMAX` chunking so the accumulators cannot
/// overflow a `u32`.
fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    const NMAX: usize = 5552;
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(NMAX) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether the header signature/checksum of `bytes` verifies.
    fn header_is_valid(bytes: &[u8]) -> bool {
        bytes.len() > 32
            && bytes[12..32] == sha1(&bytes[32..])
            && u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]])
                == adler32(&bytes[12..])
    }

    #[test]
    fn sha1_matches_known_vectors() {
        assert_eq!(
            sha1(b"")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            sha1(b"abc")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        // Longer than one 64-byte block, exercising the message schedule.
        assert_eq!(
            sha1(&vec![b'a'; 1000])
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "291e9a6c66994949b57ba5e650361e98fc36b1ba"
        );
    }

    #[test]
    fn adler32_matches_known_vectors() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"abc"), 0x024d0127);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn version_rewrite_keeps_the_header_verifiable() {
        let mut dex = b"dex\n039\0".to_vec();
        dex.extend_from_slice(&[0u8; 120]);
        assert!(set_dex_version(&mut dex, 38).unwrap());
        assert_eq!(&dex[4..8], b"038\0");
        assert!(header_is_valid(&dex));
        // Second call is a no-op.
        assert!(!set_dex_version(&mut dex, 38).unwrap());
    }

    #[test]
    fn version_rewrite_rejects_non_dex() {
        let mut junk = vec![0u8; 64];
        assert!(set_dex_version(&mut junk, 38).is_err());
    }
}
