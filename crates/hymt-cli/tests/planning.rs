use std::path::PathBuf;
use std::process::{Command, Stdio};

struct TestHome(PathBuf);

impl TestHome {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "hymt-plan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hymt"));
        command
            .env("HOME", &self.0)
            .current_dir(&self.0)
            .stdin(Stdio::null());
        command
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove planning test home");
    }
}

fn assert_plan_rejected(output: &std::process::Output) {
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "hymt: --plan supports only text, stdin, file input, and batch"
    );
}

#[test]
fn unsupported_plan_rejects_before_config_or_state_access() {
    let commands: &[&[&str]] = &[
        &["config", "edit"],
        &["backend", "inspect"],
        &["man", "--original", "synthetic"],
        &["info", "synthetic"],
        &["exec", "--", "touch", "executed"],
        &["exec", "install"],
        &["exec", "precache"],
        &["tokenizer", "download"],
        &["estimate", "10"],
        &["history", "--clear"],
        &["recall"],
        &["translate-doc", "missing.md", "--output", "output.md"],
        &["telegram", "--regenerate-claim-password"],
    ];
    for malformed in [false, true] {
        for args in commands {
            let home = TestHome::new();
            let config = home.0.join(".config/hymt/config.toml");
            if malformed {
                std::fs::create_dir_all(config.parent().unwrap()).unwrap();
                std::fs::write(&config, "[invalid").unwrap();
            }
            let output = home.command().arg("--plan").args(*args).output().unwrap();
            assert_plan_rejected(&output);
            if malformed {
                assert_eq!(std::fs::read(&config).unwrap(), b"[invalid");
            } else {
                assert!(!home.0.join(".config").exists());
            }
            for path in [".cache", ".local", "executed", "output.md"] {
                assert!(
                    !home.0.join(path).exists(),
                    "unexpected side effect: {path}"
                );
            }
        }
    }
}

#[test]
fn unsupported_document_plan_rejects_before_preflight_or_output() {
    let home = TestHome::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = home.0.join(".config/hymt/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let config_bytes = format!(
        "[endpoint]\nurl = \"http://{}/v1\"\nbackend = \"openai_compatible\"\nmodel = \"test-model\"\n[translation]\ntimeout = 1\n",
        listener.local_addr().unwrap()
    );
    std::fs::write(&config, &config_bytes).unwrap();
    std::fs::write(home.0.join("source.md"), "```text\nsynthetic\n```\n").unwrap();
    std::fs::write(home.0.join("output.md"), "sentinel").unwrap();
    let output = home
        .command()
        .args([
            "translate-doc",
            "--plan",
            "source.md",
            "--output",
            "output.md",
        ])
        .output()
        .unwrap();
    assert_plan_rejected(&output);
    assert_eq!(
        std::fs::read(home.0.join("output.md")).unwrap(),
        b"sentinel"
    );
    assert_eq!(std::fs::read(&config).unwrap(), config_bytes.as_bytes());
    assert!(!home.0.join(".local").exists());
    assert!(!home.0.join(".cache").exists());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
