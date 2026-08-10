use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tempfile::TempDir;

struct TestConfigIsolation {
    _root: TempDir,
    config_dir: PathBuf,
    previously_resolved_config_dir: PathBuf,
}

static TEST_CONFIG_ISOLATION: OnceLock<TestConfigIsolation> = OnceLock::new();

/// Pin this integration-test process to one durable scratch config root.
///
/// `TOKSCALE_CONFIG_DIR` is process-global, so per-test guards can race when
/// the test harness runs cases concurrently. A process-lifetime `OnceLock`
/// makes initialization atomic and keeps the `TempDir` alive until exit.
pub fn isolate_config_dir() -> &'static Path {
    let isolation = TEST_CONFIG_ISOLATION.get_or_init(|| {
        let previously_resolved_config_dir = tokscale_core::paths::get_config_dir();
        let root = tempfile::TempDir::new().expect("create integration-test config root");
        let config_dir = root.path().join("tokscale-config");
        std::fs::create_dir_all(&config_dir).expect("create integration-test config directory");

        // SAFETY: initialization is serialized by OnceLock, every test in this
        // process calls this helper before invoking a cache-aware core API, and
        // the override is intentionally never restored before process exit.
        unsafe { std::env::set_var("TOKSCALE_CONFIG_DIR", &config_dir) };

        TestConfigIsolation {
            _root: root,
            config_dir,
            previously_resolved_config_dir,
        }
    });

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
