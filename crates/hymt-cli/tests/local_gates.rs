use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::process::Command;

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    artifacts: Mutex<Vec<PathBuf>>,
    cleanup_failed: AtomicBool,
}

impl Fixture {
    async fn new() -> Self {
        use std::os::unix::fs::DirBuilderExt;
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let validated = std::process::Command::new("/usr/bin/python3")
            .args([
                "-B",
                "-c",
                "import runpy,sys; gate=runpy.run_path(sys.argv[1]); gate['_private_path'](gate['ARTIFACT_ROOT'], directory=True); print(gate['ARTIFACT_ROOT'])",
            ])
            .arg(source_root().join("scripts/hooks/local-gates.py"))
            .output()
            .unwrap();
        assert!(
            validated.status.success(),
            "invalid private fixture root: {}",
            String::from_utf8_lossy(&validated.stderr)
        );
        let root =
            PathBuf::from(String::from_utf8(validated.stdout).unwrap().trim()).join(format!(
                "hymt-gates-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let fixture = Self {
            root,
            repo,
            artifacts: Mutex::new(Vec::new()),
            cleanup_failed: AtomicBool::new(false),
        };
        fixture.git(&["init", "--initial-branch=main"]).await;
        fixture.git(&["config", "user.name", "Gate Fixture"]).await;
        fixture
            .git(&["config", "user.email", "gate-fixture@example.invalid"])
            .await;
        std::fs::write(fixture.repo.join("sentinel"), "fixture\n").unwrap();
        fixture.git(&["add", "sentinel"]).await;
        fixture.git(&["commit", "-m", "fixture base"]).await;
        fixture.git(&["checkout", "-b", "fix/fixture"]).await;
        let hooks = fixture.repo.join("scripts/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        for name in ["review-check.sh", "local-gates.py", "branch-protection.sh"] {
            let source = source_root().join("scripts/hooks").join(name);
            if source.exists() {
                std::fs::copy(source, hooks.join(name)).unwrap();
            }
        }
        fixture.git(&["add", "scripts"]).await;
        fixture.git(&["commit", "-m", "fixture hooks"]).await;
        fixture
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("HYMT_LOCAL_GATE_RECEIPT")
            .env_remove("HYMT_NATIVE_REVIEW_RECEIPT")
            .env_remove("CSA_SKIP_REVIEW_CHECK")
            .env_remove("CSA_SESSION_ID")
            .env_remove("CSA_DEPTH")
            .kill_on_drop(true);
        command
    }

    fn spawn(&self, command: Command) -> OwnedChild<'_> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let number = NEXT.fetch_add(1, Ordering::Relaxed);
        let stdout = self.root.join(format!("command-{number}.stdout"));
        let stderr = self.root.join(format!("command-{number}.stderr"));
        use std::os::unix::fs::OpenOptionsExt;
        let capture = |path: &Path| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .unwrap()
        };
        let output = capture(&stdout);
        let errors = capture(&stderr);
        let control = self.root.join(format!("command-{number}.control"));
        let failed = self.root.join(format!("command-{number}.unsettled"));
        let settled = self.root.join(format!("command-{number}.settled"));
        // Each custodian has its own adoption boundary, isolated from parallel tests.
        let actual = command.as_std();
        let mut owner = std::process::Command::new("/usr/bin/python3");
        owner
            .current_dir(&self.repo)
            .args(["-c", FIXTURE_OWNER])
            .arg(source_root().join("scripts/hooks/local-gates.py"))
            .arg(&control)
            .arg(&failed)
            .arg(&settled)
            .arg(actual.get_program())
            .args(actual.get_args());
        for (key, value) in actual.get_envs() {
            if let Some(value) = value {
                owner.env(key, value);
            } else {
                owner.env_remove(key);
            }
        }
        owner
            .stdin(std::process::Stdio::piped())
            .stdout(output)
            .stderr(errors);
        let child = owner.spawn().unwrap();
        OwnedChild {
            fixture: self,
            child,
            status: None,
            control,
            failed,
            settled,
            stdout,
            stderr,
        }
    }

    async fn output(&self, command: Command) -> std::process::Output {
        self.spawn(command)
            .wait(None, Duration::from_secs(20))
            .await
            .expect("gate fixture failed or exceeded its deadline")
    }

    async fn git(&self, args: &[&str]) -> String {
        let mut command = self.command("/usr/bin/git");
        command.args(args);
        let output = self.output(command).await;
        assert!(
            output.status.success(),
            "fixture git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    async fn add_inputs(&self, paths: &[(&str, &str)]) {
        for (path, text) in paths {
            let path = self.repo.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        self.git(&["add", "."]).await;
        self.git(&["commit", "-m", "fixture inputs"]).await;
    }

    async fn helper(&self, action: &str) -> std::process::Output {
        let mut command = self.command("/usr/bin/python3");
        command.args(["scripts/hooks/local-gates.py", action]);
        self.output(command).await
    }

    async fn producer(&self, body: &str) -> std::process::Output {
        self.add_inputs(&[("justfile", &format!(
            "set shell := [\"bash\", \"-c\"]\npre-push-gate target=\"head\":\n    python3 scripts/hooks/local-gates.py run {{{{target}}}}\n_full-gate:\n    {body}\n"
        ))]).await;
        let mut command = self.command("/home/obj/.local/share/mise/shims/just");
        command.args(["--no-dotenv", "pre-push-gate", "head"]);
        self.output(command).await
    }

    async fn receipts(&self) -> (PathBuf, PathBuf, PathBuf) {
        let counter = self.root.join("aggregate-count");
        let output = self
            .producer(&format!(
                "printf 'owned aggregate\\n'; printf 'run\\n' >> '{}'",
                counter.display()
            ))
            .await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let gate = PathBuf::from(
            stdout
                .lines()
                .find_map(|line| line.strip_prefix("HYMT_LOCAL_GATE_RECEIPT="))
                .unwrap(),
        );
        let report = self.root.join("review.md");
        let data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&gate).unwrap()).unwrap();
        private_write(&report, &format!(
            "VERDICT: PASS\nACTIONABLE_COUNT: 0\nHEAD: {}\nTREE: {}\nBASE: {}\nRANGE: main...HEAD\nSynthetic whole-range fixture review only.\nVERDICT: PASS\nACTIONABLE_COUNT: 0\n",
            data["head"].as_str().unwrap(), data["tree"].as_str().unwrap(), data["base"].as_str().unwrap()
        ));
        let review = self.root.join("review.receipt.json");
        let mut native = data;
        native.as_object_mut().unwrap().remove("log");
        native.as_object_mut().unwrap().remove("log_sha256");
        native["command"] = "native review main...HEAD".into();
        native["report"] = report.to_str().unwrap().into();
        native["report_sha256"] = self.digest(&report).await.into();
        private_write(&review, &serde_json::to_string(&native).unwrap());
        (gate, review, report)
    }

    async fn digest(&self, path: &Path) -> String {
        let mut command = self.command("/usr/bin/sha256sum");
        command.arg(path);
        let output = self.output(command).await;
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned()
    }

    async fn record(&self) -> String {
        format!(
            "refs/heads/fix/fixture {} refs/heads/fix/fixture {}\n",
            self.git(&["rev-parse", "HEAD"]).await,
            "0".repeat(40)
        )
    }

    async fn verify(
        &self,
        gate: &Path,
        review: &Path,
        input: &str,
        lefthook: bool,
    ) -> std::process::Output {
        let mut command = self.command(if lefthook {
            "/home/obj/.local/bin/lefthook"
        } else {
            "/usr/bin/bash"
        });
        if lefthook {
            command.args(["run", "pre-push", "--no-auto-install", "--no-tty"]);
        } else {
            command.arg("scripts/hooks/review-check.sh");
        }
        command
            .env("HYMT_LOCAL_GATE_RECEIPT", gate)
            .env("HYMT_NATIVE_REVIEW_RECEIPT", review)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        self.spawn(command)
            .wait(Some(input.as_bytes()), Duration::from_secs(20))
            .await
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.cleanup_failed.load(Ordering::Relaxed) {
            eprintln!(
                "unsettled fixture processes; paths retained: {}",
                self.root.display()
            );
            return;
        }
        for path in self.artifacts.get_mut().unwrap() {
            std::fs::remove_dir_all(path).unwrap();
        }
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

// The producer can die before its own cleanup. This dedicated owner remains
// alive to settle only that command's direct and adopted descendants.
const FIXTURE_OWNER: &str = r#"
import os, runpy, signal, subprocess, sys, time
from pathlib import Path
helper = runpy.run_path(sys.argv[1])
control, failed, settled = map(Path, sys.argv[2:5])
helper['_subreaper'](1)
pending = []
def record_interrupt(signum, _frame):
    if not pending:
        pending.append(signum)
for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGALRM):
    signal.signal(signum, record_interrupt)
child = None
root = None
status = 1
try:
    child = subprocess.Popen(sys.argv[5:], start_new_session=True)
    root = helper['_ChildIdentity'](child.pid)
    while not root.exited():
        if pending:
            raise InterruptedError(f'fixture interrupted by signal {pending[0]}')
        if control.exists():
            action = control.read_text()
            control.unlink()
            if action == 'stop':
                raise InterruptedError('fixture cancelled')
            root.signal(int(action))
        time.sleep(0.01)
finally:
    original = sys.exception()
    if child is not None:
        try:
            status = helper['_settle'](child, root)
            with os.fdopen(os.open(settled, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'w') as output:
                output.write('settled\n')
        except BaseException as error:
            with os.fdopen(os.open(failed, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'w') as output:
                output.write(str(error))
            if original is None:
                raise
            print(f'fixture cleanup: {error}', file=sys.stderr)
if pending and status == 0:
    status = 1
sys.exit(status if status >= 0 else 128 - status)
"#;

struct OwnedChild<'a> {
    fixture: &'a Fixture,
    // std::Child keeps root custody until our explicit try_wait/wait.
    child: std::process::Child,
    status: Option<std::process::ExitStatus>,
    control: PathBuf,
    failed: PathBuf,
    settled: PathBuf,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl OwnedChild<'_> {
    fn register_artifacts(&self) {
        // Capture files and this guard are owned before any cancellable wait.
        // Read announcements during the wait and again after settlement on drop.
        let bytes = std::fs::read(&self.stdout).unwrap();
        let mut owned = self.fixture.artifacts.lock().unwrap();
        for line in String::from_utf8_lossy(&bytes).lines() {
            if let Some(path) = line.strip_prefix("local-gates: log: ") {
                let leaf = Path::new(path).parent().unwrap().to_owned();
                if !owned.contains(&leaf) {
                    owned.push(leaf);
                }
            }
        }
    }

    async fn wait(
        mut self,
        input: Option<&[u8]>,
        limit: Duration,
    ) -> std::io::Result<std::process::Output> {
        let began = Instant::now();
        if let Some(input) = input {
            use std::io::Write;
            // Fixture records fit within a pipe; close stdin before waiting.
            self.child.stdin.take().unwrap().write_all(input)?;
        } else {
            self.child.stdin.take();
        }
        let status = loop {
            self.register_artifacts();
            if let Some(status) = self.child.try_wait()? {
                self.status = Some(status);
                if !self.acknowledged() {
                    return Err(std::io::Error::other(
                        "custodian settlement acknowledgement missing; fixture paths retained",
                    ));
                }
                break status;
            }
            if began.elapsed() >= limit {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "fixture process deadline",
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        Ok(std::process::Output {
            status,
            stdout: std::fs::read(&self.stdout)?,
            stderr: std::fs::read(&self.stderr)?,
        })
    }

    fn acknowledged(&self) -> bool {
        use std::os::unix::fs::PermissionsExt;
        !self.failed.exists()
            && std::fs::symlink_metadata(&self.settled).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o777 == 0o600
            })
            && std::fs::read(&self.settled).is_ok_and(|bytes| bytes == b"settled\n")
    }

    fn interrupt(&self, signum: i32) {
        publish_control(&self.control, &signum.to_string()).unwrap();
    }

    fn settle(&mut self) -> bool {
        if self.status.is_some() {
            return self.acknowledged();
        }
        if let Err(error) = publish_control(&self.control, "stop") {
            eprintln!("fixture cancellation publication failed: {error}");
        }
        let began = Instant::now();
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.status = Some(status);
                    return self.acknowledged();
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("fixture owner wait failed: {error}");
                    return false;
                }
            }
            if began.elapsed() >= Duration::from_secs(6) {
                // Root has never been reaped: std::Child still holds its identity.
                // A lost custodian cannot certify descendant settlement.
                if let Err(error) = self.child.kill() {
                    eprintln!("fixture owner kill failed: {error}");
                }
                while began.elapsed() < Duration::from_secs(8) {
                    match self.child.try_wait() {
                        Ok(Some(status)) => {
                            self.status = Some(status);
                            break;
                        }
                        Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                        Err(_) => break,
                    }
                }
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for OwnedChild<'_> {
    fn drop(&mut self) {
        if !self.settle() {
            self.fixture.cleanup_failed.store(true, Ordering::Relaxed);
            eprintln!("fixture custodian did not certify descendant settlement");
        }
        self.register_artifacts();
    }
}

fn publish_control(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // One controller owns each command's distinct control path. A collision
    // fails without touching the file created by another invocation.
    let temporary = path.with_extension("pending");
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        output.write_all(text.as_bytes())?;
        drop(output);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn private_write(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn rejected(output: &std::process::Output, expected: &str) {
    assert!(
        !output.status.success(),
        "unexpected success: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(expected),
        "wrong rejection: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn source_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn local_gates_missing_receipts_are_rejected() {
    let fixture = Fixture::new().await;
    let bin = fixture.root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    for (name, executable) in [("git", "/usr/bin/git"), ("python3", "/usr/bin/python3")] {
        std::os::unix::fs::symlink(executable, bin.join(name)).unwrap();
    }
    let head = fixture.git(&["rev-parse", "HEAD"]).await;
    let mut command = fixture.command("/usr/bin/bash");
    command
        .arg("scripts/hooks/review-check.sh")
        .env("PATH", bin)
        .stdin(std::process::Stdio::piped());
    let input = format!(
        "refs/heads/fix/fixture {head} refs/heads/fix/fixture {}\n",
        "0".repeat(40)
    );
    let status = fixture
        .spawn(command)
        .wait(Some(input.as_bytes()), Duration::from_secs(20))
        .await
        .unwrap()
        .status;
    assert!(
        !status.success(),
        "missing quality/review receipts must reject a push even without CSA"
    );
}

#[tokio::test]
async fn local_gates_receipt_and_outgoing_boundaries() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new().await;
    let (gate, review, report) = fixture.receipts().await;
    let record = fixture.record().await;
    assert!(fixture
        .verify(&gate, &review, &record, false)
        .await
        .status
        .success());
    for bad in [
        String::new(),
        record.repeat(2),
        record.replace("refs/heads/fix/fixture", "refs/tags/v1"),
        record.replace("refs/heads/fix/fixture", "refs/heads/main"),
        record.replacen("refs/heads/fix/fixture", "refs/heads/other", 1),
        record.replacen("refs/heads/fix/fixture", "refs/heads/other", 2),
        record.replace(&fixture.git(&["rev-parse", "HEAD"]).await, &"0".repeat(40)),
        record.replace(&"0".repeat(40), "invalid-old-oid"),
    ] {
        assert!(!fixture
            .verify(&gate, &review, &bad, false)
            .await
            .status
            .success());
    }
    let original = std::fs::read_to_string(&gate).unwrap();
    let value: serde_json::Value = serde_json::from_str(&original).unwrap();
    for (key, wrong) in [
        ("version", serde_json::json!(true)),
        ("status", "FAIL".into()),
        ("head", "0".repeat(40).into()),
        ("tree", "0".repeat(40).into()),
        ("base", "0".repeat(40).into()),
        ("branch", "other".into()),
        ("range", "HEAD~1..HEAD".into()),
        ("outgoing_oid", "0".repeat(40).into()),
        ("clean_before", false.into()),
        ("command", "just test".into()),
        ("extra", "unknown".into()),
    ] {
        let mut changed = value.clone();
        changed[key] = wrong;
        private_write(&gate, &changed.to_string());
        assert!(
            !fixture
                .verify(&gate, &review, &record, false)
                .await
                .status
                .success(),
            "accepted field {key}"
        );
    }
    private_write(
        &gate,
        &original.replace("\"version\": 1", "\"version\": 1, \"version\": 1"),
    );
    rejected(
        &fixture.verify(&gate, &review, &record, false).await,
        "duplicate receipt field",
    );
    private_write(&gate, &original);
    let log = PathBuf::from(value["log"].as_str().unwrap());
    let log_text = std::fs::read_to_string(&log).unwrap();
    private_write(&log, "tampered\n");
    rejected(
        &fixture.verify(&gate, &review, &record, false).await,
        "log digest",
    );
    private_write(&log, &log_text);
    std::fs::set_permissions(&gate, std::fs::Permissions::from_mode(0o644)).unwrap();
    rejected(
        &fixture.verify(&gate, &review, &record, false).await,
        "permissions",
    );
    std::fs::set_permissions(&gate, std::fs::Permissions::from_mode(0o600)).unwrap();
    let linked = fixture.root.join("linked.receipt.json");
    std::os::unix::fs::symlink(&gate, &linked).unwrap();
    rejected(
        &fixture.verify(&linked, &review, &record, false).await,
        "regular path",
    );
    let native_original = std::fs::read_to_string(&review).unwrap();
    let report_original = std::fs::read_to_string(&report).unwrap();
    for bad in [
        report_original.replacen("VERDICT: PASS", "VERDICT: FAIL", 1),
        report_original
            .trim_end()
            .trim_end_matches("ACTIONABLE_COUNT: 0")
            .trim_end()
            .trim_end_matches("VERDICT: PASS")
            .to_owned()
            + "VERDICT: FAIL\nACTIONABLE_COUNT: 1\n",
        report_original.replace("ACTIONABLE_COUNT: 0", "ACTIONABLE_COUNT: 2"),
        report_original.replace("RANGE: main...HEAD", "RANGE: HEAD~1..HEAD"),
    ] {
        private_write(&report, &bad);
        let mut native: serde_json::Value = serde_json::from_str(&native_original).unwrap();
        native["report_sha256"] = fixture.digest(&report).await.into();
        private_write(&review, &native.to_string());
        assert!(
            !fixture
                .verify(&gate, &review, &record, false)
                .await
                .status
                .success(),
            "accepted an accurately hashed FAIL/finite report"
        );
    }
    private_write(&report, &report_original);
    private_write(&review, &native_original);
    assert!(fixture
        .verify(&gate, &review, &record, false)
        .await
        .status
        .success());
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("aggregate-count")).unwrap(),
        "run\n",
        "verification reran the aggregate"
    );
    std::fs::write(fixture.repo.join("sentinel"), "dirty\n").unwrap();
    rejected(
        &fixture.verify(&gate, &review, &record, false).await,
        "clean index",
    );
}

#[tokio::test]
async fn local_gates_producer_failure_and_changed_head_leave_no_receipt() {
    for body in [
        "printf 'visible failure\\n'; exit 7",
        "git commit --allow-empty -m moved",
    ] {
        let fixture = Fixture::new().await;
        let output = fixture.producer(body).await;
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("HYMT_LOCAL_GATE_RECEIPT="));
        assert_eq!(fixture.artifacts.lock().unwrap().len(), 1);
        let leaf = fixture.artifacts.lock().unwrap()[0].clone();
        assert!(leaf.join("gate.log").exists());
        assert!(!leaf.join("gate.receipt.json").exists());
    }
}

#[tokio::test]
async fn local_gates_real_lefthook_and_bare_transport() {
    let fixture = Fixture::new().await;
    std::fs::copy(
        source_root().join("lefthook.yml"),
        fixture.repo.join("lefthook.yml"),
    )
    .unwrap();
    fixture.git(&["add", "lefthook.yml"]).await;
    fixture.git(&["commit", "-m", "fixture hook config"]).await;
    let (gate, review, _) = fixture.receipts().await;
    let output = fixture
        .verify(&gate, &review, &fixture.record().await, true)
        .await;
    assert!(
        output.status.success(),
        "resolved Lefthook failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("no checks rerun"));
    let bad = fixture.verify(&gate, &review, "", true).await;
    assert!(
        !bad.status.success(),
        "Lefthook did not forward empty stdin"
    );
    let bare = fixture.root.join("remote.git");
    let mut command = fixture.command("/usr/bin/git");
    command.args(["init", "--bare"]).arg(&bare);
    assert!(fixture.output(command).await.status.success());
    fixture
        .git(&["remote", "add", "origin", bare.to_str().unwrap()])
        .await;
    let mut install = fixture.command("/home/obj/.local/bin/lefthook");
    install.arg("install");
    assert!(fixture.output(install).await.status.success());
    let mut push = fixture.command("/usr/bin/git");
    push.args(["push", "origin", "fix/fixture"])
        .env("LEFTHOOK_BIN", "/home/obj/.local/bin/lefthook")
        .env("HYMT_LOCAL_GATE_RECEIPT", &gate)
        .env("HYMT_NATIVE_REVIEW_RECEIPT", &review);
    let output = fixture.output(push).await;
    assert!(
        output.status.success(),
        "bare transport failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut check = fixture.command("/usr/bin/git");
    check
        .arg("--git-dir")
        .arg(&bare)
        .args(["rev-parse", "refs/heads/fix/fixture"]);
    let output = fixture.output(check).await;
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        fixture.git(&["rev-parse", "HEAD"]).await
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("aggregate-count")).unwrap(),
        "run\n"
    );
    let mut push = fixture.command("/usr/bin/git");
    push.args(["push", "origin", "fix/fixture:main"])
        .env("LEFTHOOK_BIN", "/home/obj/.local/bin/lefthook")
        .env("HYMT_LOCAL_GATE_RECEIPT", &gate)
        .env("HYMT_NATIVE_REVIEW_RECEIPT", &review);
    assert!(
        !fixture.output(push).await.status.success(),
        "protected destination was accepted"
    );
    let mut check = fixture.command("/usr/bin/git");
    check
        .arg("--git-dir")
        .arg(&bare)
        .args(["show-ref", "--verify", "refs/heads/main"]);
    assert!(!fixture.output(check).await.status.success());
    // A failing branch guard must stop the next command, even if that command
    // would otherwise return success. This probes Lefthook's execution order.
    fixture.git(&["branch", "-D", "main"]).await;
    fixture.git(&["branch", "-m", "main"]).await;
    let called = fixture.root.join("guard-was-bypassed");
    std::fs::write(
        fixture.repo.join("scripts/hooks/review-check.sh"),
        format!("#!/bin/bash\ntouch '{}'\n", called.display()),
    )
    .unwrap();
    let output = fixture
        .verify(&gate, &review, &fixture.record().await, true)
        .await;
    assert!(!output.status.success());
    assert!(
        !called.exists(),
        "Lefthook ran verification after branch protection failed"
    );
}

#[tokio::test]
async fn local_gates_complete_nul_vectors() {
    let fixture = Fixture::new().await;
    fixture
        .add_inputs(&[
            ("scripts/a good.sh", "#!/bin/bash\nprintf '%s\\n' ok\n"),
            ("scripts/z bad\nname.sh", "#!/bin/bash\nif then\n"),
            ("fixture.toml", "value = 1\n"),
        ])
        .await;
    rejected(&fixture.helper("shell-syntax").await, "syntax error");
    fixture
        .add_inputs(&[("scripts/z bad\nname.sh", "#!/bin/bash\nprintf '%s\\n' ok\n")])
        .await;
    assert!(fixture.helper("shell-syntax").await.status.success());
    assert!(fixture.helper("shellcheck").await.status.success());
    fixture
        .add_inputs(&[("scripts/z bad\nname.sh", "#!/bin/bash\necho $1\n")])
        .await;
    let output = fixture.helper("shellcheck").await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("SC2086"));
    assert!(fixture.helper("toml-parse").await.status.success());
    fixture
        .add_inputs(&[("z bad\nname.toml", "broken = [\n")])
        .await;
    assert!(
        !fixture.helper("toml-parse").await.status.success(),
        "last tracked TOML was not parsed"
    );
}

#[tokio::test]
async fn local_gates_fixed_aggregate_uses_all_locked_linux_checks() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new().await;
    std::fs::copy(
        source_root().join("justfile"),
        fixture.repo.join("justfile"),
    )
    .unwrap();
    let bin = fixture.root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let counter = fixture.root.join("cargo-arguments");
    let cargo = bin.join("cargo");
    std::fs::write(
        &cargo,
        format!(
            "#!/bin/bash\nprintf '%s\\n' \"$*\" >> '{}'\n",
            counter.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o700)).unwrap();
    let services = fixture.repo.join("services");
    std::fs::create_dir(&services).unwrap();
    for name in ["hy-mt2-quality.service", "hy-mt2-throughput.service"] {
        std::fs::copy(
            source_root().join("services").join(name),
            services.join(name),
        )
        .unwrap();
    }
    let verify = bin.join("systemd-analyze");
    std::fs::write(&verify, "#!/bin/bash\nprintf '%s\\n' 'hy-mt2-throughput.service: Command /home/obj/.local/share/llama-cpp-cuda/llama-server is not executable: No such file or directory'\nexit 1\n").unwrap();
    std::fs::set_permissions(&verify, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture.add_inputs(&[("fixture.toml", "value = 1\n")]).await;
    let mut command = fixture.command("/home/obj/.local/share/mise/shims/just");
    command.args(["--no-dotenv", "pre-push-gate", "head"]).env(
        "PATH",
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
    );
    let output = fixture.output(command).await;
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "fixed aggregate failed: {stdout} {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(counter).unwrap(),
        concat!(
            "fmt --all -- --check\n",
            "clippy --workspace --locked -- -D warnings\n",
            "test --workspace --locked\n",
            "check --workspace --locked\n",
            "check -p hymt-cli --locked --no-default-features\n",
            "test -p hymt-cli --locked --no-default-features\n"
        )
    );
    assert!(stdout.contains("bash -n:"));
    assert!(stdout.contains("Parsed fixture.toml"));
    assert!(stdout.contains("Only the two established host-specific"));
    assert!(stdout.contains("HYMT_LOCAL_GATE_RECEIPT="));
    // A non-ExecStart warning, even alongside the allowed host diagnostic,
    // must fail and may not mint a new PASS receipt.
    std::fs::write(&verify, "#!/bin/bash\nprintf '%s\\n' 'hy-mt2-throughput.service: Command /home/obj/.local/share/llama-cpp-cuda/llama-server is not executable: No such file or directory' 'unrelated warning'\nexit 1\n").unwrap();
    let mut command = fixture.command("/usr/bin/python3");
    command
        .args(["scripts/hooks/local-gates.py", "service-verify"])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        );
    rejected(&fixture.output(command).await, "unexpected systemd verify");
    std::fs::write(fixture.repo.join("sentinel"), "dirty\n").unwrap();
    let mut command = fixture.command("/usr/bin/python3");
    command.args(["scripts/hooks/local-gates.py", "run", "head"]);
    let output = fixture.output(command).await;
    rejected(&output, "clean index");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("local-gates: log:"));
}

#[tokio::test]
async fn local_gates_service_executable_and_library_paths_are_coherent() {
    let fixture = Fixture::new().await;
    let persistent = "/home/obj/.local/share/llama-cpp-cuda";
    let services = fixture.repo.join("services");
    std::fs::create_dir(&services).unwrap();
    for name in ["hy-mt2-quality.service", "hy-mt2-throughput.service"] {
        let source = source_root().join("services").join(name);
        let text = std::fs::read_to_string(&source).unwrap();
        let executable = text
            .lines()
            .find_map(|line| line.strip_prefix("ExecStart="))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();
        let library = text
            .lines()
            .find_map(|line| line.strip_prefix("Environment=LD_LIBRARY_PATH="))
            .unwrap();
        assert_eq!(
            executable,
            format!("{persistent}/llama-server"),
            "{name} must use the persistent backend"
        );
        assert_eq!(
            Path::new(executable).parent().unwrap(),
            Path::new(library),
            "{name} executable/library directory mismatch"
        );
        std::fs::copy(source, services.join(name)).unwrap();
    }
    // The real systemd parser checks the complete source units, including every
    // option beyond these paths. This never starts or reloads a service.
    let output = fixture.helper("service-verify").await;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "real static service validation failed"
    );
    if Path::new(persistent).join("llama-server").is_file() {
        assert!(
            output.stdout.is_empty() && output.stderr.is_empty(),
            "unexpected diagnostic on the provisioned host"
        );
    }
    println!(
        "real systemd static validation: exit {}, stdout={} bytes, stderr={} bytes",
        output.status.code().unwrap(),
        output.stdout.len(),
        output.stderr.len()
    );
}

#[tokio::test]
async fn local_gates_private_root_alias_and_unsafe_ancestor() {
    let fixture = Fixture::new().await;
    let mut command = fixture.command("/usr/bin/python3");
    command.arg("-c").arg(
        r#"
import importlib.util
import os
from pathlib import Path
spec = importlib.util.spec_from_file_location('gates', 'scripts/hooks/local-gates.py')
gates = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gates)
parent = Path.cwd().parent / 'private-parent'
parent.mkdir(mode=0o700)
root = parent / 'root'
root.mkdir(mode=0o700)
leaf = root / 'receipt.json'
leaf.write_text('{}')
leaf.chmod(0o600)
alias = Path.cwd().parent / 'designated-root'
alias.symlink_to(root, target_is_directory=True)
gates.ARTIFACT_ROOT = alias
assert gates._private_path(alias / leaf.name) == leaf
assert alias.is_symlink() and alias.resolve() == root
assert gates._private_path(leaf) == leaf
parent.chmod(0o777)
try:
    gates._private_path(alias / leaf.name)
except ValueError as error:
    assert 'unsafe artifact-root ancestor' in str(error)
else:
    raise AssertionError('private leaf must not waive its writable ancestor')
parent.chmod(0o700)
leaf.unlink()
leaf.symlink_to(root / 'missing')
try:
    gates._private_path(alias / leaf.name)
except ValueError as error:
    assert 'regular path' in str(error)
else:
    raise AssertionError('artifact leaf symlink must not be followed')
other = Path('/usr')
if other.stat().st_dev != Path('/mnt/ssd').stat().st_dev:
    gates.ARTIFACT_ROOT = other
    try:
        gates._private_path(other)
    except ValueError as error:
        assert 'verified SSD' in str(error)
    else:
        raise AssertionError('a different filesystem must not be accepted')
    print('cross-device artifact-root rejection exercised')
"#,
    );
    let output = fixture.output(command).await;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    assert!(
        output.status.success(),
        "root boundary fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn local_gates_blocked_output_and_wait_are_interruptible() {
    let mut failures = Vec::new();
    for (mode, signum) in [
        ("pipe", 2),
        ("pipe", 15),
        ("pipe", 14),
        ("eof", 2),
        ("eof", 14),
    ] {
        let fixture = Fixture::new().await;
        let worker = fixture.root.join("blocked-worker.py");
        let producer = fixture.root.join("blocked-producer.py");
        let controller = fixture.root.join("blocked-controller.py");
        let worker_pid = fixture.root.join("worker.pid");
        let grandchild_pid = fixture.root.join("grandchild.pid");
        let settled = fixture.root.join("producer-settled");
        let waiting = fixture.root.join("producer-waiting");
        std::fs::write(
            &worker,
            r#"
import os, sys, time
from pathlib import Path
mode, worker_pid, grandchild_pid = sys.argv[1:]
if os.fork() == 0:
    os.setsid()
    Path(grandchild_pid).write_text(str(os.getpid()))
    if mode == 'eof':
        os.close(1); os.close(2)
    while True: time.sleep(1)
Path(worker_pid).write_text(str(os.getpid()))
if mode == 'eof':
    os.close(1); os.close(2)
else:
    os.write(1, b'X' * 1048576)
while True: time.sleep(1)
"#,
        )
        .unwrap();
        std::fs::write(&producer, r#"
import importlib.util, os, subprocess, sys
from pathlib import Path
gate_path, settled, waiting, worker, mode, worker_pid, grandchild_pid = sys.argv[1:]
spec = importlib.util.spec_from_file_location('gate', gate_path)
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)
real_settle = gate._settle
def settle(child, root):
    status = real_settle(child, root)
    Path(settled).write_text('producer completed settlement')
    return status
gate._settle = settle
if mode == 'eof':
    real_popen = subprocess.Popen
    def spawn(argv, **kwargs):
        if argv[0] == 'just':
            return real_popen(['/usr/bin/python3', worker, mode, worker_pid, grandchild_pid], **kwargs)
        return real_popen(argv, **kwargs)
    gate.subprocess.Popen = spawn
    real_waitid = os.waitid
    def marked_waitid(*args):
        if args[0] == os.P_PIDFD and not args[2] & os.WNOHANG:
            Path(waiting).write_text('blocking wait entered')
        return real_waitid(*args)
    gate.os.waitid = marked_waitid
try:
    gate.run('head')
except BaseException as error:
    print(f'producer rejected: {type(error).__name__}: {error}', file=sys.stderr)
    sys.exit(1)
raise AssertionError('interrupted producer emitted success')
"#).unwrap();
        std::fs::write(&controller, r#"
import os, signal, subprocess, sys, time
from pathlib import Path
producer, gate_path, settled, waiting, worker, mode, worker_pid, grandchild_pid, signum = sys.argv[1:]
child = subprocess.Popen(['/usr/bin/python3', '-B', producer, gate_path, settled, waiting, worker, mode, worker_pid, grandchild_pid], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
announcement = child.stdout.readline().decode()
assert announcement.startswith('local-gates: log: '), announcement
print(announcement, end='', flush=True)
log = Path(announcement.removeprefix('local-gates: log: ').strip())
deadline = time.monotonic() + 5
while True:
    assert time.monotonic() < deadline, 'producer did not reach blocked boundary'
    if mode == 'pipe':
        ready = log.exists() and log.stat().st_size >= 4096 and 'pipe_write' in Path(f'/proc/{child.pid}/wchan').read_text()
    else:
        ready = Path(waiting).exists()
    if ready and Path(worker_pid).exists() and Path(grandchild_pid).exists():
        break
    time.sleep(0.01)
os.kill(child.pid, int(signum))
try:
    status = child.wait(timeout=6)
    bounded = True
except subprocess.TimeoutExpired:
    child.kill()
    child.wait()
    status = -9
    bounded = False
pids = [Path(worker_pid).read_text(), Path(grandchild_pid).read_text()]
owned_gone = all(not Path('/proc', pid).exists() for pid in pids)
receipt_absent = not log.with_name('gate.receipt.json').exists()
print(f'ORACLE mode={mode} signal={signum} bounded={bounded} status={status} producer_settled={Path(settled).exists()} owned_gone={owned_gone} receipt_absent={receipt_absent}', flush=True)
"#).unwrap();
        fixture.add_inputs(&[("justfile", &format!(
            "set shell := [\"bash\", \"-c\"]\n_full-gate:\n    /usr/bin/python3 '{}' pipe '{}' '{}'\n",
            worker.display(), worker_pid.display(), grandchild_pid.display()
        ))]).await;
        let mut command = fixture.command("/usr/bin/python3");
        command
            .arg("-B")
            .arg(&controller)
            .arg(&producer)
            .arg(source_root().join("scripts/hooks/local-gates.py"))
            .arg(&settled)
            .arg(&waiting)
            .arg(&worker)
            .arg(mode)
            .arg(&worker_pid)
            .arg(&grandchild_pid)
            .arg(signum.to_string());
        let output = fixture.output(command).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        if !text.contains("bounded=True")
            || !text.contains("producer_settled=True")
            || !text.contains("owned_gone=True")
            || !text.contains("receipt_absent=True")
            || !text.contains("status=1")
        {
            failures.push(text);
        }
    }
    assert!(
        failures.is_empty(),
        "blocked cancellation failed: {failures:?}"
    );
}

#[tokio::test]
async fn local_gates_launch_and_repeated_interrupts_settle_own_child() {
    let fixture = Fixture::new().await;
    fixture
        .add_inputs(&[(
            "justfile",
            "set shell := [\"bash\", \"-c\"]\n_full-gate:\n    sleep 30\n",
        )])
        .await;
    let child_pid = fixture.root.join("gate-child.pid");
    let settlement = fixture.root.join("producer-settled");
    let mut command = fixture.command("/usr/bin/python3");
    command.args([
        "-B",
        "-c",
        r#"
import importlib.util, os, signal, subprocess, sys
from pathlib import Path
spec = importlib.util.spec_from_file_location('gate', 'scripts/hooks/local-gates.py')
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)
real_popen = subprocess.Popen
real_settle = gate._settle
def launch(*args, **kwargs):
    child = real_popen(*args, **kwargs)
    if args[0][0] == 'just':
        Path(sys.argv[1]).write_text(str(child.pid))
        os.kill(os.getpid(), signal.SIGINT)
        os.kill(os.getpid(), signal.SIGTERM)
    return child
def settle(child, root):
    os.kill(os.getpid(), signal.SIGALRM)
    status = real_settle(child, root)
    Path(sys.argv[2]).write_text('producer completed settlement')
    return status
subprocess.Popen = launch
gate._settle = settle
try:
    gate.run('head')
except InterruptedError as error:
    assert 'signal 2' in str(error), error
    print(f'producer interrupted: {error}', file=sys.stderr)
    sys.exit(1)
raise AssertionError('interrupted gate emitted success')
"#,
    ]);
    command.arg(&child_pid).arg(&settlement);
    let output = fixture.output(command).await;
    rejected(
        &output,
        "producer interrupted: local gate interrupted by signal 2",
    );
    assert!(
        settlement.exists(),
        "outer custodian must not hide incomplete producer settlement"
    );
    let pid = std::fs::read_to_string(child_pid).unwrap();
    assert!(!Path::new("/proc").join(pid).exists());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("HYMT_LOCAL_GATE_RECEIPT="));
    let artifacts = fixture.artifacts.lock().unwrap();
    assert_eq!(artifacts.len(), 1);
    assert!(!artifacts[0].join("gate.receipt.json").exists());
}

#[tokio::test]
async fn local_gates_interrupt_and_fixture_timeout_settle_descendants() {
    for (interrupt, block_ack) in [(2, false), (14, false), (9, false), (0, false), (0, true)] {
        let fixture = Fixture::new().await;
        let worker = fixture.root.join("worker.py");
        let parent_pid = fixture.root.join("parent.pid");
        let grandchild_pid = fixture.root.join("grandchild.pid");
        std::fs::write(&worker, format!(
            "import os,signal,time\nfrom pathlib import Path\nsignal.signal(signal.SIGTERM,signal.SIG_IGN)\nif os.fork()==0:\n os.setsid()\n Path('{}').write_text(str(os.getpid()))\nelse:\n Path('{}').write_text(str(os.getpid()))\nwhile True:time.sleep(1)\n",
            grandchild_pid.display(), parent_pid.display()
        )).unwrap();
        fixture
            .add_inputs(&[(
                "justfile",
                &format!(
                    "set shell := [\"bash\", \"-c\"]\n_full-gate:\n    /usr/bin/python3 '{}'\n",
                    worker.display()
                ),
            )])
            .await;
        let mut command = fixture.command("/usr/bin/python3");
        command.args(["scripts/hooks/local-gates.py", "run", "head"]);
        let child = fixture.spawn(command);
        let ready = Instant::now();
        while !parent_pid.exists() || !grandchild_pid.exists() {
            assert!(
                ready.elapsed() < Duration::from_secs(5),
                "adversarial worker failed to start"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pids: Vec<String> = [&parent_pid, &grandchild_pid]
            .into_iter()
            .map(|path| std::fs::read_to_string(path).unwrap())
            .collect();
        let failed = child.failed.clone();
        if block_ack {
            // Settlement still runs, but positive acknowledgement cannot publish.
            std::fs::create_dir(&child.settled).unwrap();
        }
        let began = Instant::now();
        if interrupt != 0 {
            // Signal the actual producer, not its wrapper or synthetic verdict.
            child.interrupt(interrupt);
        }
        let result = child
            .wait(
                None,
                if interrupt != 0 {
                    Duration::from_secs(6)
                } else {
                    Duration::from_millis(50)
                },
            )
            .await;
        assert!(
            began.elapsed() < Duration::from_secs(8),
            "cleanup exceeded its bound"
        );
        if interrupt != 0 {
            let output = result.unwrap();
            if interrupt == 9 {
                assert!(!output.status.success(), "lost producer must not pass");
            } else {
                rejected(
                    &output,
                    if interrupt == 14 {
                        "3005-second deadline"
                    } else {
                        "interrupted by signal 2"
                    },
                );
            }
            assert!(!String::from_utf8_lossy(&output.stdout).contains("HYMT_LOCAL_GATE_RECEIPT="));
        } else {
            let timeout = result.unwrap_err();
            assert_eq!(timeout.kind(), std::io::ErrorKind::TimedOut);
            let qualified = !fixture.cleanup_failed.load(Ordering::Relaxed);
            if block_ack {
                assert!(!qualified, "missing ACK must reject timeout cleanup");
                let fault = std::fs::read_to_string(&failed).unwrap();
                assert!(fault.contains("File exists"), "wrong ACK fault: {fault}");
                println!("timeout preserved; missing ACK cleanup rejected: {fault}");
            } else {
                assert!(qualified, "uncertified cleanup after original {timeout}");
            }
        }
        for pid in pids {
            assert!(
                !Path::new("/proc").join(pid).exists(),
                "recorded descendant survived cleanup"
            );
        }
        let artifacts = fixture.artifacts.lock().unwrap();
        assert_eq!(
            artifacts.len(),
            1,
            "producer artifact ownership was lost on cancellation"
        );
        assert!(artifacts[0].join("gate.log").exists());
        assert!(!artifacts[0].join("gate.receipt.json").exists());
        if block_ack {
            let retained = artifacts.clone();
            drop(artifacts);
            let root = fixture.root.clone();
            drop(fixture);
            assert!(
                root.exists(),
                "uncertified timeout must retain fixture paths"
            );
            for leaf in retained {
                assert!(
                    leaf.exists(),
                    "uncertified timeout must retain its gate log"
                );
                // Controlled ACK publication failed after settlement; both recorded
                // descendants and the reaped custodian are gone before disposal.
                std::fs::remove_dir_all(leaf).unwrap();
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}

#[tokio::test]
async fn local_gates_reject_stale_and_foreign_child_handles() {
    let fixture = Fixture::new().await;
    let mut command = fixture.command("/usr/bin/python3");
    command.args([
        "-c",
        r#"
import importlib.util, os, signal, subprocess, sys
spec = importlib.util.spec_from_file_location('gate', 'scripts/hooks/local-gates.py')
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)
real_signal = signal.pidfd_send_signal
def forbidden(*args):
    raise AssertionError('unowned identity reached a signal syscall')
signal.pidfd_send_signal = forbidden
try:
    gate._ChildIdentity(os.getpid())
except gate._GateError as error:
    assert 'foreign' in str(error)
else:
    raise AssertionError('foreign process accepted')
child = subprocess.Popen(['/usr/bin/python3', '-c', 'import time; time.sleep(30)'])
identity = gate._ChildIdentity(child.pid)
child.kill()
child.wait()
try:
    identity.signal(signal.SIGTERM)
except gate._GateError as error:
    assert 'stale' in str(error)
else:
    raise AssertionError('externally reaped child accepted')
identity.close()
try:
    identity.signal(signal.SIGTERM)
except gate._GateError as error:
    assert 'closed' in str(error)
else:
    raise AssertionError('closed child handle accepted')
signal.pidfd_send_signal = real_signal
child = subprocess.Popen(['/usr/bin/python3', '-c', 'import time; time.sleep(30)'])
identity = gate._ChildIdentity(child.pid)
identity.signal(signal.SIGTERM)
assert child.wait(timeout=2) == -signal.SIGTERM
identity.close()
print('foreign/stale/closed rejected; owned positive control settled')
"#,
    ]);
    let output = fixture.output(command).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("owned positive control settled"));
}

#[tokio::test]
async fn local_gates_cleanup_errors_preserve_primary_and_settle_siblings() {
    let fixture = Fixture::new().await;
    let worker = fixture.root.join("worker.py");
    let pids = fixture.root.join("owned.pids");
    std::fs::write(&worker, format!(
        "import os,signal,time\nfrom pathlib import Path\nsignal.signal(signal.SIGTERM,signal.SIG_IGN)\nif os.fork()==0:os.setsid()\nwith Path('{}').open('a') as f:f.write(str(os.getpid())+'\\n')\nprint('worker ready',flush=True)\nwhile True:time.sleep(1)\n",
        pids.display()
    )).unwrap();
    fixture
        .add_inputs(&[(
            "justfile",
            &format!(
                "set shell := [\"bash\", \"-c\"]\n_full-gate:\n    /usr/bin/python3 '{}'\n",
                worker.display()
            ),
        )])
        .await;
    let mut command = fixture.command("/usr/bin/python3");
    command.args([
        "-B",
        "-c",
        r#"
import importlib.util, signal, subprocess, sys, time
from pathlib import Path
spec = importlib.util.spec_from_file_location('gate', 'scripts/hooks/local-gates.py')
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)
real_signal = gate._ChildIdentity.signal
real_wait = subprocess.Popen.wait
calls = {'signal': 0, 'wait': 0}
def failing_signal(self, signum):
    calls['signal'] += 1
    if calls['signal'] == 1:
        raise OSError(5, 'injected owned signal failure')
    return real_signal(self, signum)
def failing_wait(self, *args, **kwargs):
    if kwargs.get('timeout') == 0.05:
        calls['wait'] += 1
        if calls['wait'] == 1:
            raise subprocess.TimeoutExpired('injected wait timeout', 0.05)
    return real_wait(self, *args, **kwargs)
gate._ChildIdentity.signal = failing_signal
subprocess.Popen.wait = failing_wait
real_write = gate.os.write
def broken_write(fd, data):
    if fd == sys.stdout.fileno():
        deadline = time.monotonic() + 5
        while not Path(sys.argv[1]).exists() or len(Path(sys.argv[1]).read_text().split()) < 2:
            assert time.monotonic() < deadline, 'worker registration deadline'
            time.sleep(0.01)
        raise OSError('primary output failure')
    return real_write(fd, data)
gate.os.write = broken_write
try:
    gate.run('head')
except OSError as error:
    assert str(error) == 'primary output failure', str(error)
    assert calls['signal'] > 1 and calls['wait'] > 1, calls
    print(f'caught-primary: {error}', file=sys.stderr)
    sys.exit(1)
raise AssertionError('failed producer emitted success')
"#,
    ]);
    command.arg(&pids);
    let output = fixture.output(command).await;
    rejected(&output, "caught-primary: primary output failure");
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(errors.contains("injected owned signal failure"));
    assert!(errors.contains("injected wait timeout"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("HYMT_LOCAL_GATE_RECEIPT="));
    let recorded = std::fs::read_to_string(&pids).unwrap();
    assert_eq!(recorded.split_whitespace().count(), 2);
    for pid in recorded.split_whitespace() {
        assert!(
            !Path::new("/proc").join(pid).exists(),
            "owned sibling survived cleanup"
        );
    }
}

#[tokio::test]
async fn local_gates_custodian_exit_without_ack_fails_and_retains_paths() {
    let fixture = Fixture::new().await;
    let root = fixture.root.clone();
    // The real custodian fails before Popen creates a child. No descendants exist,
    // so the regression can safely remove retained paths after checking the fence.
    let command = fixture.command("/nonexistent-hymt-custodian-fixture-command");
    let error = fixture
        .spawn(command)
        .wait(None, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("settlement acknowledgement missing"));
    assert!(fixture.cleanup_failed.load(Ordering::Relaxed));
    assert!(root.exists());
    drop(fixture);
    assert!(root.exists(), "missing ACK must retain fixture paths");
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn local_gates_control_collision_preserves_unowned_bytes() {
    let fixture = Fixture::new().await;
    let control = fixture.root.join("collision.control");
    let temporary = control.with_extension("pending");
    private_write(&temporary, "previous invocation bytes\n");
    let error = publish_control(&control, "14").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read(&temporary).unwrap(),
        b"previous invocation bytes\n"
    );
    assert!(!control.exists());
    // The test owns this counterexample file; the failed publisher did not.
    std::fs::remove_file(&temporary).unwrap();
    publish_control(&control, "14").unwrap();
    assert_eq!(std::fs::read(&control).unwrap(), b"14");
    assert!(!temporary.exists());
}
