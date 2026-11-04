use std::process::{Command, Stdio};

#[test]
fn version_reports_embedded_git_revision() {
    let revision = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("read source revision");
    assert!(revision.status.success());
    let revision = String::from_utf8(revision.stdout).unwrap();
    let expected = format!(
        "hymt {} (git {})\n",
        env!("CARGO_PKG_VERSION"),
        revision.trim()
    );
    for flag in ["--version", "-V"] {
        let output = Command::new(env!("CARGO_BIN_EXE_hymt"))
            .arg(flag)
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null())
            .output()
            .expect("run version outside the source checkout");
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    }
}
