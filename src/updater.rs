//! Self-update support backed by GitHub Release assets.
//!
//! Every release publishes `npu-update.json` plus one uncompressed executable
//! per supported platform. The manifest is the stable contract between old
//! binaries and future releases: it identifies the release, maps the running
//! platform to an asset, and authenticates the downloaded bytes with SHA-256.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Version embedded by Cargo when this binary is compiled.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const MANIFEST_URL: &str =
    "https://github.com/fmatsos/npu/releases/latest/download/npu-update.json";
const RELEASE_DOWNLOAD_BASE: &str = "https://github.com/fmatsos/npu/releases/download";
const MANIFEST_LIMIT: u64 = 1024 * 1024;
const BINARY_LIMIT: u64 = 100 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const MANIFEST_SCHEMA_VERSION: u64 = 1;

#[derive(Debug, Deserialize)]
struct Manifest {
    schema_version: u64,
    version: String,
    assets: BTreeMap<String, Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    sha256: String,
}

/// Result printed by `npu update` on stdout.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The running binary is the same version as, or newer than, the release.
    UpToDate { current: Version, latest: Version },
    /// A newer release replaced the running executable.
    Updated { previous: Version, current: Version },
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::UpToDate { current, latest } if current == latest => {
                write!(f, "npu {current} is already up to date")
            }
            Outcome::UpToDate { current, latest } => {
                write!(f, "npu {current} is newer than latest release {latest}")
            }
            Outcome::Updated { previous, current } => {
                write!(f, "updated npu from {previous} to {current}")
            }
        }
    }
}

/// Checks GitHub's latest release and replaces the running executable when
/// its manifest advertises a newer version for this platform.
pub fn update() -> crate::Result<Outcome> {
    let platform = current_platform()?;
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .build()
        .new_agent();

    update_with(
        VERSION,
        &platform,
        |url, limit| download(&agent, url, limit),
        replace_executable,
    )
}

fn update_with(
    current_version: &str,
    platform: &str,
    fetch: impl Fn(&str, u64) -> crate::Result<Vec<u8>>,
    replace: impl Fn(&[u8]) -> crate::Result<()>,
) -> crate::Result<Outcome> {
    let current = parse_version(current_version, "current binary")?;
    let manifest_bytes = fetch(MANIFEST_URL, MANIFEST_LIMIT)?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes).map_err(|err| {
        crate::Error::Update(format!(
            "invalid release manifest from {MANIFEST_URL}: {err}"
        ))
    })?;

    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(crate::Error::Update(format!(
            "unsupported release manifest schema {} (this binary supports {})",
            manifest.schema_version, MANIFEST_SCHEMA_VERSION
        )));
    }

    let latest = parse_version(&manifest.version, "release manifest")?;
    if latest <= current {
        return Ok(Outcome::UpToDate { current, latest });
    }

    let asset = manifest.assets.get(platform).ok_or_else(|| {
        crate::Error::Update(format!(
            "release {latest} has no binary for platform {platform}"
        ))
    })?;
    validate_asset(asset)?;

    let url = format!("{RELEASE_DOWNLOAD_BASE}/v{latest}/{}", asset.name);
    let binary = fetch(&url, BINARY_LIMIT)?;
    verify_sha256(&binary, &asset.sha256)?;
    replace(&binary)?;

    Ok(Outcome::Updated {
        previous: current,
        current: latest,
    })
}

fn parse_version(value: &str, source: &str) -> crate::Result<Version> {
    Version::parse(value).map_err(|err| {
        crate::Error::Update(format!("invalid version {value:?} in {source}: {err}"))
    })
}

fn validate_asset(asset: &Asset) -> crate::Result<()> {
    if !asset.name.starts_with("npu-")
        || asset.name.contains('/')
        || asset.name.contains('\\')
        || asset.name.contains("..")
    {
        return Err(crate::Error::Update(format!(
            "unsafe asset name in release manifest: {:?}",
            asset.name
        )));
    }

    if asset.sha256.len() != 64 || !asset.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(crate::Error::Update(format!(
            "invalid SHA-256 for release asset {:?}",
            asset.name
        )));
    }

    Ok(())
}

fn verify_sha256(bytes: &[u8], expected: &str) -> crate::Result<()> {
    let actual = sha256_hex(bytes);
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(crate::Error::Update(format!(
            "downloaded binary failed SHA-256 verification (expected {expected}, got {actual})"
        )))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn download(agent: &ureq::Agent, url: &str, limit: u64) -> crate::Result<Vec<u8>> {
    let mut response = agent
        .get(url)
        .header("User-Agent", concat!("npu/", env!("CARGO_PKG_VERSION")))
        .call()
        .map_err(|err| crate::Error::Update(format!("downloading {url} failed: {err}")))?;

    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|err| crate::Error::Update(format!("reading {url} failed: {err}")))
}

fn replace_executable(bytes: &[u8]) -> crate::Result<()> {
    let path = temporary_download_path()?;
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        self_replace::self_replace(&path)?;
        Ok::<(), std::io::Error>(())
    })();

    let cleanup_result = std::fs::remove_file(&path);
    if let Err(err) = result {
        return Err(crate::Error::Update(format!(
            "replacing the current executable failed: {err}"
        )));
    }
    if let Err(err) = cleanup_result
        && err.kind() != std::io::ErrorKind::NotFound
    {
        return Err(crate::Error::Update(format!(
            "the executable was updated, but the temporary file {} could not be removed: {err}",
            path.display()
        )));
    }
    Ok(())
}

fn temporary_download_path() -> crate::Result<PathBuf> {
    let executable = std::env::current_exe().map_err(crate::Error::Io)?;
    let parent = executable.parent().ok_or_else(|| {
        crate::Error::Update("the current executable has no parent directory".to_string())
    })?;
    let file_name = executable.file_name().ok_or_else(|| {
        crate::Error::Update("the current executable has no file name".to_string())
    })?;

    for attempt in 0..100_u8 {
        let mut candidate = file_name.to_os_string();
        candidate.push(format!(".update-{}-{attempt}", std::process::id()));
        let path = parent.join(candidate);
        if !path.exists() {
            return Ok(path);
        }
    }

    Err(crate::Error::Update(format!(
        "cannot allocate a temporary update file next to {}",
        executable.display()
    )))
}

fn current_platform() -> crate::Result<String> {
    let os_release = if std::env::consts::OS == "linux" {
        std::fs::read_to_string("/etc/os-release").ok()
    } else {
        None
    };
    platform_for(
        std::env::consts::OS,
        std::env::consts::ARCH,
        os_release.as_deref(),
    )
}

fn platform_for(os: &str, arch: &str, os_release: Option<&str>) -> crate::Result<String> {
    let platform = match (os, arch) {
        ("linux", "x86_64") => {
            match linux_distribution(os_release.unwrap_or_default()).as_deref() {
                Some("fedora") => "x86_64-fedora",
                Some("arch") => "x86_64-arch",
                _ => "x86_64-unknown-linux-gnu",
            }
        }
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        _ => {
            return Err(crate::Error::Update(format!(
                "unsupported platform {arch}-{os}"
            )));
        }
    };
    Ok(platform.to_string())
}

fn linux_distribution(os_release: &str) -> Option<String> {
    for key in ["ID", "ID_LIKE"] {
        let Some(value) = os_release.lines().find_map(|line| {
            line.trim()
                .strip_prefix(&format!("{key}="))
                .map(|value| value.trim_matches(['\'', '"']))
        }) else {
            continue;
        };
        for id in value.split_whitespace() {
            if id == "fedora" || id == "arch" {
                return Some(id.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn update_downloads_verifies_and_replaces_the_platform_asset() {
        let binary = b"new npu binary";
        let manifest = format!(
            r#"{{"schema_version":1,"version":"0.2.0","assets":{{"x86_64-unknown-linux-gnu":{{"name":"npu-x86_64-unknown-linux-gnu","sha256":"{}"}}}}}}"#,
            sha256_hex(binary)
        );
        let requested = RefCell::new(Vec::new());
        let replaced = RefCell::new(Vec::new());

        let outcome = update_with(
            "0.1.0",
            "x86_64-unknown-linux-gnu",
            |url, limit| {
                requested.borrow_mut().push((url.to_string(), limit));
                if url == MANIFEST_URL {
                    Ok(manifest.as_bytes().to_vec())
                } else {
                    Ok(binary.to_vec())
                }
            },
            |bytes| {
                replaced.borrow_mut().extend_from_slice(bytes);
                Ok(())
            },
        )
        .expect("the update should succeed");

        assert_eq!(
            outcome,
            Outcome::Updated {
                previous: Version::parse("0.1.0").expect("valid version"),
                current: Version::parse("0.2.0").expect("valid version"),
            }
        );
        assert_eq!(replaced.into_inner(), binary);
        assert_eq!(
            requested.borrow()[0],
            (MANIFEST_URL.to_string(), MANIFEST_LIMIT)
        );
        assert_eq!(
            requested.borrow()[1],
            (
                "https://github.com/fmatsos/npu/releases/download/v0.2.0/npu-x86_64-unknown-linux-gnu"
                    .to_string(),
                BINARY_LIMIT,
            )
        );
    }

    #[test]
    fn current_version_does_not_download_or_replace_a_binary() {
        let manifest = br#"{"schema_version":1,"version":"0.1.0","assets":{}}"#;
        let calls = RefCell::new(0);

        let outcome = update_with(
            "0.1.0",
            "irrelevant",
            |_, _| {
                *calls.borrow_mut() += 1;
                Ok(manifest.to_vec())
            },
            |_| Err(crate::Error::Update("must not replace".to_string())),
        )
        .expect("an up-to-date binary should succeed");

        assert_eq!(outcome.to_string(), "npu 0.1.0 is already up to date");
        assert_eq!(calls.into_inner(), 1);
    }

    #[test]
    fn checksum_mismatch_is_rejected_before_replacement() {
        let manifest = br#"{"schema_version":1,"version":"0.2.0","assets":{"x86_64-unknown-linux-gnu":{"name":"npu-x86_64-unknown-linux-gnu","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}}}"#;
        let replaced = RefCell::new(false);

        let err = update_with(
            "0.1.0",
            "x86_64-unknown-linux-gnu",
            |url, _| {
                if url == MANIFEST_URL {
                    Ok(manifest.to_vec())
                } else {
                    Ok(b"corrupt".to_vec())
                }
            },
            |_| {
                *replaced.borrow_mut() = true;
                Ok(())
            },
        )
        .expect_err("a checksum mismatch must fail");

        assert!(err.to_string().contains("SHA-256 verification"));
        assert!(!replaced.into_inner());
    }

    #[test]
    fn platform_mapping_selects_distro_specific_linux_builds() {
        assert_eq!(
            platform_for("linux", "x86_64", Some("ID=fedora\n")).expect("supported"),
            "x86_64-fedora"
        );
        assert_eq!(
            platform_for(
                "linux",
                "x86_64",
                Some("ID=nobara\nID_LIKE=\"fedora rhel\"\n")
            )
            .expect("supported"),
            "x86_64-fedora"
        );
        assert_eq!(
            platform_for("linux", "x86_64", Some("ID=arch\n")).expect("supported"),
            "x86_64-arch"
        );
        assert_eq!(
            platform_for("linux", "x86_64", Some("ID=ubuntu\n")).expect("supported"),
            "x86_64-unknown-linux-gnu"
        );
    }

    #[test]
    fn unsafe_asset_name_is_rejected() {
        let asset = Asset {
            name: "../../npu".to_string(),
            sha256: "0".repeat(64),
        };
        let err = validate_asset(&asset).expect_err("path traversal must be rejected");
        assert!(err.to_string().contains("unsafe asset name"));
    }
}
