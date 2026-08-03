//! Cross-platform resolution for tokscale's user config and cache dirs.
//!
//! Tokscale-core needs the same path helpers tokscale-cli uses (settings
//! and message/pricing caches read from related directories), so the
//! resolver lives here and is re-exported from tokscale-cli for callers
//! that already imported it from there. macOS users following the docs
//! expect `~/.config/tokscale/` because that is what `auth.rs`,
//! `cursor.rs`, and `antigravity.rs` already write to.
//! `dirs::config_dir()` would instead return `~/Library/Application Support/`
//! on macOS, splitting state across two roots and silently ignoring
//! settings.json edits the user made via the documented path. This module
//! enforces the unified `~/.config/tokscale/` location on macOS + Linux,
//! while keeping the platform default on Windows.

use std::path::PathBuf;

/// Resolve the user's home directory, honoring `$HOME` on every platform.
///
/// `dirs::home_dir()` reads `$HOME` on Unix but goes straight to the Win32
/// known-folder API on Windows, where no environment variable can redirect
/// it. The asymmetry is invisible until something actually runs on Windows:
/// a caller that points `HOME` at a scratch directory is obeyed on Unix and
/// silently ignored on Windows, so it keeps reading — and writing — the real
/// profile. That is exactly what #997 found once a Windows runner existed;
/// `test_ensure_config_dir` was creating `%USERPROFILE%\.config\tokscale` on
/// the machine running the suite, and `test_load_credentials_nonexistent`
/// was asserting against the developer's own credentials file.
///
/// `$HOME` is only honored on Windows when it is an absolute native path
/// (`C:\...`, `\\?\C:\...`, `\\server\share`). Two shapes are rejected, both
/// because `Path` silently resolves them against ambient state:
///
/// - POSIX-shaped values such as `/home/user`, exported by MSYS2, Cygwin and
///   Git Bash. `Path` reads the leading `/` as "root of the current drive",
///   so obeying them would relocate the config of every user who launches
///   tokscale from a Unix-shell emulator to `C:\home\user`.
/// - Drive-relative values such as `C:temp`, which carry a `Prefix` but no
///   root. Windows resolves those against the *per-drive current directory*,
///   so the same `HOME` names a different place depending on where the
///   process last `cd`-ed on drive C — home-rooted reads and writes would
///   land somewhere unintended and unreproducible.
///
/// `Path::is_absolute` on Windows is exactly `has_root() && prefix().is_some()`,
/// which rejects both in one check while leaving the redirect available to
/// anything that can set a real native path.
///
/// Every home-rooted path in the workspace must resolve through here rather
/// than calling `dirs::home_dir()` directly, otherwise the redirect holds for
/// some roots and not others and the two disagree about where state lives.
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(explicit) = windows_native_home_override() {
            return Some(explicit);
        }
    }

    dirs::home_dir()
}

/// `$HOME` on Windows, but only when it names a path Win32 can actually use.
///
/// See [`home_dir`] for why the absoluteness check is load-bearing rather than
/// a nicety: without it a Git Bash `HOME=/home/user` would win over the real
/// profile, and a drive-relative `HOME=C:temp` would resolve against whatever
/// the current directory on drive C happens to be.
#[cfg(windows)]
fn windows_native_home_override() -> Option<PathBuf> {
    let raw = std::env::var_os("HOME")?;
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    path.is_absolute().then_some(path)
}

/// Resolve the tokscale config dir, honoring `TOKSCALE_CONFIG_DIR` first.
///
/// Resolution order:
/// 1. `TOKSCALE_CONFIG_DIR` taken verbatim when set to a non-empty value.
///    Absolute paths are recommended; relative paths are accepted and
///    resolved against the process CWD. Empty strings are treated as
///    unset so the user gets the platform default instead of a surprise
///    `./` write — keeps the resolver consistent with
///    [`is_config_dir_overridden`], which also rejects empty strings.
/// 2. macOS: `$HOME/.config/tokscale` (overrides `dirs::config_dir()`,
///    which would return `~/Library/Application Support/` and split state
///    across two roots — see module docs).
/// 3. Linux: `dirs::config_dir().join("tokscale")` so XDG_CONFIG_HOME is
///    honored. Falls through to `$HOME/.config/tokscale` when neither
///    `XDG_CONFIG_HOME` nor `HOME` resolve.
/// 4. Windows (and any other platform): `dirs::config_dir().join("tokscale")`.
/// 5. Last-ditch fallback: `./.tokscale` so a missing HOME never panics.
pub fn get_config_dir() -> PathBuf {
    if let Some(custom) = std::env::var_os("TOKSCALE_CONFIG_DIR") {
        if !custom.is_empty() {
            return PathBuf::from(custom);
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(home) = home_dir() {
            return home.join(".config").join("tokscale");
        }
    }

    dirs::config_dir()
        .map(|d| d.join("tokscale"))
        .unwrap_or_else(|| PathBuf::from(".tokscale"))
}

/// Resolve the tokscale cache dir as `<config_dir>/cache`.
///
/// Caches (TUI display data, source-message bincode, pricing JSON, the
/// OpenCode migration record, Wrapped fonts/images) all live under this
/// single subdirectory so an isolated profile (`TOKSCALE_CONFIG_DIR=...`)
/// covers everything in one shot, and so `rm -rf <cache_dir>` is always
/// safe — no durable state mixed in.
pub fn get_cache_dir() -> PathBuf {
    get_config_dir().join("cache")
}

/// Whether `TOKSCALE_CONFIG_DIR` is explicitly set in the environment.
///
/// Callers that want to read a legacy on-disk location during a path
/// transition MUST gate that fallback on this returning `false`. When the
/// override is set (CI sandbox, tests, isolated profile), the user has
/// asked for an explicit, hermetic root — silently ingesting files from
/// the historic `~/.cache/tokscale/` or `~/Library/Caches/tokscale/`
/// locations defeats that contract.
pub fn is_config_dir_overridden() -> bool {
    std::env::var_os("TOKSCALE_CONFIG_DIR").is_some_and(|v| !v.is_empty())
}

/// Pre-#470 cache directory at `dirs::cache_dir()/tokscale`.
///
/// On macOS this resolves to `~/Library/Caches/tokscale/` (where the
/// source-message-cache, pricing caches, and opencode-migration.json
/// historically lived). On Linux this resolves to `$XDG_CACHE_HOME/tokscale`
/// or `~/.cache/tokscale/`.
///
/// Returns `None` when `TOKSCALE_CONFIG_DIR` is set so the override stays
/// hermetic (no legacy-data leak into isolated profiles).
pub fn legacy_dirs_cache_dir() -> Option<PathBuf> {
    if is_config_dir_overridden() {
        return None;
    }
    dirs::cache_dir().map(|d| d.join("tokscale"))
}

/// Pre-#470 cache directory at `~/.cache/tokscale`.
///
/// This is where the TUI display cache (`tui-data-cache.json`) and the
/// Wrapped image / font caches lived before #470 consolidated everything
/// under `<config_dir>/cache`. On Linux this typically equals
/// [`legacy_dirs_cache_dir`]; on macOS it does NOT (Library/Caches vs
/// `.cache`), so both legacy probes need to run during migration.
///
/// Returns `None` when `TOKSCALE_CONFIG_DIR` is set or HOME cannot be
/// resolved.
pub fn legacy_dot_cache_tokscale_dir() -> Option<PathBuf> {
    if is_config_dir_overridden() {
        return None;
    }
    home_dir().map(|h| h.join(".cache").join("tokscale"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;
    use std::path::Path;

    fn save_env() -> (
        Option<std::ffi::OsString>,
        Option<std::ffi::OsString>,
        Option<std::ffi::OsString>,
    ) {
        (
            env::var_os("TOKSCALE_CONFIG_DIR"),
            env::var_os("HOME"),
            env::var_os("XDG_CONFIG_HOME"),
        )
    }

    fn restore_env(
        prev: (
            Option<std::ffi::OsString>,
            Option<std::ffi::OsString>,
            Option<std::ffi::OsString>,
        ),
    ) {
        unsafe {
            match prev.0 {
                Some(v) => env::set_var("TOKSCALE_CONFIG_DIR", v),
                None => env::remove_var("TOKSCALE_CONFIG_DIR"),
            }
            match prev.1 {
                Some(v) => env::set_var("HOME", v),
                None => env::remove_var("HOME"),
            }
            match prev.2 {
                Some(v) => env::set_var("XDG_CONFIG_HOME", v),
                None => env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    /// The regression #997 is really about: this assertion has always held on
    /// Unix and has never held on Windows, because `dirs::home_dir()` consults
    /// the Win32 profile API there and no environment variable can reach it.
    /// Every home-rooted test in the workspace redirects `HOME` and assumes
    /// the code under test follows, so this one assertion is what makes the
    /// rest of them mean the same thing on both platforms.
    #[test]
    #[serial]
    fn home_dir_follows_an_explicit_native_home_on_every_platform() {
        let prev = save_env();
        // `env::temp_dir()` is absolute and carries a drive prefix on Windows,
        // which is exactly the shape a `TempDir`-based test produces.
        let redirect = env::temp_dir().join("tokscale-core-home-dir-probe");
        unsafe {
            env::set_var("HOME", &redirect);
        }
        assert_eq!(home_dir(), Some(redirect));
        restore_env(prev);
    }

    /// MSYS2, Cygwin and Git Bash export `HOME=/home/<user>`. `Path` reads a
    /// leading `/` on Windows as "root of the current drive", so honoring that
    /// value would silently move a real user's credentials and scan roots to
    /// `C:\home\<user>`. The prefix check in `windows_native_home_override`
    /// exists solely to keep that from happening.
    #[test]
    #[serial]
    #[cfg(windows)]
    fn home_dir_ignores_a_posix_shaped_home() {
        let prev = save_env();
        unsafe {
            env::set_var("HOME", "/home/runner");
        }
        assert_ne!(home_dir(), Some(PathBuf::from("/home/runner")));
        restore_env(prev);
    }

    /// `C:temp` carries a `Prefix` component but no root, so a prefix-only
    /// check accepts it. Windows then resolves it against the *per-drive
    /// current directory* for C: — the same `HOME` names a different directory
    /// depending on where the process last `cd`-ed, so credentials and scan
    /// roots move unpredictably. Only absolute native paths may redirect home.
    ///
    /// Windows-only by construction: `C:temp` is a perfectly ordinary relative
    /// filename on Unix, and `Path`'s prefix parsing only exists on Windows
    /// targets, so there is no way to exercise this on macOS. It does run —
    /// on the `windows-latest` leg this PR adds.
    #[test]
    #[serial]
    #[cfg(windows)]
    fn home_dir_ignores_a_drive_relative_home() {
        let prev = save_env();
        unsafe {
            env::set_var("HOME", r"C:temp");
        }
        assert_ne!(
            home_dir(),
            Some(PathBuf::from(r"C:temp")),
            "a drive-relative HOME resolves against the current directory on that drive"
        );
        restore_env(prev);
    }

    /// Same contract as `get_config_dir`: an exported-but-blank variable is a
    /// misconfiguration, not a request to resolve every home-rooted path
    /// against the process CWD.
    #[test]
    #[serial]
    #[cfg(windows)]
    fn home_dir_treats_an_empty_home_as_unset() {
        let prev = save_env();
        unsafe {
            env::set_var("HOME", "");
        }
        assert_ne!(home_dir(), Some(PathBuf::new()));
        restore_env(prev);
    }

    #[test]
    #[serial]
    fn env_override_is_returned_verbatim() {
        let prev = save_env();
        unsafe {
            env::set_var("TOKSCALE_CONFIG_DIR", "/tmp/tokscale-custom");
        }
        assert_eq!(get_config_dir(), PathBuf::from("/tmp/tokscale-custom"));
        restore_env(prev);
    }

    #[test]
    #[serial]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn unix_default_is_dot_config_tokscale_under_home() {
        let prev = save_env();
        unsafe {
            env::remove_var("TOKSCALE_CONFIG_DIR");
            env::remove_var("XDG_CONFIG_HOME");
            env::set_var("HOME", "/tmp/tokscale-core-paths-home");
        }
        assert_eq!(
            get_config_dir(),
            PathBuf::from("/tmp/tokscale-core-paths-home/.config/tokscale"),
        );
        restore_env(prev);
    }

    #[test]
    #[serial]
    #[cfg(target_os = "linux")]
    fn linux_honors_xdg_config_home_when_set() {
        let prev = save_env();
        unsafe {
            env::remove_var("TOKSCALE_CONFIG_DIR");
            env::set_var("XDG_CONFIG_HOME", "/tmp/tokscale-core-paths-xdg");
        }
        assert_eq!(
            get_config_dir(),
            PathBuf::from("/tmp/tokscale-core-paths-xdg/tokscale"),
        );
        restore_env(prev);
    }

    #[test]
    #[serial]
    fn cache_dir_is_cache_subdir_of_config_dir() {
        let prev = save_env();
        unsafe {
            env::set_var("TOKSCALE_CONFIG_DIR", "/tmp/tokscale-cache-test");
        }
        assert_eq!(
            get_cache_dir(),
            PathBuf::from("/tmp/tokscale-cache-test/cache")
        );
        restore_env(prev);
    }

    #[test]
    #[serial]
    fn legacy_helpers_return_none_when_overridden() {
        let prev = save_env();
        unsafe {
            env::set_var("TOKSCALE_CONFIG_DIR", "/tmp/tokscale-override");
        }
        assert!(legacy_dirs_cache_dir().is_none());
        assert!(legacy_dot_cache_tokscale_dir().is_none());
        restore_env(prev);
    }

    #[test]
    #[serial]
    fn legacy_helpers_return_some_when_not_overridden() {
        let prev = save_env();
        unsafe {
            env::remove_var("TOKSCALE_CONFIG_DIR");
        }
        assert!(
            legacy_dirs_cache_dir().is_some(),
            "dirs::cache_dir always resolves on test platforms"
        );
        assert!(
            legacy_dot_cache_tokscale_dir().is_some(),
            "HOME is set in test environments"
        );
        restore_env(prev);
    }

    #[test]
    #[serial]
    fn get_config_dir_treats_empty_override_as_unset() {
        // Empty TOKSCALE_CONFIG_DIR previously slipped through and
        // produced PathBuf::from(""), which silently relocated cache
        // writes to ./cache and ./.tokscale. The resolver must agree
        // with `is_config_dir_overridden`: empty == unset.
        let prev = save_env();
        unsafe {
            env::set_var("TOKSCALE_CONFIG_DIR", "");
        }
        let resolved = get_config_dir();
        assert_ne!(
            resolved,
            PathBuf::from(""),
            "empty override must not resolve to the empty path"
        );
        assert!(
            resolved.is_absolute() || resolved == Path::new(".tokscale"),
            "empty override must fall through to platform default, got {resolved:?}"
        );
        restore_env(prev);
    }

    #[test]
    #[serial]
    fn is_config_dir_overridden_treats_empty_string_as_unset() {
        let prev = save_env();
        unsafe {
            env::set_var("TOKSCALE_CONFIG_DIR", "");
        }
        assert!(!is_config_dir_overridden());
        restore_env(prev);
    }
}
