# Changelog

All notable changes to `noren-sdk` are documented in this file.

## [0.1.0] - Phase 1 Foundation

### Added
- **NIP transport & peer node (`NipNode`)**: full-duplex overlapped named pipes (`\\.\pipe\noren.<module>.<user-sid>`), same-user + Medium Mandatory Label SDDL (`S:(ML;;NW;;;ME)`), discovery via `noren.` prefix, `hello`/`welcome` handshake, capability registry, `state` snapshots, `event` delivery, `cmd`/`reply` with bounded timeout, allowlist enforcement, `bye`/crash detection, and exponential backoff reconnect.
- **Theme & `general.toml` (`theme`)**: `ThemeConfig` (`System`/`Light`/`Dark`, `Mica`/`Acrylic`, `Windows`/`Custom` accent), Windows Registry fallback & live resolution (`ResolvedTheme`), atomic persistence, and `theme.changed` broadcast.
- **Manifest & settings schema (`schema`, `storage`)**: `noren-module.toml` parser, all 7 field types (`boolean`, `number`, `choice`, `color`, `text`, `key_chord`, `action`), `requires_running` flag, `advanced_editors`, atomic `MoveFileExW` writes, and comment-preserving `noren-wm.conf` (`kv`) + `config.toml` (`toml`) adapters.
- **Control Center shell (`control_center`)**: on-demand WinUI 3 (`windows-reactor`) settings window launched via `--control-center`, General tab for theme, one tab per installed/running module with `(off)` marker when stopped, live `controlcenter.focus` handling, and atomic offline saves.
- **Testbed (`noren-testbed`)**: standalone Win32 testbed application + `--self-test` integration suite covering peer discovery, crash recovery, stopped-module tabs, and live `theme.changed` updates.
