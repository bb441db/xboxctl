//! The Xbox Accessories app's firmware feed: a JSON index on Microsoft's CDN
//! that names the current firmware file for each controller model.

use crate::gip::rdhm::Package;
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// PC feed; redirects to the current JSON index. The Xbox console feed is
/// `https://aka.ms/xboxaccessoriesfwcdnxbga`.
pub const FEED_URL: &str = "https://aka.ms/xboxaccessoriesfwcdnpcga";

#[derive(Debug, Deserialize)]
pub struct Feed {
    /// Feed name, e.g. `pc_ga`.
    #[serde(rename = "g")]
    pub name: String,
    #[serde(rename = "v")]
    pub version: String,
    /// Base URL of the firmware files.
    #[serde(rename = "p")]
    pub base_url: String,
    #[serde(rename = "d")]
    pub devices: Vec<FeedDevice>,
}

#[derive(Debug, Deserialize)]
pub struct FeedDevice {
    #[serde(rename = "__n", default)]
    pub codename: String,
    /// `VID_PID` in upper-case hex, sometimes with a `-cc-XX` suffix.
    pub id: String,
    #[serde(rename = "f", default)]
    pub files: Vec<FeedFile>,
    /// Minimum version the app requires before anything else.
    #[serde(rename = "req", default)]
    pub required: Vec<FeedVersion>,
}

#[derive(Debug, Deserialize)]
pub struct FeedFile {
    #[serde(rename = "__t", default)]
    pub tag: String,
    #[serde(rename = "p")]
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct FeedVersion {
    #[serde(rename = "v")]
    pub version: String,
}

impl Feed {
    pub fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|error| format!("invalid firmware feed: {error}"))
    }

    /// The entry for a USB VID/PID; entries with a `-cc-` suffix are variants
    /// that the plain VID/PID does not identify.
    pub fn device(&self, vendor_id: u16, product_id: u16) -> Option<&FeedDevice> {
        let id = format!("{vendor_id:04X}_{product_id:04X}");
        self.devices
            .iter()
            .find(|device| device.id.eq_ignore_ascii_case(&id))
    }
}

impl FeedFile {
    /// Firmware version from the file name (`..._v_5_23_6_0_h_...`).
    pub fn version(&self) -> Option<String> {
        let start = self.name.find("_v_")? + 3;
        let end = start + self.name[start..].find("_h_")?;
        Some(self.name[start..end].replace('_', "."))
    }

    /// Expected base64url SHA-256 of the file, from its name.
    pub fn expected_hash(&self) -> Option<&str> {
        let start = self.name.find("_h_")? + 3;
        self.name[start..].strip_suffix(".bin")
    }

    /// Check `data` against the hash in the file name.
    pub fn verify(&self, data: &[u8]) -> Result<(), String> {
        let expected = self
            .expected_hash()
            .ok_or_else(|| format!("{} has no hash in its name", self.name))?;
        let actual = base64::engine::general_purpose::URL_SAFE.encode(Sha256::digest(data));
        if actual != expected {
            return Err(format!(
                "{} is corrupt: SHA-256 {actual}, expected {expected}",
                self.name
            ));
        }
        Ok(())
    }
}

pub fn fetch_feed() -> Result<Feed, String> {
    let json = ureq::get(FEED_URL)
        .call()
        .map_err(|error| format!("could not fetch the firmware feed: {error}"))?
        .body_mut()
        .read_to_string()
        .map_err(|error| format!("could not read the firmware feed: {error}"))?;
    Feed::parse(&json)
}

/// Download `file` into `directory` unless a verified copy is already there.
/// Returns the path and the parsed package.
pub fn download(
    feed: &Feed,
    file: &FeedFile,
    directory: &Path,
) -> Result<(PathBuf, Package), String> {
    if file.name.contains(['/', '\\']) || file.name.starts_with('.') {
        return Err(format!("refusing unexpected file name {:?}", file.name));
    }
    let path = directory.join(&file.name);
    let data = match fs::read(&path) {
        Ok(data) if file.verify(&data).is_ok() => data,
        _ => {
            let url = format!("{}{}", feed.base_url, file.name);
            let data = ureq::get(&url)
                .call()
                .map_err(|error| format!("could not download {url}: {error}"))?
                .body_mut()
                .with_config()
                .limit(64 * 1024 * 1024)
                .read_to_vec()
                .map_err(|error| format!("could not download {url}: {error}"))?;
            file.verify(&data)?;
            fs::create_dir_all(directory)
                .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
            fs::write(&path, &data)
                .map_err(|error| format!("could not write {}: {error}", path.display()))?;
            data
        }
    };
    let package = Package::parse(&data).map_err(|error| format!("{}: {error}", file.name))?;
    Ok((path, package))
}

/// Default download directory: the platform's per-user cache.
pub fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                let home = PathBuf::from(home);
                if cfg!(target_os = "macos") {
                    home.join("Library/Caches")
                } else {
                    home.join(".cache")
                }
            })
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("xboxctl").join("firmware")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"g":"pc_ga","v":"1.0.0.23",
        "p":"https://example.invalid/accessories/FW/",
        "d":[{"__n":"merl","id":"045E_0B12",
              "f":[{"__t":"beta","p":"MERL_045E_0B12_v_5_23_6_0_h_jC-y7nRWRtQCHQCyi2rv6xtQpgikM6JG0r6lYE3A5pM=.bin"}],
              "req":[{"v":"5.9.2709.0"}]},
             {"__n":"norl","id":"045E_0B17-cc-72","f":[]}]}"#;

    #[test]
    fn parses_feed_and_file_names() {
        let feed = Feed::parse(SAMPLE).unwrap();
        assert_eq!(feed.name, "pc_ga");
        let device = feed.device(0x045e, 0x0b12).unwrap();
        assert_eq!(device.codename, "merl");
        assert_eq!(device.required[0].version, "5.9.2709.0");
        let file = &device.files[0];
        assert_eq!(file.version().as_deref(), Some("5.23.6.0"));
        assert_eq!(
            file.expected_hash(),
            Some("jC-y7nRWRtQCHQCyi2rv6xtQpgikM6JG0r6lYE3A5pM=")
        );
        assert!(feed.device(0x045e, 0x0b17).is_none());
    }

    #[test]
    fn rejects_corrupt_data() {
        let file = FeedFile {
            tag: String::new(),
            name: "X_v_1_0_0_0_h_AAAA.bin".to_owned(),
        };
        assert!(file.verify(b"anything").is_err());
    }
}
