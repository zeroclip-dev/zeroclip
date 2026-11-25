//! Self-update from GitHub Releases.
//!
//! The pure parts (version compare, asset pick, `SHA256SUMS.txt` parsing, hash check, "can we
//! install now") are plain functions with unit tests. The I/O parts shell out to the `curl.exe`
//! that ships with Windows 10 1803+, so the app needs no HTTP client crate. All of it runs on a
//! background thread; the UI only ever sees the result.
//!
//! The SHA-256 check catches a corrupt or truncated download. It does NOT prove who published
//! the file: whoever can publish a release can publish a matching hash. Real protection needs
//! code signing or a signature verified against a key built into the app.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, bail};
use semver::Version;
use sha2::{Digest as _, Sha256};
use zc_core::control::EngineStatus;

/// The newest published (non-draft, non-prerelease) release.
const LATEST_URL: &str = "https://api.github.com/repos/zeroclip-dev/zeroclip/releases/latest";
/// Download URLs must start with this; anything else in the API response is ignored.
const DOWNLOAD_PREFIX: &str = "https://github.com/zeroclip-dev/zeroclip/releases/download/";
/// The checksum file attached to every release.
const SUMS_NAME: &str = "SHA256SUMS.txt";
/// Passed to the new copy so it waits for this one to release the single-instance mutex.
pub const AFTER_UPDATE_FLAG: &str = "--after-update";

/// A release newer than the running app, with the two files needed to install it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub exe_name: String,
    pub exe_url: String,
    pub sums_url: String,
}

/// Where an update is in its life. Held by the app; drawn by `ui::update_banner`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UpdateState {
    #[default]
    None,
    Available(Release),
    Downloading(Release),
    /// Downloaded and verified; waiting for a safe moment (not recording or saving) and a click.
    Ready {
        release: Release,
        path: PathBuf,
    },
}

/// The version of the running app.
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0))
}

/// `v1.2.3` or `1.2.3` to a version.
pub fn parse_tag(tag: &str) -> anyhow::Result<Version> {
    let text = tag.strip_prefix('v').unwrap_or(tag);
    Version::parse(text).with_context(|| format!("unrecognised release tag {tag:?}"))
}

/// The exact asset name a release of `version` carries for the bare exe.
pub fn exe_asset_name(version: &Version) -> String {
    format!("ZeroClip-v{version}-windows-x64.exe")
}

/// Finds the exe and checksum downloads among `(name, url)` pairs. The exe must be named exactly
/// for `version`, and both URLs must point at this repo's release downloads.
pub fn pick_assets(version: &Version, assets: &[(String, String)]) -> Option<Release> {
    let find = |wanted: &str| {
        assets
            .iter()
            .find(|(name, url)| name == wanted && url.starts_with(DOWNLOAD_PREFIX))
            .map(|(_, url)| url.clone())
    };
    let exe_name = exe_asset_name(version);
    Some(Release {
        version: version.clone(),
        exe_url: find(&exe_name)?,
        sums_url: find(SUMS_NAME)?,
        exe_name,
    })
}

/// Reads the `releases/latest` JSON. Fails when the tag or the assets are not usable.
pub fn parse_release(json: &str) -> anyhow::Result<Release> {
    let value: serde_json::Value = serde_json::from_str(json).context("bad release JSON")?;
    let tag = value["tag_name"].as_str().context("release has no tag")?;
    let version = parse_tag(tag)?;
    let assets: Vec<(String, String)> = value["assets"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|a| {
                    Some((
                        a["name"].as_str()?.to_owned(),
                        a["browser_download_url"].as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    pick_assets(&version, &assets).with_context(|| format!("release {tag} has no usable assets"))
}

/// True when `latest` is a higher version than `current`.
pub fn is_newer(current: &Version, latest: &Version) -> bool {
    latest > current
}

/// Parses `sha256sum` output: `<64 hex>  <name>` per line (`*` before the name marks binary mode).
pub fn parse_sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (hash, name) = line.trim().split_once(char::is_whitespace)?;
            let name = name.trim().trim_start_matches('*');
            let valid = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
            (valid && !name.is_empty()).then(|| (hash.to_ascii_lowercase(), name.to_owned()))
        })
        .collect()
}

/// Checks `actual_hex` against the entry for `name` in a `SHA256SUMS.txt` body.
pub fn verify_hash(sums: &str, name: &str, actual_hex: &str) -> anyhow::Result<()> {
    let entries = parse_sums(sums);
    let Some((expected, _)) = entries.iter().find(|(_, n)| n == name) else {
        bail!("{SUMS_NAME} has no entry for {name}");
    };
    if !expected.eq_ignore_ascii_case(actual_hex) {
        bail!("the download of {name} does not match its published SHA-256, so it was discarded");
    }
    Ok(())
}

/// Whether an update may replace the running app now. `Err` carries the reason for the user.
pub fn can_install(status: &EngineStatus, saving_clip: bool) -> Result<(), &'static str> {
    if status.recording {
        Err("the recording has finished")
    } else if saving_clip {
        Err("the clip has finished saving")
    } else {
        Ok(())
    }
}

/// SHA-256 of a file as lowercase hex, read in chunks.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// `<exe>.old` / `<exe>.new` next to the running exe.
fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Runs the system `curl.exe` (by full path, never via `PATH`) and returns stdout. `out` writes
/// the body to a file instead.
fn curl(url: &str, out: Option<&Path>, max_secs: u32, max_bytes: u64) -> anyhow::Result<Vec<u8>> {
    use std::os::windows::process::CommandExt as _;
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let exe = Path::new(&root).join("System32").join("curl.exe");
    let mut cmd = Command::new(exe);
    cmd.args(["--fail", "--location", "--silent", "--show-error"])
        .args(["--proto", "=https", "--proto-redir", "=https"])
        .args(["--max-time", &max_secs.to_string()])
        .args(["--max-filesize", &max_bytes.to_string()])
        .args(["--user-agent", &format!("ZeroClip/{}", env!("CARGO_PKG_VERSION"))])
        .args(["--header", "Accept: application/vnd.github+json"]);
    if let Some(out) = out {
        cmd.arg("--output").arg(out);
    }
    cmd.arg(url).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // CREATE_NO_WINDOW: the release build is a GUI app, so a console child would flash a window.
    // https://learn.microsoft.com/windows/win32/procthread/process-creation-flags
    cmd.creation_flags(0x0800_0000);
    let output = cmd.output().context("couldn't run curl.exe (needs Windows 10 1803 or later)")?;
    if !output.status.success() {
        bail!("download failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(output.stdout)
}

/// Asks GitHub for the latest release. `None` when the running app is up to date.
pub fn check_latest() -> anyhow::Result<Option<Release>> {
    let body = curl(LATEST_URL, None, 20, 4 << 20)?;
    let release = parse_release(&String::from_utf8_lossy(&body))?;
    Ok(is_newer(&current_version(), &release.version).then_some(release))
}

/// Downloads the new exe beside the running one and checks it against `SHA256SUMS.txt`.
/// Returns the staged file. A file that fails the check is deleted.
pub fn download(release: &Release) -> anyhow::Result<PathBuf> {
    let staged = sibling(&std::env::current_exe()?, ".new");
    let sums = curl(&release.sums_url, None, 20, 1 << 20)?;
    let result = curl(&release.exe_url, Some(&staged), 600, 512 << 20)
        .and_then(|_| Ok(sha256_file(&staged)?))
        .and_then(|hash| verify_hash(&String::from_utf8_lossy(&sums), &release.exe_name, &hash));
    if let Err(error) = result {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    Ok(staged)
}

/// Swaps the staged exe in and starts it. Windows lets a running exe be renamed but not
/// overwritten, so: running exe -> `.old`, staged -> exe, spawn. The caller then quits; the new
/// copy retries the single-instance mutex until this one has gone. On failure the old exe is put back.
pub fn swap_and_restart(staged: &Path) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let old = sibling(&exe, ".old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).context("couldn't move the running exe aside")?;
    if let Err(error) = std::fs::rename(staged, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(error).context("couldn't move the new exe into place");
    }
    if let Err(error) = Command::new(&exe).arg(AFTER_UPDATE_FLAG).spawn() {
        let _ = std::fs::rename(&exe, staged);
        let _ = std::fs::rename(&old, &exe);
        return Err(error).context("couldn't start the new version");
    }
    Ok(())
}

/// Deletes the previous exe and any half-finished download. Call once the instance guard is held,
/// so the old process is gone.
pub fn cleanup_leftovers() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(sibling(&exe, ".old"));
        let _ = std::fs::remove_file(sibling(&exe, ".new"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }
    fn url(name: &str) -> String {
        format!("{DOWNLOAD_PREFIX}v0.2.0/{name}")
    }

    #[test]
    fn newer_means_strictly_higher() {
        assert!(is_newer(&v("0.1.0"), &v("0.1.1")));
        assert!(is_newer(&v("0.9.0"), &v("0.10.0")));
        assert!(!is_newer(&v("0.2.0"), &v("0.2.0")));
        assert!(!is_newer(&v("0.2.0"), &v("0.1.9")));
        assert!(!is_newer(&v("1.0.0"), &v("1.0.0-rc.1")));
    }

    #[test]
    fn tags_with_and_without_v() {
        assert_eq!(parse_tag("v0.2.0").unwrap(), v("0.2.0"));
        assert_eq!(parse_tag("0.2.0").unwrap(), v("0.2.0"));
        assert!(parse_tag("nightly").is_err());
    }

    #[test]
    fn picks_the_exact_exe_and_the_sums() {
        let assets = vec![
            ("ZeroClip-v0.2.0-windows-x64.zip".to_owned(), url("ZeroClip-v0.2.0-windows-x64.zip")),
            ("ZeroClip-v0.2.0-windows-x64.exe".to_owned(), url("ZeroClip-v0.2.0-windows-x64.exe")),
            ("SHA256SUMS.txt".to_owned(), url("SHA256SUMS.txt")),
        ];
        let release = pick_assets(&v("0.2.0"), &assets).unwrap();
        assert_eq!(release.exe_name, "ZeroClip-v0.2.0-windows-x64.exe");
        assert!(release.exe_url.ends_with(".exe"));
        assert!(release.sums_url.ends_with("SHA256SUMS.txt"));
        // No exe for this version, or a URL outside the repo: nothing usable.
        assert!(pick_assets(&v("0.3.0"), &assets).is_none());
        let foreign = vec![
            ("ZeroClip-v0.2.0-windows-x64.exe".to_owned(), "https://evil.example/a.exe".to_owned()),
            ("SHA256SUMS.txt".to_owned(), url("SHA256SUMS.txt")),
        ];
        assert!(pick_assets(&v("0.2.0"), &foreign).is_none());
    }

    #[test]
    fn parses_a_release_document() {
        let json = format!(
            r#"{{"tag_name":"v0.2.0","assets":[
                {{"name":"ZeroClip-v0.2.0-windows-x64.exe","browser_download_url":"{}"}},
                {{"name":"SHA256SUMS.txt","browser_download_url":"{}"}}]}}"#,
            url("ZeroClip-v0.2.0-windows-x64.exe"),
            url("SHA256SUMS.txt")
        );
        assert_eq!(parse_release(&json).unwrap().version, v("0.2.0"));
        assert!(parse_release(r#"{"tag_name":"v0.2.0","assets":[]}"#).is_err());
        assert!(parse_release("not json").is_err());
    }

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn parses_sha256sum_lines() {
        let text =
            format!("{HASH_A}  one.exe\r\n{}  *two.zip\nnot a hash line\n", HASH_B.to_uppercase());
        let entries = parse_sums(&text);
        assert_eq!(
            entries,
            vec![
                (HASH_A.to_owned(), "one.exe".to_owned()),
                (HASH_B.to_owned(), "two.zip".to_owned())
            ]
        );
    }

    #[test]
    fn hash_check_accepts_match_and_refuses_the_rest() {
        let sums = format!("{HASH_A}  one.exe\n{HASH_B}  two.zip\n");
        assert!(verify_hash(&sums, "one.exe", HASH_A).is_ok());
        assert!(verify_hash(&sums, "one.exe", &HASH_A.to_uppercase()).is_ok());
        assert!(verify_hash(&sums, "one.exe", HASH_B).is_err());
        assert!(verify_hash(&sums, "missing.exe", HASH_A).is_err());
    }

    #[test]
    fn hashes_a_file() {
        let path = std::env::temp_dir().join(format!("zeroclip-sha-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        let hash = sha256_file(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(hash, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn never_installs_while_recording_or_saving() {
        let idle = EngineStatus::default();
        assert!(can_install(&idle, false).is_ok());
        assert!(can_install(&idle, true).is_err());
        let recording = EngineStatus { recording: true, ..EngineStatus::default() };
        assert!(can_install(&recording, false).is_err());
    }
}
