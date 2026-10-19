# tuxcharge-profile

[![MIT License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

Set a TUXEDO laptop charging profile based on the connected Wi-Fi network.

TUXEDO Control Center can limit battery charging to ~90% or ~80%, but the setting
lives in the embedded controller and goes with it. Uninstall TCC and the profile
silently reverts to full capacity; nothing re-applies it. This tool makes the
choice network-driven instead: limit charging on the networks where you sit at a
desk, charge fully everywhere else.

No daemon, no polling: NetworkManager invokes it on network changes, plus one
boot-time unit that re-asserts the profile.

## Charging profiles

The values come from the TUXEDO driver (`tuxedo_keyboard`, Uniwill EC):

| Profile | Behavior | Real charge |
|---|---|---|
| `high_capacity` | Full charge, full charging speed | ~100% |
| `balanced` | Reduced charging speed | ~90% |
| `stationary` | Slowest charging, best for desk use | ~80% |

The operating system still displays 100% when a limit is active — the cap happens
in firmware and is not visible to the kernel.

## Install

```bash
git clone https://github.com/Okazakee/tuxcharge-profile
cd tuxcharge-profile
cargo build --release
sudo ./target/release/tuxcharge-profile install
```

`install` copies the binary to `/usr/local/bin`, writes
`/etc/tuxcharge-profile/config.toml`, adds the NetworkManager hook and the boot
unit, then enables the unit.

## Configure

`/etc/tuxcharge-profile/config.toml`:

```toml
default = "high_capacity"

[networks]
"Calypso-TIM" = "stationary"
"Office WiFi" = "balanced"
```

- A network listed under `[networks]` gets its profile.
- Everything else — another Wi-Fi, Wi-Fi off, plain Ethernet — gets `default`.
- Quote SSIDs containing spaces or special characters.

Then apply immediately:

```bash
sudo tuxcharge-profile apply
```

## Commands

| Command | |
|---|---|
| `tuxcharge-profile apply` | Apply the profile for the current network (no-op if already correct) |
| `tuxcharge-profile status` | Active network, matched rule, EC state, whether `tccd` is running |
| `tuxcharge-profile set <profile>` | Set an EC profile directly, ignoring the config |
| `tuxcharge-profile install` | Install config, NetworkManager hook and boot unit (root) |

Useful flags: `--dry-run` (report without writing), `--quiet` (only print on
change), `--config <path>`, `--ec <path>`.

## How it works

1. NetworkManager runs `/etc/NetworkManager/dispatcher.d/90-tuxcharge-profile` on
   every network change; the boot unit covers the case where it is already
   connected to something.
2. The tool reads the active SSID from `nmcli`, resolves it against the config,
   reads the current EC profile, and writes the new one only if it differs.
3. Writes go to the same driver attribute TCC used:
   `/sys/devices/platform/tuxedo_keyboard/charging_profile/charging_profile`.

Because the write goes through the driver, the driver's own state stays in sync,
so a resume from suspend does not undo the change.

## Requirements

- TUXEDO laptop whose EC exposes the charging profile attribute — check with
  `tuxcharge-profile status`
- `tuxedo-drivers-dkms`
- NetworkManager (`nmcli`)
- Root for `apply`/`set`/`install`; `status` works as a normal user

## Notes and limitations

- If `tccd` (TUXEDO Control Center) is running, the tool refuses to write: TCC owns
  the same setting, and both writing it would cause flip-flopping. Uninstall TCC
  or stop `tccd` to use this.
- Matching is exact per SSID. There are deliberately no wildcards or
  AC/battery rules.
- NetworkManager only. Users on `iwd`/`systemd-networkd` would need a different
  trigger.

## Uninstall

```bash
sudo systemctl disable --now tuxcharge-profile.service
sudo rm /etc/NetworkManager/dispatcher.d/90-tuxcharge-profile \
        /etc/systemd/system/tuxcharge-profile.service \
        /usr/local/bin/tuxcharge-profile
sudo systemctl daemon-reload
sudo rm -r /etc/tuxcharge-profile   # config, optional
```

The EC keeps whatever profile was last applied; set it back with
`tuxcharge-profile set high_capacity` beforehand if you want full charge.

## License

MIT
