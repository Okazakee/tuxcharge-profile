//! Core logic for `tuxcharge-profile`.
//!
//! Maps the currently connected network (Wi-Fi SSID, or a NetworkManager
//! connection name/UUID for wired links) to a TUXEDO embedded controller
//! charging profile, so the battery charge limit follows you between networks
//! even without TUXEDO Control Center installed.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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
pub const EXAMPLE_CONFIG: &str = r#"# tuxcharge-profile — charging profile per network
#
# Profiles written by the TUXEDO driver (Uniwill EC):
#   high_capacity  full charge, full charging speed  (100%)
#   balanced       reduced capacity                  (~90%)
#   stationary     stationary use, slowest charging   (~80%)
#
# [networks] matches the connected Wi-Fi SSID.
# [connections] matches the NetworkManager connection name or UUID, which also
# covers Ethernet. An SSID rule wins when both would apply.
#
# Any network without a rule below gets `default`.

default = "high_capacity"

[networks]
# Quotes are required for SSIDs containing spaces or special characters.
"HomeWiFi" = "stationary"

[connections]
# Names and UUIDs come from `nmcli -t -f NAME,UUID connection show`.
"Office dock" = "balanced"
"#;

/// Where the example config lives when installed from a package.
pub const EXAMPLE_CONFIG_PATH: &str = "/usr/share/tuxcharge-profile/config.toml.example";

// =====================================================================
// config
// =====================================================================

/// Mapping of networks to charging profiles.
///
/// `[networks]` matches the connected Wi-Fi SSID. `[connections]` matches the
/// NetworkManager connection name or UUID, which is what Ethernet has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub default_profile: String,
    pub networks: BTreeMap<String, String>,
    pub connections: BTreeMap<String, String>,
}

#[derive(serde::Deserialize)]
struct RawConfig {
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    networks: BTreeMap<String, String>,
    #[serde(default)]
    connections: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            default_profile: DEFAULT_PROFILE.to_string(),
            networks: BTreeMap::new(),
            connections: BTreeMap::new(),
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
        config.connections = raw.connections;

        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    /// Which profile the given network should use, and why.
    ///
    /// An SSID rule wins over a connection rule, so existing `[networks]`
    /// configs keep behaving exactly as before.
    pub fn resolve(&self, active: &ActiveNetwork) -> Resolution {
        if let Some(ssid) = active.ssid.as_deref() {
            if let Some(profile) = self.networks.get(ssid) {
                return Resolution {
                    profile: profile.clone(),
                    reason: format!("rule for Wi-Fi \"{ssid}\""),
                };
            }
        }

        if let Some(connection) = active.connection.as_ref() {
            if let Some(profile) = self.connections.get(&connection.name) {
                return Resolution {
                    profile: profile.clone(),
                    reason: format!("rule for connection \"{}\"", connection.name),
                };
            }
            if let Some(profile) = self.connections.get(&connection.uuid) {
                return Resolution {
                    profile: profile.clone(),
                    reason: format!("rule for connection UUID {}", connection.uuid),
                };
            }
        }

        Resolution {
            profile: self.default_profile.clone(),
            reason: format!("default (no rule for {})", active.label()),
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
///
/// `--ignore-missing` matters at boot: NetworkManager reports the first link
/// before `tuxedo_keyboard` has finished probing and published the EC
/// attributes, and a non-zero exit there is logged as a failed hook. The boot
/// unit waits for the attribute, so this one can stay a no-op.
pub fn dispatcher_script(bin: &str) -> String {
    format!(
        r#"#!/bin/sh
# Installed by tuxcharge-profile — apply the charging profile for the new network.
# NetworkManager runs this as root on every network change.
[ -x {bin} ] || exit 0
exec {bin} apply --quiet --ignore-missing
"#
    )
}

/// systemd unit re-asserting the profile at boot.
///
/// The wait covers the window between `NetworkManager.service` and the
/// `tuxedo_keyboard` platform-probe that creates the sysfs attributes.
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
ExecStart={bin} apply --wait 5

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

/// Poll `path` until it exists or `timeout` elapses.
///
/// Kernel drivers publish sysfs attributes when their probe completes, which
/// can be later than the unit or hook that wants to write them. Callers that
/// run at boot use this instead of failing on the first look.
pub fn wait_for(path: &Path, timeout: Duration, interval: Duration) -> bool {
    if path.exists() {
        return true;
    }

    let interval = interval.max(Duration::from_millis(1));
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(interval.min(remaining));
        if path.exists() {
            return true;
        }
    }

    false
}

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

    /// Wait up to `timeout` for the driver to publish the attribute.
    pub fn wait_until_present(&self, timeout: Duration) -> bool {
        wait_for(&self.profile_path, timeout, Duration::from_millis(100))
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

/// NetworkManager connection behind the active link.
///
/// The name and UUID both address the same profile, and this is the only
/// identifier Ethernet has: it has no SSID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveConnection {
    pub name: String,
    pub uuid: String,
}

/// Everything matching needs to know about the current network.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveNetwork {
    pub ssid: Option<String>,
    pub connection: Option<ActiveConnection>,
}

impl ActiveNetwork {
    /// Human-readable description for status output and rule reasons.
    pub fn label(&self) -> String {
        match (&self.ssid, &self.connection) {
            (Some(ssid), _) => format!("Wi-Fi \"{ssid}\""),
            (None, Some(connection)) => format!("connection \"{}\"", connection.name),
            (None, None) => "no active network".to_string(),
        }
    }
}

/// Read both identifiers. Wi-Fi is optional; a wired link has only a connection.
pub fn active_network() -> Result<ActiveNetwork, String> {
    Ok(ActiveNetwork {
        ssid: active_ssid()?,
        connection: active_connection()?,
    })
}

/// Name and UUID of the active NetworkManager connection, if any.
pub fn active_connection() -> Result<Option<ActiveConnection>, String> {
    let output = Command::new("nmcli")
        .args([
            "-t",
            "-f",
            "NAME,UUID,TYPE",
            "connection",
            "show",
            "--active",
        ])
        .output()
        .map_err(|e| format!("cannot run nmcli: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("nmcli failed: {}", stderr.trim()));
    }

    Ok(parse_active_connection(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// Extract the active connection from `nmcli -t connection show --active`.
///
/// Prefers a wired or wireless link over a VPN or bridge layered on top of it,
/// so a VPN does not hide the physical connection a rule was written for.
pub fn parse_active_connection(output: &str) -> Option<ActiveConnection> {
    let mut fallback = None;

    for line in output.lines() {
        let fields = split_nmcli_fields(line);
        if fields.len() < 3 {
            continue;
        }

        let (name, uuid, kind) = (&fields[0], &fields[1], &fields[2]);
        if name.is_empty() || uuid.is_empty() || kind == "loopback" {
            continue;
        }

        let connection = ActiveConnection {
            name: name.clone(),
            uuid: uuid.clone(),
        };

        if kind == "802-11-wireless" || kind == "802-3-ethernet" {
            return Some(connection);
        }
        fallback.get_or_insert(connection);
    }

    fallback
}

/// Split one line of `nmcli --terse` output into its fields.
///
/// `nmcli` escapes ':' and '\\' inside values with a backslash.
fn split_nmcli_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut chars = line.chars();

    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(escaped) = chars.next() {
                    field.push(escaped);
                }
            }
            ':' => fields.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    fields.push(field);

    fields
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
    pub network: String,
    pub reason: String,
    pub desired: String,
    pub current: String,
    pub changed: bool,
    pub dry_run: bool,
}

/// Decide which profile belongs on `active` and apply it unless `dry_run`.
pub fn apply(
    config: &Config,
    ec: &Ec,
    active: &ActiveNetwork,
    dry_run: bool,
) -> Result<Report, String> {
    if !ec.present() {
        return Err(format!(
            "EC charging profile not found at {} — is tuxedo-drivers installed and is this a supported TUXEDO model?",
            ec.path().display()
        ));
    }

    let resolution = config.resolve(active);
    // Validate even on a dry run: a typo in the config should fail loudly.
    ec.validate(&resolution.profile)?;

    let current = ec.current()?;
    let changed = current != resolution.profile;

    if changed && !dry_run {
        ec.set(&resolution.profile)?;
    }

    Ok(Report {
        network: active.label(),
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

    fn wifi(ssid: &str) -> ActiveNetwork {
        ActiveNetwork {
            ssid: Some(ssid.to_string()),
            connection: None,
        }
    }

    fn wired(name: &str, uuid: &str) -> ActiveNetwork {
        ActiveNetwork {
            ssid: None,
            connection: Some(ActiveConnection {
                name: name.to_string(),
                uuid: uuid.to_string(),
            }),
        }
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
    fn parses_active_connection_and_skips_non_physical_links() {
        let out = "\
lo:11111111-1111-1111-1111-111111111111:loopback\n\
work vpn:22222222-2222-2222-2222-222222222222:vpn\n\
HomeWiFi:33333333-3333-3333-3333-333333333333:802-11-wireless\n";
        assert_eq!(
            parse_active_connection(out),
            Some(ActiveConnection {
                name: "HomeWiFi".to_string(),
                uuid: "33333333-3333-3333-3333-333333333333".to_string(),
            })
        );
    }

    #[test]
    fn parses_wired_connection_with_escaped_colon() {
        let out = "Office\\: dock:44444444-4444-4444-4444-444444444444:802-3-ethernet\n";
        assert_eq!(
            parse_active_connection(out),
            Some(ActiveConnection {
                name: "Office: dock".to_string(),
                uuid: "44444444-4444-4444-4444-444444444444".to_string(),
            })
        );
    }

    #[test]
    fn no_active_connection_is_none() {
        assert_eq!(parse_active_connection(""), None);
        assert_eq!(
            parse_active_connection("lo:11111111-1111-1111-1111-111111111111:loopback\n"),
            None
        );
    }

    #[test]
    fn parses_config_with_default_and_rules() {
        let config = Config::parse(
            r#"
            default = "high_capacity"

            [networks]
            "Calypso-TIM" = "stationary"
            "Office WiFi" = "balanced"

            [connections]
            "Office dock" = "stationary"
            "#,
        )
        .unwrap();

        assert_eq!(config.default_profile, "high_capacity");
        assert_eq!(config.networks.len(), 2);
        assert_eq!(config.connections.len(), 1);
        assert_eq!(config.networks["Office WiFi"], "balanced");
        assert_eq!(config.connections["Office dock"], "stationary");
    }

    #[test]
    fn empty_config_falls_back_to_default_profile() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.default_profile, DEFAULT_PROFILE);
        assert!(config.networks.is_empty());
        assert!(config.connections.is_empty());
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

        assert_eq!(config.resolve(&wifi("Calypso-TIM")).profile, "stationary");
        assert_eq!(config.resolve(&wifi("Other")).profile, "high_capacity");
        assert_eq!(
            config.resolve(&ActiveNetwork::default()).profile,
            "high_capacity"
        );
        assert!(config.resolve(&wifi("Other")).reason.contains("no rule"));
    }

    #[test]
    fn resolves_connection_by_name_and_uuid() {
        let config = Config::parse(
            r#"
            [connections]
            "Office dock" = "stationary"
            "55555555-5555-5555-5555-555555555555" = "balanced"
            "#,
        )
        .unwrap();

        assert_eq!(
            config.resolve(&wired("Office dock", "uuid-a")).profile,
            "stationary"
        );
        assert_eq!(
            config
                .resolve(&wired("Other", "55555555-5555-5555-5555-555555555555"))
                .profile,
            "balanced"
        );
        assert!(config
            .resolve(&wired("Office dock", "uuid-a"))
            .reason
            .contains("Office dock"));
    }

    #[test]
    fn ssid_rule_wins_over_connection_rule() {
        let config = Config::parse(
            r#"
            [networks]
            "HomeWiFi" = "stationary"
            [connections]
            "HomeWiFi" = "balanced"
            "#,
        )
        .unwrap();

        let active = ActiveNetwork {
            ssid: Some("HomeWiFi".to_string()),
            connection: Some(ActiveConnection {
                name: "HomeWiFi".to_string(),
                uuid: "uuid-a".to_string(),
            }),
        };

        assert_eq!(config.resolve(&active).profile, "stationary");
    }

    #[test]
    fn ethernet_without_a_rule_gets_the_default() {
        let config = Config::parse("[networks]\n\"HomeWiFi\" = \"stationary\"\n").unwrap();

        assert_eq!(
            config
                .resolve(&wired("Wired connection 1", "uuid-a"))
                .profile,
            "high_capacity"
        );
    }

    #[test]
    fn applies_rule_and_is_idempotent() {
        let (dir, ec) = temp_ec("apply");
        let config = Config::parse("[networks]\n\"Calypso-TIM\" = \"stationary\"\n").unwrap();

        let first = apply(&config, &ec, &wifi("Calypso-TIM"), false).unwrap();
        assert!(first.changed);
        assert_eq!(first.desired, "stationary");
        assert_eq!(ec.current().unwrap(), "stationary");

        let second = apply(&config, &ec, &wifi("Calypso-TIM"), false).unwrap();
        assert!(!second.changed);
        assert_eq!(second.current, "stationary");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn dry_run_does_not_write() {
        let (dir, ec) = temp_ec("dry-run");
        let config = Config::parse("[networks]\n\"Calypso-TIM\" = \"stationary\"\n").unwrap();

        let report = apply(&config, &ec, &wifi("Calypso-TIM"), true).unwrap();
        assert!(report.changed);
        assert!(report.dry_run);
        assert_eq!(ec.current().unwrap(), "high_capacity");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn dry_run_still_rejects_bad_config_profile() {
        let (dir, ec) = temp_ec("typo");
        let config = Config::parse("[networks]\n\"Calypso-TIM\" = \"stationarry\"\n").unwrap();

        let err = apply(&config, &ec, &wifi("Calypso-TIM"), true).unwrap_err();
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
        assert!(dispatcher_script(bin)
            .contains("/usr/bin/tuxcharge-profile apply --quiet --ignore-missing"));
        assert!(service_unit(bin).contains("ExecStart=/usr/bin/tuxcharge-profile apply --wait 5"));
    }

    #[test]
    fn wait_for_returns_immediately_when_the_path_exists() {
        let (dir, ec) = temp_ec("wait-present");

        let started = Instant::now();
        assert!(ec.wait_until_present(Duration::from_secs(2)));
        assert!(started.elapsed() < Duration::from_millis(500));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn wait_for_ignores_a_zero_timeout_when_present() {
        let (dir, ec) = temp_ec("wait-zero");
        assert!(ec.wait_until_present(Duration::ZERO));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn wait_for_gives_up_after_the_timeout() {
        let path = std::env::temp_dir().join(format!(
            "tuxcharge-profile-test-wait-absent-{}/charging_profile",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(path.parent().unwrap());

        let started = Instant::now();
        assert!(!wait_for(
            &path,
            Duration::from_millis(120),
            Duration::from_millis(10)
        ));
        assert!(started.elapsed() >= Duration::from_millis(100));
    }

    #[test]
    fn wait_for_returns_once_the_attribute_appears() {
        let dir = std::env::temp_dir().join(format!(
            "tuxcharge-profile-test-wait-late-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("charging_profile");

        let created = path.clone();
        let creator = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            fs::write(&created, "balanced\n").unwrap();
        });

        assert!(wait_for(
            &path,
            Duration::from_secs(2),
            Duration::from_millis(10)
        ));
        creator.join().unwrap();

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn leaving_mapped_network_reverts_to_default() {
        let (dir, ec) = temp_ec("revert");
        let config = Config::parse("[networks]\n\"Calypso-TIM\" = \"stationary\"\n").unwrap();

        apply(&config, &ec, &wifi("Calypso-TIM"), false).unwrap();
        apply(&config, &ec, &ActiveNetwork::default(), false).unwrap();
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
        let err = apply(&Config::default(), &ec, &ActiveNetwork::default(), false).unwrap_err();
        assert!(err.contains("not found"));
    }
}
