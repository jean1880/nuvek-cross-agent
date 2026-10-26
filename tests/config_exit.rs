//! An unusable secret-surface configuration stops the run with the documented exit code, before
//! any subcommand work. The binary runs with a temporary HOME and config directory.

use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cross-agent-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch directory");
    dir
}

fn run(home: &Path, surfaces: Option<PathBuf>) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cross-agent"));
    cmd.args(["status", "--json"])
        .env_clear()
        .env("HOME", home)
        .env("NUVEK_LOG_SINK", "stderr");
    if let Some(file) = surfaces {
        cmd.env("NUVEK_SECRET_SURFACES_FILE", file);
    }
    cmd.output().expect("run cross-agent")
}

#[test]
fn a_missing_explicit_surface_file_exits_78() {
    let home = scratch("missing");
    let output = run(&home, Some(home.join("absent.toml")));
    assert_eq!(output.status.code(), Some(78));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("secret-surface configuration refused"),
        "{stderr}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_valid_surface_file_runs_normally() {
    let home = scratch("valid");
    let file = home.join("surfaces.toml");
    std::fs::write(&file, "surfaces = [\"~/.config/example-app/token\"]\n").expect("write file");
    let output = run(&home, Some(file));
    assert_eq!(output.status.code(), Some(0));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn no_surface_file_runs_normally() {
    let home = scratch("none");
    let output = run(&home, None);
    assert_eq!(output.status.code(), Some(0));
    let _ = std::fs::remove_dir_all(&home);
}
