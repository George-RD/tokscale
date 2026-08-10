use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tempfile::TempDir;

struct TestConfigIsolation {
    _root: TempDir,
    config_dir: PathBuf,
    previously_resolved_config_dir: PathBuf,
}

static TEST_CONFIG_ISOLATION: OnceLock<TestConfigIsolation> = OnceLock::new();

/// Initialize the integration binary's forced config isolation before the Rust
/// test harness starts worker threads.
///
/// This process-lifetime redirect is deliberate: per-test environment guards
/// race when tests execute concurrently, while restoring at process exit has no
/// observable benefit. Every test in a binary that imports this module uses the
/// same scratch cache, including future tests that forget to call `temp_home`.
#[ctor::ctor]
fn initialize_config_isolation() {
    let previously_resolved_config_dir = tokscale_core::paths::get_config_dir();
    let root = tempfile::TempDir::new().expect("create integration-test config root");
    let config_dir = root.path().join("tokscale-config");
    std::fs::create_dir_all(&config_dir).expect("create integration-test config directory");

    // SAFETY: `ctor` runs before the test harness starts any worker threads, so
    // no concurrent environment readers exist. The value is intentionally
    // immutable for the remainder of this short-lived integration process.
    unsafe { std::env::set_var("TOKSCALE_CONFIG_DIR", &config_dir) };

    let isolation = TestConfigIsolation {
        _root: root,
        config_dir,
        previously_resolved_config_dir,
    };
    assert!(
        TEST_CONFIG_ISOLATION.set(isolation).is_ok(),
        "integration-test config isolation initialized more than once"
    );
}

/// Return the process-lifetime scratch config root and verify the cache resolver
/// cannot escape it.
pub fn isolate_config_dir() -> &'static Path {
    let isolation = TEST_CONFIG_ISOLATION
        .get()
        .expect("config isolation constructor must run before tests");

    let resolved = tokscale_core::paths::get_config_dir();
    assert_eq!(
        resolved, isolation.config_dir,
        "integration tests must resolve the process-lifetime scratch config root"
    );
    assert_eq!(
        tokscale_core::paths::get_cache_dir(),
        isolation.config_dir.join("cache"),
        "source-message cache must remain under the scratch config root"
    );
    assert_ne!(
        isolation.config_dir, isolation.previously_resolved_config_dir,
        "integration-test config root unexpectedly aliases the pre-test user config root"
    );

    &isolation.config_dir
}

pub fn temp_home() -> TempDir {
    isolate_config_dir();
    tempfile::TempDir::new().expect("create integration-test home")
}

#[test]
fn source_message_cache_resolves_inside_ephemeral_config_dir() {
    let config_dir = isolate_config_dir();
    assert!(config_dir.is_dir());
    assert!(tokscale_core::paths::get_cache_dir().starts_with(config_dir));
}
