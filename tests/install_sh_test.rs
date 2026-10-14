//! `install.sh` against a fake release folder (`file://`), with a fake `uname`.
#![allow(clippy::expect_used)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn write(path: &Path, text: &str, mode: u32) {
    std::fs::write(path, text).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// Runs install.sh. `sums` is the SHA256SUMS text for the asset `zriz-runner-9.9.9-x86_64-linux`.
fn run(sums: &str) -> (bool, String, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_path_buf();
    let rel = dir.join("rel/download/v9.9.9");
    std::fs::create_dir_all(&rel).expect("mkdir");
    write(
        &rel.join("zriz-runner-9.9.9-x86_64-linux"),
        "binary\n",
        0o644,
    );
    write(&rel.join("SHA256SUMS"), sums, 0o644);
    let fake = dir.join("fake");
    std::fs::create_dir_all(&fake).expect("mkdir");
    write(
        &fake.join("uname"),
        "#!/bin/sh\nif [ \"$1\" = -m ]; then echo x86_64; else echo Linux; fi\n",
        0o755,
    );
    let path = format!(
        "{}:{}",
        fake.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .env("PATH", path)
        .env(
            "ZRIZ_RUNNER_BASE_URL",
            format!("file://{}", dir.join("rel").display()),
        )
        .env("ZRIZ_RUNNER_VERSION", "v9.9.9")
        .env("ZRIZ_RUNNER_BIN_DIR", dir.join("bin"))
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run sh");
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.success(), err, tmp)
}

fn sum_of_binary() -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = dir.path().join("b");
    std::fs::write(&f, "binary\n").expect("write");
    let out = Command::new("sh")
        .args([
            "-c",
            "(sha256sum \"$1\" 2>/dev/null || shasum -a 256 \"$1\") | cut -d ' ' -f 1",
            "sh",
        ])
        .arg(&f)
        .output()
        .expect("sum");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn install_checks_the_sum_and_fails_closed() {
    let name = "zriz-runner-9.9.9-x86_64-linux";
    let good = sum_of_binary();
    let (ok, err, bin) = run(&format!("{good}  {name}\n"));
    assert!(ok, "good sum: {err}");
    assert!(bin.path().join("bin/zriz-runner").exists());
    let left: Vec<_> = std::fs::read_dir(bin.path().join("bin"))
        .expect("ls")
        .collect();
    assert_eq!(left.len(), 1, "no temp file stays");

    let (ok, err, dir) = run(&format!("{}  {name}\n", "0".repeat(64)));
    assert!(
        !ok && !dir.path().join("bin/zriz-runner").exists(),
        "wrong sum must fail"
    );
    assert!(err.contains("checksum mismatch"), "{err}");

    let (ok, err, dir) = run(&format!("{good}  other-file\n"));
    assert!(
        !ok && !dir.path().join("bin/zriz-runner").exists(),
        "missing sum must fail"
    );
    assert!(err.contains("no checksum"), "{err}");
}
