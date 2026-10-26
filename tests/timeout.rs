//! A hung grandchild that holds the output pipes must not keep `cross-agent` waiting past its
//! timeout. A stand-in `claude` on a temporary PATH starts a background `sleep` and then hangs;
//! the run must still end with exit 124 within a few seconds.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cross-agent-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch directory");
    dir
}

#[test]
fn a_hung_grandchild_does_not_delay_the_timeout_exit() {
    let home = scratch("timeout");
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).expect("create bin directory");
    let fake = bin.join("claude");
    std::fs::write(&fake, "#!/bin/sh\nsleep 60 &\nsleep 60\n").expect("write stand-in");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_cross-agent"))
        .args(["claude", "--timeout", "1", "hello world"])
        .env_clear()
        .env("HOME", &home)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("NUVEK_LOG_SINK", "stderr")
        .output()
        .expect("run cross-agent");
    let elapsed = started.elapsed();

    assert_eq!(output.status.code(), Some(124), "{output:?}");
    assert!(
        elapsed < Duration::from_secs(15),
        "took {elapsed:?}, a grandchild held the pipes"
    );
    let _ = std::fs::remove_dir_all(&home);
}
