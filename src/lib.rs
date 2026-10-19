//! Core logic for `tuxcharge-profile`.
//!
//! Maps the currently connected Wi-Fi network (SSID) to a TUXEDO embedded
//! controller charging profile, so the battery charge limit follows you
//! between networks even without TUXEDO Control Center installed.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Charging profile attribute exposed by the `tuxedo_keyboard` driver.
pub const EC_PROFILE_PATH: &str =
    "/sys/devices/platform/tuxedo_keyboard/charging_profile/charging_profile";

/// Default config location.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/tuxcharge-profile/config.toml";

/// Profiles used as a fallback when the driver does not publish its own list.
pub const KNOWN_PROFILES: [&str; 3] = ["high_capacity", "balanced", "stationary"];

/// Profile applied when no rule matches (full charge, no limit).
pub const DEFAULT_PROFILE: &str = "high_capacity";

/// Template written by `tuxcharge-profile install` and shipped as an example.
pub const EXAMPLE_CONFIG: &str = r#"# tuxcharge-profile — charging profile per Wi-Fi network
#
# Profiles written by the TUXEDO driver (Uniwill EC):
#   high_capacity  full charge, full charging speed  (100%)
#   balanced       reduced capacity                  (~90%)
#   stationary     stationary use, slowest charging   (~80%)
#
# Any network without a rule below gets `default`.

default = "high_capacity"

[networks]
# Quotes are required for SSIDs containing spaces or special characters.
"HomeWiFi" = "stationary"
"#;

/// Where the example config lives when installed from a package.
pub const EXAMPLE_CONFIG_PATH: &str = "/usr/share/tuxcharge-profile/config.toml.example";

// =====================================================================
// config
// =====================================================================

/// Mapping of Wi-Fi networks to charging profiles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub default_profile: String,
    pub networks: BTreeMap<String, String>,
}

#[derive(serde::Deserialize)]
struct RawConfig {
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    networks: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            default_profile: DEFAULT_PROFILE.to_string(),
            networks: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| format!("invalid config: {e}"))?;

        let mut config = Config::default();
        if let Some(default) = raw.default {
            config.default_profile = default;
        }
        config.networks = raw.networks;

        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    /// Which profile the given network should use, and why.
    pub fn resolve(&self, ssid: Option<&str>) -> Resolution {
        match ssid {
            Some(ssid) => match self.networks.get(ssid) {
                Some(profile) => Resolution {
                    profile: profile.clone(),
                    reason: format!("rule for \"{ssid}\""),
                },
                None => Resolution {
                    profile: self.default_profile.clone(),
                    reason: format!("default (no rule for \"{ssid}\")"),
                },
            },
            None => Resolution {
                profile: self.default_profile.clone(),
                reason: "default (no Wi-Fi connected)".to_string(),
            },
        }
    }
}

/// Load the config; a missing file is not an error.
///
/// Returns the config and whether the built-in defaults were used, so callers
/// can warn about it. A present-but-invalid config is still an error.
pub fn load_config(path: &Path) -> Result<(Config, bool), String> {
    if !path.exists() {
        return Ok((Config::default(), true));
    }

    Ok((Config::load(path)?, false))
}

/// NetworkManager dispatcher script, run as root on network changes.
pub fn dispatcher_script(bin: &str) -> String {
    format!(
        r#"#!/bin/sh
# Installed by tuxcharge-profile — apply the charging profile for the new network.
# NetworkManager runs this as root on every network change.
[ -x {bin} ] || exit 0
exec {bin} apply --quiet
"#
    )
}

/// systemd unit re-asserting the profile at boot.
pub fn service_unit(bin: &str) -> String {
    format!(
        r#"[Unit]
Description=Apply TUXEDO charging profile for the current network (tuxcharge-profile)
Documentation=https://github.com/Okazakee/tuxcharge-profile
After=NetworkManager.service
Wants=NetworkManager.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart={bin} apply

[Install]
WantedBy=multi-user.target
"#
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub profile: String,
    pub reason: String,
}

// =====================================================================
// embedded controller
// =====================================================================

/// The EC charging profile attribute and its siblings.
#[derive(Debug, Clone)]
pub struct Ec {
    profile_path: PathBuf,
}

impl Ec {
    pub fn new(profile_path: impl Into<PathBuf>) -> Self {
        Ec {
            profile_path: profile_path.into(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.profile_path
    }

    pub fn present(&self) -> bool {
        self.profile_path.exists()
    }

    /// Profiles the running driver advertises, falling back to the known set.
    pub fn available(&self) -> Vec<String> {
        self.profile_path
            .parent()
            .map(|dir| dir.join("charging_profiles_available"))
            .and_then(|path| fs::read_to_string(path).ok())
            .map(|text| {
                text.split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|profiles| !profiles.is_empty())
            .unwrap_or_else(|| KNOWN_PROFILES.iter().map(|p| p.to_string()).collect())
    }

    pub fn current(&self) -> Result<String, String> {
        let raw = fs::read_to_string(&self.profile_path).map_err(|e| {
            format!(
                "cannot read EC profile {}: {e}",
                self.profile_path.display()
            )
        })?;
        Ok(raw.trim().to_string())
    }

    /// Reject profiles the running driver does not advertise.
    pub fn validate(&self, profile: &str) -> Result<(), String> {
        let available = self.available();
        if available.iter().any(|p| p == profile) {
            Ok(())
        } else {
            Err(format!(
                "unknown profile \"{profile}\" (available: {})",
                available.join(", ")
            ))
        }
    }

    pub fn set(&self, profile: &str) -> Result<(), String> {
        self.validate(profile)?;

        fs::write(&self.profile_path, format!("{profile}\n")).map_err(|e| {
            let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
                " (run with sudo)"
            } else {
                ""
            };
            format!(
                "cannot write EC profile {}: {e}{hint}",
                self.profile_path.display()
            )
        })
    }
}

// =====================================================================
// network
// =====================================================================

/// SSID of the connected access point, or `None` when no Wi-Fi is connected.
pub fn active_ssid() -> Result<Option<String>, String> {
    let output = Command::new("nmcli")
        .args(["-t", "-f", "ACTIVE,SSID", "dev", "wifi"])
        .output()
        .map_err(|e| format!("cannot run nmcli: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("nmcli failed: {}", stderr.trim()));
    }

    Ok(parse_active_ssid(&String::from_utf8_lossy(&output.stdout)))
}

/// Extract the SSID of the connected network from `nmcli -t` output.
pub fn parse_active_ssid(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("yes:").map(unescape_nmcli))
        .filter(|ssid| !ssid.is_empty())
}

/// `nmcli --terse` escapes ':' inside values as '\:'.
fn unescape_nmcli(value: &str) -> String {
    value.replace("\\:", ":")
}

/// True when TUXEDO Control Center's daemon is running.
///
/// It owns the same EC setting, so writing behind its back would fight it.
pub fn tccd_running() -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };

    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        fs::read_to_string(entry.path().join("comm"))
            .map(|comm| comm.trim() == "tccd")
            .unwrap_or(false)
    })
}

// =====================================================================
// apply
// =====================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub ssid: Option<String>,
    pub reason: String,
    pub desired: String,
    pub current: String,
    pub changed: bool,
    pub dry_run: bool,
}

/// Decide which profile belongs on `ssid` and apply it unless `dry_run`.
pub fn apply(
    config: &Config,
    ec: &Ec,
    ssid: Option<String>,
    dry_run: bool,
) -> Result<Report, String> {
    if !ec.present() {
        return Err(format!(
            "EC charging profile not found at {} — is tuxedo-drivers installed and is this a supported TUXEDO model?",
            ec.path().display()
        ));
    }

    let resolution = config.resolve(ssid.as_deref());
    // Validate even on a dry run: a typo in the config should fail loudly.
    ec.validate(&resolution.profile)?;

    let current = ec.current()?;
    let changed = current != resolution.profile;

    if changed && !dry_run {
        ec.set(&resolution.profile)?;
    }

    Ok(Report {
        ssid,
        reason: resolution.reason,
        desired: resolution.profile,
        current,
        changed,
        dry_run,
    })
}

// =====================================================================
// tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_ec(tag: &str) -> (PathBuf, Ec) {
        let dir = std::env::temp_dir().join(format!(
            "tuxcharge-profile-test-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("charging_profile")).unwrap();

        let profile_path = dir.join("charging_profile/charging_profile");
        fs::write(&profile_path, "high_capacity\n").unwrap();
        fs::write(
            dir.join("charging_profile/charging_profiles_available"),
            "high_capacity balanced stationary\n",
        )
        .unwrap();

        (dir, Ec::new(profile_path))
    }

    #[test]
    fn parses_connected_ssid() {
        let out = "no:Neighbour\nyes:Calypso-TIM\nno:Other\n";
        assert_eq!(parse_active_ssid(out), Some("Calypso-TIM".to_string()));
    }

    #[test]
    fn parses_ssid_with_escaped_colon() {
        assert_eq!(
            parse_active_ssid("yes:My\\:Network\n"),
            Some("My:Network".to_string())
        );
    }

    #[test]
    fn no_connected_network_is_none() {
        assert_eq!(parse_active_ssid("no:Neighbour\n"), None);
        assert_eq!(parse_active_ssid(""), None);
    }

    #[test]
    fn parses_config_with_default_and_rules() {
        let config = Config::parse(
            r#"
            default = "high_capacity"

            [networks]
            "Calypso-TIM" = "stationary"
            "Office WiFi" = "balanced"
            "#,
        )
        .unwrap();

        assert_eq!(config.default_profile, "high_capacity");
        assert_eq!(config.networks.len(), 2);
        assert_eq!(config.networks["Office WiFi"], "balanced");
    }

    #[test]
    fn empty_config_falls_back_to_default_profile() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.default_profile, DEFAULT_PROFILE);
        assert!(config.networks.is_empty());
    }

    #[test]
    fn resolves_rule_default_and_offline() {
        let config = Config::parse(
            r#"
            default = "high_capacity"
            [networks]
            "Calypso-TIM" = "stationary"
            "#,
        )
        .unwrap();

        assert_eq!(config.resolve(Some("Calypso-TIM")).profile, "stationary");
        assert_eq!(config.resolve(Some("Other")).profile, "high_capacity");
        assert_eq!(config.resolve(None).profile, "high_capacity");
        assert!(config.resolve(Some("Other")).reason.contains("no rule"));
    }

    #[test]
    fn applies_rule_and_is_idempotent() {
        let (dir, ec) = temp_ec("apply");
        let config = Config::parse("[networks]\n\"Calypso-TIM\" = \"stationary\"\n").unwrap();

        let first = apply(&config, &ec, Some("Calypso-TIM".into()), false).unwrap();
        assert!(first.changed);
        assert_eq!(first.desired, "stationary");
        assert_eq!(ec.current().unwrap(), "stationary");

        let second = apply(&config, &ec, Some("Calypso-TIM".into()), false).unwrap();
        assert!(!second.changed);
        assert_eq!(second.current, "stationary");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn dry_run_does_not_write() {
        let (dir, ec) = temp_ec("dry-run");
        let config = Config::parse("[networks]\n\"Calypso-TIM\" = \"stationary\"\n").unwrap();

        let report = apply(&config, &ec, Some("Calypso-TIM".into()), true).unwrap();
        assert!(report.changed);
        assert!(report.dry_run);
        assert_eq!(ec.current().unwrap(), "high_capacity");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn dry_run_still_rejects_bad_config_profile() {
        let (dir, ec) = temp_ec("typo");
        let config = Config::parse("[networks]\n\"Calypso-TIM\" = \"stationarry\"\n").unwrap();

        let err = apply(&config, &ec, Some("Calypso-TIM".into()), true).unwrap_err();
        assert!(err.contains("unknown profile"));
        assert_eq!(ec.current().unwrap(), "high_capacity");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_config_falls_back_to_defaults() {
        let path = std::env::temp_dir().join(format!(
            "tuxcharge-profile-test-missing-{}/config.toml",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(path.parent().unwrap());

        let (config, fallback) = load_config(&path).unwrap();
        assert!(fallback);
        assert_eq!(config.default_profile, DEFAULT_PROFILE);
        assert!(config.networks.is_empty());
    }

    #[test]
    fn invalid_config_is_still_an_error() {
        let dir = std::env::temp_dir().join(format!(
            "tuxcharge-profile-test-invalid-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, "default = \n").unwrap();

        assert!(load_config(&path).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn templates_embed_the_binary_path() {
        let bin = "/usr/bin/tuxcharge-profile";
        assert!(dispatcher_script(bin).contains("/usr/bin/tuxcharge-profile apply --quiet"));
        assert!(service_unit(bin).contains("ExecStart=/usr/bin/tuxcharge-profile apply"));
    }

    #[test]
    fn leaving_mapped_network_reverts_to_default() {
        let (dir, ec) = temp_ec("revert");
        let config = Config::parse("[networks]\n\"Calypso-TIM\" = \"stationary\"\n").unwrap();

        apply(&config, &ec, Some("Calypso-TIM".into()), false).unwrap();
        apply(&config, &ec, None, false).unwrap();
        assert_eq!(ec.current().unwrap(), "high_capacity");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_unknown_profile() {
        let (dir, ec) = temp_ec("unknown");
        assert!(ec.set("turbo").is_err());
        assert!(ec.set("stationary").is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reports_missing_ec_attribute() {
        let ec = Ec::new("/nonexistent/charging_profile/charging_profile");
        let err = apply(&Config::default(), &ec, None, false).unwrap_err();
        assert!(err.contains("not found"));
    }
}
