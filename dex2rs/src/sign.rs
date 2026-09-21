//! APK signing — dcc.py's `sign()`, still driving `apksigner.jar` with the same
//! keystore and scheme rules.
//!
//! Signing is best-effort, exactly as in dcc.py: with no keystore, no
//! `apksigner.jar` or no `java`, the unsigned APK is copied to the output path and
//! the run says why.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::config::{DEFAULT_KEYSTORE, SignatureConfig};

/// A `tools/apksigner.jar` next to the executable, for a hand-assembled install.
const ADJACENT_JAR: &str = "tools/apksigner.jar";

/// The copy inside this crate — the same bytes that are embedded in the binary.
const CRATE_JAR: &str = "assets/apksigner.jar";

/// The `apksigner.jar` to run: `DEX2RS_TOOLS`, then one next to the executable,
/// then the one in this crate (a checkout, or the unpacked registry source), then
/// the copy embedded in this binary.
///
/// The last one is what makes `cargo install dex2rs` able to sign at all: there is
/// no checkout to look in there, and a tool that silently produces unsigned APKs
/// is worse than one that explains it.
fn apksigner_jar() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("DEX2RS_TOOLS") {
        return Ok(PathBuf::from(dir).join("apksigner.jar"));
    }
    if let Ok(exe) = std::env::current_exe() {
        let candidate = exe
            .parent()
            .map(|dir| dir.join(ADJACENT_JAR))
            .filter(|jar| jar.exists());
        if let Some(jar) = candidate {
            return Ok(jar);
        }
    }
    let in_crate = Path::new(env!("CARGO_MANIFEST_DIR")).join(CRATE_JAR);
    if in_crate.exists() {
        return Ok(in_crate);
    }
    extract_embedded("apksigner.jar", include_bytes!("../assets/apksigner.jar"))
}

/// The keystore to sign with, and whether it is the embedded debug key.
///
/// A configured path is used as-is whenever it exists. When it is exactly the
/// default — `keystore/debug.keystore`, the debug key dcc.py ships too — and that
/// file is not here, the same key embedded in this binary is used instead, so an
/// installed `dex2rs` run outside a checkout still produces an installable APK.
/// Any *other* missing path stays missing: guessing there would sign with the
/// wrong key rather than reporting the typo.
fn keystore(signature: &SignatureConfig) -> Result<(PathBuf, bool)> {
    let configured = Path::new(&signature.keystore_path);
    if configured.is_file() {
        return Ok((configured.to_path_buf(), false));
    }
    if signature.keystore_path != DEFAULT_KEYSTORE {
        return Ok((configured.to_path_buf(), false));
    }
    let key = extract_embedded("debug.keystore", include_bytes!("../assets/debug.keystore"))?;
    Ok((key, true))
}

/// Write an embedded asset to the temporary directory and return its path.
///
/// The file name carries the byte count, so a copy left by an earlier build is
/// replaced instead of reused. Hashing would buy nothing here: this is our own
/// file, in the directory the platform says is for temporary files.
fn extract_embedded(file_name: &str, bytes: &[u8]) -> Result<PathBuf> {
    let (stem, extension) = file_name.rsplit_once('.').unwrap_or((file_name, "bin"));
    let path = std::env::temp_dir().join(format!("dex2rs-{stem}-{}.{extension}", bytes.len()));
    if std::fs::metadata(&path).map(|meta| meta.len()).ok() != Some(bytes.len() as u64) {
        std::fs::write(&path, bytes)
            .with_context(|| format!("writing the embedded {file_name} to {}", path.display()))?;
    }
    Ok(path)
}

/// Sign `unsigned_apk` into `signed_apk`; falls back to a plain copy whenever
/// signing is impossible, exactly like dcc.py.
pub fn sign(
    unsigned_apk: &Path,
    signed_apk: &Path,
    min_sdk: u32,
    signature: &SignatureConfig,
) -> Result<()> {
    let jar = apksigner_jar()?;
    let v1_enabled = signature.v1_enabled && min_sdk < 24;
    let v2_enabled = signature.v2_enabled;
    let v3_enabled = signature.v3_enabled;

    if !v1_enabled && !v2_enabled && !v3_enabled {
        eprintln!(" At least one signing scheme should be enabled from v1, v2 & v3");
        return copy(unsigned_apk, signed_apk);
    }
    let (keystore, embedded_key) = keystore(signature)?;
    if !keystore.is_file() {
        eprintln!(" KeyStore not found in defined path or not recognized as a file");
        return copy(unsigned_apk, signed_apk);
    }
    if embedded_key {
        eprintln!(
            " {} is not here, so signing with the embedded Android debug keystore \
             (set \"keystore_path\" in dcc.cfg for your own)",
            signature.keystore_path
        );
    }
    if !jar.exists() {
        eprintln!(
            " apksigner.jar not found at {}; leaving the APK unsigned",
            jar.display()
        );
        return copy(unsigned_apk, signed_apk);
    }

    eprintln!(
        " Signing {} -> {}",
        unsigned_apk.display(),
        signed_apk.display()
    );
    let status = Command::new("java")
        .arg("-jar")
        .arg(&jar)
        .args(["sign", "--in"])
        .arg(unsigned_apk)
        .arg("--out")
        .arg(signed_apk)
        .args(["--ks", &keystore.to_string_lossy()])
        .args(["--ks-key-alias", &signature.alias])
        .args(["--ks-pass", &format!("pass:{}", signature.keystore_pass)])
        .args(["--key-pass", &format!("pass:{}", signature.store_pass)])
        .args(["--v1-signing-enabled", bool_str(v1_enabled)])
        .args(["--v2-signing-enabled", bool_str(v2_enabled)])
        .args(["--v3-signing-enabled", bool_str(v3_enabled)])
        .args(["--v4-signing-enabled", "false", "--verity-enabled", "false"])
        .status()
        .context("running apksigner (is java installed?)")?;

    if !status.success() {
        eprintln!(
            " Signing {} failed! Copying the unsigned apk instead",
            unsigned_apk.display()
        );
        return copy(unsigned_apk, signed_apk);
    }
    Ok(())
}

fn bool_str(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn copy(unsigned_apk: &Path, signed_apk: &Path) -> Result<()> {
    eprintln!(" Moving unsigned apk -> {}", signed_apk.display());
    std::fs::copy(unsigned_apk, signed_apk).with_context(|| {
        format!(
            "copying {} to {}",
            unsigned_apk.display(),
            signed_apk.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default keystore path is the one case that resolves to the embedded key,
    /// which is why the repository does not ship a copy of it: a debug-signed APK
    /// needs no setup, and a key dropped at that path still wins.
    #[test]
    fn the_default_keystore_path_resolves_to_the_embedded_key() {
        let signature = SignatureConfig {
            keystore_path: DEFAULT_KEYSTORE.to_string(),
            ..SignatureConfig::default()
        };
        let (path, embedded) = keystore(&signature).unwrap();
        if Path::new(DEFAULT_KEYSTORE).is_file() {
            assert_eq!(path, PathBuf::from(DEFAULT_KEYSTORE));
            assert!(!embedded, "a local copy must win over the embedded key");
        } else {
            assert!(embedded, "with no local copy the embedded key is used");
            assert!(path.is_file(), "{path:?} should be the extracted keystore");
        }
    }

    /// The file an embedded asset is written to is named for its length, which is
    /// also what decides whether an older copy is replaced.
    #[test]
    fn an_embedded_asset_is_written_under_its_length() {
        let path = extract_embedded("test-asset.bin", b"hello").unwrap();
        assert!(path.to_string_lossy().ends_with("dex2rs-test-asset-5.bin"));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        std::fs::remove_file(&path).ok();
    }

    /// Any *other* missing path stays missing: guessing there would sign with the
    /// wrong key rather than reporting the typo.
    #[test]
    fn a_mistyped_keystore_is_reported_not_substituted() {
        let signature = SignatureConfig {
            keystore_path: "keystore/not-the-default.keystore".to_string(),
            ..SignatureConfig::default()
        };
        let (path, embedded) = keystore(&signature).unwrap();
        assert_eq!(path, PathBuf::from("keystore/not-the-default.keystore"));
        assert!(
            !embedded,
            "a mistyped keystore must be reported, not substituted"
        );
    }
}
