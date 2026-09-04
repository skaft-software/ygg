//! Black-box Pi import regression coverage.

use std::fs;
use std::process::Command;

#[test]
fn dry_run_import_maps_canonical_model_without_destination_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("pi");
    let home = temp.path().join("home");
    let xdg_config = temp.path().join("xdg-config");
    let xdg_cache = temp.path().join("xdg-cache");
    let xdg_data = temp.path().join("xdg-data");
    let xdg_runtime = temp.path().join("xdg-runtime");
    for directory in [
        &source,
        &home,
        &xdg_config,
        &xdg_cache,
        &xdg_data,
        &xdg_runtime,
    ] {
        fs::create_dir_all(directory).unwrap();
    }
    let settings = b"{\"model\":\"openai/gpt-4o-mini\"}\n";
    fs::write(source.join("settings.json"), settings).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ygg"))
        .current_dir(temp.path())
        .env_clear()
        .env_remove("YGG_SESSION_ID")
        .env_remove("YGG_SESSION_DB")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_CACHE_HOME", &xdg_cache)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_RUNTIME_DIR", &xdg_runtime)
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .args(["--offline", "migrate", "import", "pi", "--source"])
        .arg(&source)
        .args(["--dry-run", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "import failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["models_updated"], 1);
    assert_eq!(report["skipped"], 0);
    assert_eq!(report["diagnostics"], 0);
    assert!(report.get("model_diagnostics").is_none());
    assert_eq!(fs::read(source.join("settings.json")).unwrap(), settings);
    assert!(
        fs::read_dir(&home).unwrap().next().is_none(),
        "dry run created destination artifacts"
    );
}
