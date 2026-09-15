//! Which Harness release the managed runtime tracks.
//!
//! The build pins one known-good release (`install::VERSION`): every patch
//! the installer applies is verified against exactly that source. The channel
//! file lets the user aim the installer at another release instead. Registry
//! lookups answer what exists; preflight answers whether Studio can live with
//! it; this module only records the choice.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const REGISTRY_DOCUMENT: &str = "https://registry.npmjs.org/@deepseek-ai/dsh";
const REGISTRY_BUDGET: Duration = Duration::from_secs(15);

/// The user's release choice plus the registry facts gathered so far.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    /// Registry version the user pinned, or none for the built-in release.
    #[serde(default)]
    pub pinned: Option<String>,
    /// Every version the registry advertised at the last refresh, newest first.
    #[serde(default)]
    pub known: Vec<String>,
    /// `dist-tags.latest` at the last refresh.
    #[serde(default)]
    pub latest: Option<String>,
}

fn channel_file() -> std::path::PathBuf {
    crate::paths::app_data_dir().join("dsh-channel.json")
}

/// Read the recorded channel, tolerating a missing or stale file.
pub fn load() -> Channel {
    crate::bounded_file::read_string(&channel_file(), crate::bounded_file::CONTROL_BYTES)
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default()
}

fn store(channel: &Channel) -> Result<()> {
    let body = serde_json::to_vec_pretty(channel)
        .map_err(|cause| Error::Install(format!("could not encode the channel choice: {cause}")))?;
    crate::atomic::write(&channel_file(), body)
        .map_err(|cause| Error::Install(format!("could not record the channel choice: {cause}")))
}

/// Pin a registry version, or return to the built-in release with `None`.
pub fn pin(version: Option<String>) -> Result<Channel> {
    let mut channel = load();
    channel.pinned = version;
    store(&channel)?;
    Ok(channel)
}

/// The release the installer and the runtime contract should expect.
pub fn selected() -> String {
    load()
        .pinned
        .unwrap_or_else(|| super::install::VERSION.to_string())
}

/// Refresh the registry facts: every published version and `dist-tags.latest`.
pub async fn refresh(node: &Path) -> Result<Channel> {
    let document = crate::fetch::json(node, REGISTRY_DOCUMENT, REGISTRY_BUDGET).await?;
    let mut known = document
        .get("versions")
        .and_then(|versions| versions.as_object())
        .map(|versions| versions.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    known.sort_by(|a, b| {
        match (semver::Version::parse(a), semver::Version::parse(b)) {
            (Ok(a), Ok(b)) => b.cmp(&a),
            _ => b.cmp(a),
        }
    });
    let latest = document
        .get("dist-tags")
        .and_then(|tags| tags.get("latest"))
        .and_then(|latest| latest.as_str())
        .map(str::to_string);

    let mut channel = load();
    channel.known = known;
    channel.latest = latest;
    store(&channel)?;
    Ok(channel)
}

/// The registry release worth telling the user about, when it is newer than
/// everything Studio has seen or installed.
pub fn update_available(channel: &Channel, installed: Option<&str>) -> Option<String> {
    let latest = channel.latest.clone()?;
    let current = installed
        .map(str::to_string)
        .unwrap_or_else(|| super::install::VERSION.to_string());
    let newer = match (
        semver::Version::parse(&latest),
        semver::Version::parse(&current),
    ) {
        (Ok(latest), Ok(current)) => latest > current,
        _ => latest != current,
    };
    newer.then_some(latest)
}

#[cfg(test)]
mod tests {
    use super::{update_available, Channel};

    #[test]
    fn an_update_is_newer_than_the_install() {
        let channel = Channel {
            pinned: None,
            known: vec![],
            latest: Some("0.1.5-rc.1".into()),
        };
        assert_eq!(
            update_available(&channel, Some("0.1.2-rc.1")),
            Some("0.1.5-rc.1".to_string())
        );
        assert_eq!(update_available(&channel, Some("0.1.5-rc.1")), None);
        assert!(update_available(&channel, None).is_some());
    }

    #[test]
    fn no_latest_means_no_update() {
        let channel = Channel::default();
        assert_eq!(update_available(&channel, Some("0.1.2-rc.1")), None);
    }
}
