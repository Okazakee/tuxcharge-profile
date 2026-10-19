//! `tuxcharge-profile` — apply a TUXEDO charging profile based on the connected
//! Wi-Fi network.
//!
//! Designed to be invoked by NetworkManager on network changes and once at boot.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{exit, Command};

use tuxcharge_profile_lib as lib;

const BIN: &str = "tuxcharge-profile";
const INSTALL_BIN: &str = "/usr/local/bin/tuxcharge-profile";
const CONFIG_DIR: &str = "/etc/tuxcharge-profile";
const CONFIG_PATH: &str = "/etc/tuxcharge-profile/config.toml";
const SERVICE_PATH: &str = "/etc/systemd/system/tuxcharge-profile.service";
const DISPATCHER_PATH: &str = "/etc/NetworkManager/dispatcher.d/90-tuxcharge-profile";

const DISPATCHER_CONTENT: &str = r#"#!/bin/sh
# Installed by tuxcharge-profile — apply the charging profile for the new network.
# NetworkManager runs this as root on every network change.
[ -x /usr/local/bin/tuxcharge-profile ] || exit 0
exec /usr/local/bin/tuxcharge-profile apply --quiet
"#;

const SERVICE_CONTENT: &str = r#"[Unit]
Description=Apply TUXEDO charging profile for the current network (tuxcharge-profile)
Documentation=https://github.com/Okazakee/tuxcharge-profile
After=NetworkManager.service
Wants=NetworkManager.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/bin/tuxcharge-profile apply

[Install]
WantedBy=multi-user.target
"#;

struct Options {
    config: PathBuf,
    ec: PathBuf,
    quiet: bool,
    dry_run: bool,
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
    let mut opts = Options {
        config: env_path("TUXCHARGE_PROFILE_CONFIG").unwrap_or_else(|| PathBuf::from(CONFIG_PATH)),
        ec: env_path("TUXCHARGE_PROFILE_EC").unwrap_or_else(|| PathBuf::from(lib::EC_PROFILE_PATH)),
        quiet: false,
        dry_run: false,
    };

    let mut positional: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);

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
"
    );
}

// =====================================================================
// apply
// =====================================================================

fn cmd_apply(opts: &Options) -> i32 {
    if lib::tccd_running() {
        eprintln!("{BIN}: tccd is running and owns the charging profile — leaving it alone");
        return 0;
    }

    let config = match lib::Config::load(&opts.config) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{BIN}: {err}");
            return 1;
        }
    };

    let ssid = match lib::active_ssid() {
        Ok(ssid) => ssid,
        Err(err) => {
            eprintln!("{BIN}: {err}");
            return 1;
        }
    };

    let ec = lib::Ec::new(opts.ec.clone());
    match lib::apply(&config, &ec, ssid, opts.dry_run) {
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
    let network = report.ssid.as_deref().unwrap_or("no Wi-Fi");
    let prefix = if report.dry_run { "[dry-run] " } else { "" };

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

    println!("config       {}", opts.config.display());
    match lib::Config::load(&opts.config) {
        Ok(config) => println!(
            "             default \"{}\", {} rule(s)",
            config.default_profile,
            config.networks.len()
        ),
        Err(err) => println!("             {err}"),
    }

    let ssid = match lib::active_ssid() {
        Ok(ssid) => ssid,
        Err(err) => {
            eprintln!("{BIN}: {err}");
            return 1;
        }
    };
    println!("network      {}", ssid.as_deref().unwrap_or("(none)"));

    if let Ok(config) = lib::Config::load(&opts.config) {
        let resolution = config.resolve(ssid.as_deref());
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
        println!("  binary      {INSTALL_BIN}");
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

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("{BIN}: cannot locate own binary: {err}");
            return 1;
        }
    };
    if exe != Path::new(INSTALL_BIN) {
        if let Err(err) = fs::copy(&exe, INSTALL_BIN) {
            eprintln!("{BIN}: cannot install binary to {INSTALL_BIN}: {err}");
            return 1;
        }
        let _ = fs::set_permissions(INSTALL_BIN, fs::Permissions::from_mode(0o755));
        println!("installed {INSTALL_BIN}");
    }

    let scripts: [(&str, &str, u32); 2] = [
        (DISPATCHER_PATH, DISPATCHER_CONTENT, 0o755),
        (SERVICE_PATH, SERVICE_CONTENT, 0o644),
    ];
    for (path, content, mode) in scripts {
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
