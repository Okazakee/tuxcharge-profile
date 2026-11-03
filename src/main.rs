//! `tuxcharge-profile` — apply a TUXEDO charging profile based on the connected
//! Wi-Fi network.
//!
//! Designed to be invoked by NetworkManager on network changes and once at boot.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{exit, Command};
use std::time::Duration;

use tuxcharge_profile_lib as lib;

const BIN: &str = "tuxcharge-profile";
const INSTALL_BIN: &str = "/usr/local/bin/tuxcharge-profile";
const CONFIG_DIR: &str = "/etc/tuxcharge-profile";
const CONFIG_PATH: &str = "/etc/tuxcharge-profile/config.toml";
const SERVICE_PATH: &str = "/etc/systemd/system/tuxcharge-profile.service";
const DISPATCHER_PATH: &str = "/etc/NetworkManager/dispatcher.d/90-tuxcharge-profile";

struct Options {
    config: PathBuf,
    ec: PathBuf,
    quiet: bool,
    dry_run: bool,
    wait: Duration,
    ignore_missing: bool,
}

fn main() {
    let (opts, command, rest) = parse_args();

    let code = match command.as_str() {
        "apply" => cmd_apply(&opts),
        "status" => cmd_status(&opts),
        "set" => cmd_set(&opts, &rest),
        "install" => cmd_install(&opts),
        "" => {
            print_usage();
            1
        }
        other => {
            eprintln!("{BIN}: unknown command \"{other}\"\n");
            print_usage();
            1
        }
    };

    exit(code);
}

// =====================================================================
// arguments
// =====================================================================

fn parse_args() -> (Options, String, Vec<String>) {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from<I>(argv: I) -> (Options, String, Vec<String>)
where
    I: IntoIterator<Item = String>,
{
    let mut opts = Options {
        config: env_path("TUXCHARGE_PROFILE_CONFIG").unwrap_or_else(|| PathBuf::from(CONFIG_PATH)),
        ec: env_path("TUXCHARGE_PROFILE_EC").unwrap_or_else(|| PathBuf::from(lib::EC_PROFILE_PATH)),
        quiet: false,
        dry_run: false,
        wait: Duration::ZERO,
        ignore_missing: false,
    };

    let mut positional: Vec<String> = Vec::new();
    let mut args = argv.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" | "-c" => match args.next() {
                Some(value) => opts.config = PathBuf::from(value),
                None => die("--config requires a path"),
            },
            "--ec" => match args.next() {
                Some(value) => opts.ec = PathBuf::from(value),
                None => die("--ec requires a path"),
            },
            "--wait" | "-w" => match args.next() {
                Some(value) => match value.parse::<u64>() {
                    Ok(seconds) => opts.wait = Duration::from_secs(seconds),
                    Err(_) => die("--wait expects a number of seconds"),
                },
                None => die("--wait requires a number of seconds"),
            },
            "--ignore-missing" => opts.ignore_missing = true,
            "--quiet" | "-q" => opts.quiet = true,
            "--dry-run" | "-n" => opts.dry_run = true,
            "--help" | "-h" => {
                print_usage();
                exit(0);
            }
            "--version" | "-V" => {
                println!("{BIN} {}", env!("CARGO_PKG_VERSION"));
                exit(0);
            }
            other => positional.push(other.to_string()),
        }
    }

    let command = positional.first().cloned().unwrap_or_default();
    let rest = positional.into_iter().skip(1).collect();

    (opts, command, rest)
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn die(message: &str) -> ! {
    eprintln!("{BIN}: {message}");
    exit(1);
}

fn print_usage() {
    println!(
        "\
{BIN} — set a TUXEDO charging profile based on the connected Wi-Fi network

USAGE:
    {BIN} [OPTIONS] <COMMAND>

COMMANDS:
    apply              Apply the profile for the current network (no-op if already set)
    status             Show the active network, the matched rule and the EC state
    set <profile>      Set an EC profile directly, ignoring the config
    install            Install the config, NetworkManager hook and boot unit (root)

OPTIONS:
    -c, --config <PATH>   Config file (default: {CONFIG_PATH})
        --ec <PATH>       EC charging profile attribute (default: driver default)
        --wait <SECS>     Wait up to SECS for the EC attribute to appear
        --ignore-missing  Exit 0 when the EC attribute is absent (network hook)
    -n, --dry-run         Report what would change without writing to the EC
    -q, --quiet           Only print when something changes
    -h, --help            Show this help
    -V, --version         Show the version

ENVIRONMENT:
    TUXCHARGE_PROFILE_CONFIG   Same as --config
    TUXCHARGE_PROFILE_EC       Same as --ec

PROFILES:
    high_capacity   full charge (100%)
    balanced        reduced capacity (~90%)
    stationary      stationary use (~80%, slowest charging)

Config, when no file is present, defaults to full capacity:
    {EXAMPLE}

",
        EXAMPLE = lib::EXAMPLE_CONFIG_PATH
    );
}

// =====================================================================
// apply
// =====================================================================

/// Load the config, warning when the built-in defaults are in use.
fn configured(path: &Path, quiet: bool) -> Result<lib::Config, String> {
    let (config, fallback) = lib::load_config(path)?;
    if fallback && !quiet {
        eprintln!(
            "{BIN}: no config at {} — using defaults ({}); example: {}",
            path.display(),
            config.default_profile,
            lib::EXAMPLE_CONFIG_PATH
        );
    }
    Ok(config)
}

fn cmd_apply(opts: &Options) -> i32 {
    if lib::tccd_running() {
        eprintln!("{BIN}: tccd is running and owns the charging profile — leaving it alone");
        return 0;
    }

    let ec = lib::Ec::new(opts.ec.clone());

    // The driver publishes the EC attribute when its probe finishes, which at
    // boot can be later than NetworkManager's first link event.
    if !ec.present() && !opts.wait.is_zero() {
        ec.wait_until_present(opts.wait);
    }
    if !ec.present() && opts.ignore_missing {
        return 0;
    }

    let config = match configured(&opts.config, opts.quiet) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{BIN}: {err}");
            return 1;
        }
    };

    let active = match lib::active_network() {
        Ok(active) => active,
        Err(err) => {
            eprintln!("{BIN}: {err}");
            return 1;
        }
    };

    match lib::apply(&config, &ec, &active, opts.dry_run) {
        Ok(report) => {
            print_report(&report, opts.quiet);
            0
        }
        Err(err) => {
            eprintln!("{BIN}: {err}");
            1
        }
    }
}

fn print_report(report: &lib::Report, quiet: bool) {
    let prefix = if report.dry_run { "[dry-run] " } else { "" };
    let network = &report.network;

    if !report.changed {
        if !quiet {
            println!(
                "{prefix}{network}: already {desired} ({}), nothing to do",
                report.reason,
                desired = report.desired
            );
        }
        return;
    }

    println!(
        "{prefix}{network}: {current} -> {desired} ({} {changed})",
        report.reason,
        current = report.current,
        desired = report.desired,
        changed = if report.dry_run {
            "would change"
        } else {
            "changed"
        }
    );
}

// =====================================================================
// status
// =====================================================================

fn cmd_status(opts: &Options) -> i32 {
    let ec = lib::Ec::new(opts.ec.clone());
    let loaded = lib::load_config(&opts.config);

    println!("config       {}", opts.config.display());
    match &loaded {
        Ok((_, true)) => println!("             missing — using defaults"),
        Ok((config, false)) => println!(
            "             default \"{}\", {} Wi-Fi rule(s), {} connection rule(s)",
            config.default_profile,
            config.networks.len(),
            config.connections.len()
        ),
        Err(err) => println!("             {err}"),
    }

    let active = match lib::active_network() {
        Ok(active) => active,
        Err(err) => {
            eprintln!("{BIN}: {err}");
            return 1;
        }
    };
    println!(
        "network      {}",
        active.ssid.as_deref().unwrap_or("(no Wi-Fi)")
    );
    match &active.connection {
        Some(connection) => println!("connection   {} ({})", connection.name, connection.uuid),
        None => println!("connection   (none)"),
    }

    if let Ok((config, _)) = &loaded {
        let resolution = config.resolve(&active);
        println!(
            "wanted       {} ({})",
            resolution.profile, resolution.reason
        );
    }

    if !ec.present() {
        println!("ec           not found at {}", ec.path().display());
        return 1;
    }

    match ec.current() {
        Ok(current) => println!(
            "ec           {current} (available: {})",
            ec.available().join(" ")
        ),
        Err(err) => {
            eprintln!("{BIN}: {err}");
            return 1;
        }
    }

    println!(
        "tccd         {}",
        if lib::tccd_running() {
            "running (it owns the setting)"
        } else {
            "not running"
        }
    );

    0
}

// =====================================================================
// set
// =====================================================================

fn cmd_set(opts: &Options, rest: &[String]) -> i32 {
    let Some(profile) = rest.first() else {
        eprintln!("{BIN}: set requires a profile\n");
        print_usage();
        return 1;
    };

    let ec = lib::Ec::new(opts.ec.clone());
    if !ec.present() {
        eprintln!(
            "{BIN}: EC charging profile not found at {}",
            ec.path().display()
        );
        return 1;
    }

    if let Err(err) = ec.validate(profile) {
        eprintln!("{BIN}: {err}");
        return 1;
    }

    if opts.dry_run {
        println!("[dry-run] would set {profile}");
        return 0;
    }

    match ec.set(profile) {
        Ok(()) => {
            println!("set {profile}");
            0
        }
        Err(err) => {
            eprintln!("{BIN}: {err}");
            1
        }
    }
}

// =====================================================================
// install
// =====================================================================

fn cmd_install(opts: &Options) -> i32 {
    if opts.dry_run {
        println!("[dry-run] would install:");
        println!("  binary      {INSTALL_BIN} (unless already in /usr)");
        println!("  config      {CONFIG_PATH} (only if absent)");
        println!("  NM hook     {DISPATCHER_PATH}");
        println!("  unit        {SERVICE_PATH}");
        return 0;
    }

    if let Err(err) = fs::create_dir_all(CONFIG_DIR) {
        eprintln!("{BIN}: cannot create {CONFIG_DIR}: {err}");
        eprintln!("{BIN}: run install as root (sudo)");
        return 1;
    }

    if !Path::new(CONFIG_PATH).exists() {
        if let Err(err) = fs::write(CONFIG_PATH, lib::EXAMPLE_CONFIG) {
            eprintln!("{BIN}: cannot write {CONFIG_PATH}: {err}");
            return 1;
        }
        println!("wrote {CONFIG_PATH}");
    } else {
        println!("kept existing {CONFIG_PATH}");
    }

    let bin = match install_binary() {
        Ok(bin) => bin,
        Err(err) => {
            eprintln!("{BIN}: {err}");
            return 1;
        }
    };

    // Both templates point at the binary that will actually run.
    let bin_path = format!("{}", bin.display());
    let files: [(&str, String, u32); 2] = [
        (DISPATCHER_PATH, lib::dispatcher_script(&bin_path), 0o755),
        (SERVICE_PATH, lib::service_unit(&bin_path), 0o644),
    ];
    for (path, content, mode) in files {
        if let Err(err) = fs::write(path, content) {
            eprintln!("{BIN}: cannot write {path}: {err}");
            return 1;
        }
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
        println!("wrote {path}");
    }

    for args in [
        vec!["daemon-reload"],
        vec!["enable", "--now", "tuxcharge-profile.service"],
    ] {
        match Command::new("systemctl").args(&args).status() {
            Ok(status) if status.success() => {}
            Ok(status) => eprintln!("{BIN}: systemctl {} exited with {status}", args.join(" ")),
            Err(err) => eprintln!("{BIN}: cannot run systemctl: {err}"),
        }
    }

    println!("\ndone — edit {CONFIG_PATH} to map your networks, then: {BIN} apply");
    0
}

/// Path the installed service should invoke.
///
/// A binary already living in `/usr` (package install) is used as-is; anything
/// else is copied to `/usr/local/bin` first.
fn install_binary() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate own binary: {e}"))?;

    if exe.starts_with("/usr/bin") || exe.starts_with("/usr/local/bin") {
        println!("using existing {}", exe.display());
        return Ok(exe);
    }

    fs::copy(&exe, INSTALL_BIN)
        .map_err(|e| format!("cannot install binary to {INSTALL_BIN}: {e}"))?;
    let _ = fs::set_permissions(INSTALL_BIN, fs::Permissions::from_mode(0o755));
    println!("installed {INSTALL_BIN}");

    Ok(PathBuf::from(INSTALL_BIN))
}

// =====================================================================
// tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> (Options, String, Vec<String>) {
        parse_args_from(argv.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn wait_defaults_to_zero_and_missing_is_an_error() {
        let (opts, command, rest) = parse(&["apply"]);
        assert_eq!(command, "apply");
        assert!(rest.is_empty());
        assert!(opts.wait.is_zero());
        assert!(!opts.ignore_missing);
    }

    #[test]
    fn parses_wait_and_ignore_missing() {
        let (opts, command, _) = parse(&["apply", "--wait", "5", "--ignore-missing", "--quiet"]);
        assert_eq!(command, "apply");
        assert_eq!(opts.wait, Duration::from_secs(5));
        assert!(opts.ignore_missing);
        assert!(opts.quiet);
    }

    #[test]
    fn accepts_the_short_wait_flag() {
        let (opts, _, _) = parse(&["apply", "-w", "2"]);
        assert_eq!(opts.wait, Duration::from_secs(2));
    }

    #[test]
    fn keeps_positional_arguments_after_flags() {
        let (_, command, rest) = parse(&["set", "balanced", "--dry-run"]);
        assert_eq!(command, "set");
        assert_eq!(rest, vec!["balanced".to_string()]);
    }
}
