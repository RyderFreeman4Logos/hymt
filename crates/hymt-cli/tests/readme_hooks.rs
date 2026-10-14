use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const SOURCE: &str = "Committed English README.\n";
const OLD_CHINESE: &str = "Original Chinese README.\n";
const NEW_CHINESE: &str = "Updated Chinese README.\n";

const GIT: &str = r#"#!/bin/bash
if [ "$1" = -C ]; then shift 2; fi
printf 'git' >> "$FIXTURE_LOG"
printf '\t%s' "$@" >> "$FIXTURE_LOG"
printf '\n' >> "$FIXTURE_LOG"
case "$1" in
  rev-parse)
    case "$*" in
      'rev-parse --show-toplevel') printf '%s\n' "$FIXTURE_ROOT" ;;
      'rev-parse HEAD') printf 'fixture-head\n' ;;
      'rev-parse --symbolic-full-name HEAD') printf 'refs/heads/fixture\n' ;;
      *) exit 92 ;;
    esac
    ;;
  diff-tree) printf '%s\n' "$FIXTURE_CHANGED" ;;
  diff)
    case "$*" in
      'diff --cached -- README.zh-cn.md') { [ "$FIXTURE_CACHED_DIRTY" = 1 ] || [ -f "$FIXTURE_ROOT/staged-chinese" ]; } && printf 'cached change\n' ;;
      'diff --cached -- README.md') [ "$FIXTURE_ENGLISH_STAGED" = 1 ] && printf 'staged English\n' ;;
      'diff -- README.zh-cn.md') [ "$FIXTURE_CHINESE_DIRTY" = 1 ] && printf 'Chinese change\n' ;;
      'diff -- README.md') [ "$FIXTURE_ENGLISH_DIRTY" = 1 ] && printf 'English change\n' ;;
      *) exit 91 ;;
    esac
    ;;
  show)
    [ "$FIXTURE_PREP_FAIL" = snapshot ] && exit 42
    /usr/bin/cat "$FIXTURE_ROOT/committed.md"
    ;;
  status)
    if ! /usr/bin/cmp -s "$FIXTURE_ROOT/README.zh-cn.md" "$FIXTURE_ROOT/chinese-baseline"; then
      printf ' M README.zh-cn.md\n'
    fi
    ;;
  commit)
    if [ -f "$FIXTURE_ROOT/consumer-paths" ]; then
      runtime=$(/usr/bin/head -n 1 "$FIXTURE_ROOT/consumer-paths")
      [ -d "$runtime" ] && [ -f "$runtime/.config/hymt/config.toml" ] && [ -d "$JUST_TEMPDIR" ] || exit 95
      printf 'followup-alive\t%s\n' "$JUST_TEMPDIR" >> "$FIXTURE_LOG"
    fi
    if [ "$FIXTURE_CANCEL" = commit ]; then
      printf 'commit-cancel\n' >> "$FIXTURE_LOG"
      /usr/bin/sleep 30 &
      descendant=$!
      printf '%s %s %s\n' "$BASHPID" "$(/usr/bin/awk '{print $22}' /proc/$BASHPID/stat)" "$descendant" > "$FIXTURE_ROOT/consumer-process"
      trap 'wait "$descendant" 2>/dev/null; printf settled > "$FIXTURE_ROOT/consumer-settled"; exit 143' TERM
      kill -TERM "$PPID"
      wait "$descendant"
      exit 98
    fi
    [ "$FIXTURE_COMMIT_EXIT" = 0 ] || exit "$FIXTURE_COMMIT_EXIT"
    /usr/bin/cp "$FIXTURE_ROOT/README.zh-cn.md" "$FIXTURE_ROOT/chinese-baseline"
    FIXTURE_CHANGED=README.zh-cn.md /usr/bin/bash "$FIXTURE_ROOT/scripts/hooks/auto-translate-readme.sh"
    ;;
  *) exit 92 ;;
esac
exit 0
"#;

const TRANSLATOR: &str = r#"#!/bin/bash
cancel_if_requested() {
  [ "$FIXTURE_CANCEL" = "$1" ] || return 0
  printf '%s-cancel\n' "$1" >> "$FIXTURE_LOG"
  /usr/bin/sleep 30 &
  descendant=$!
  printf '%s %s %s\n' "$BASHPID" "$(/usr/bin/awk '{print $22}' /proc/$BASHPID/stat)" "$descendant" > "$FIXTURE_ROOT/consumer-process"
  trap 'wait "$descendant" 2>/dev/null; printf settled > "$FIXTURE_ROOT/consumer-settled"; exit 143' TERM
  kill -TERM "$PPID"
  wait "$descendant"
  exit 98
}
if [ "$1" = --help ]; then
  printf 'help\n' >> "$FIXTURE_LOG"
  cancel_if_requested probe
  if [ "$FIXTURE_CAPABLE" = 1 ]; then printf 'translate-doc\n'; else printf 'legacy help\n'; fi
  exit 0
fi
if [ "$1" = -m ]; then
  [ "$2" = hymt ] || exit 93
  shift 2
  printf 'legacy' >> "$FIXTURE_LOG"
else
  printf 'installed' >> "$FIXTURE_LOG"
fi
printf '\t%s' "$@" >> "$FIXTURE_LOG"
printf '\n' >> "$FIXTURE_LOG"
[ "$1" = translate-doc ] && [ "$3" = -l ] && [ "$4" = zh ] && [ "$5" = --output ] || exit 94
/usr/bin/cp "$2" "$FIXTURE_ROOT/translated-source.md"
printf '%s\n' "$HOME" "$6" "$2" > "$FIXTURE_ROOT/consumer-paths"
/usr/bin/stat -c '%a' "$HOME" "$HOME/.config/hymt" "$HOME/.cache/hymt" "$HOME/.local/share/hymt" "$HOME/.config/hymt/config.toml" "$6" "$2" > "$FIXTURE_ROOT/consumer-modes"
case "$FIXTURE_REPLACE" in
  workspace)
    workspace=${HOME%/*}
    /usr/bin/mv "$workspace" "$workspace.saved"
    /usr/bin/mkdir "$workspace"
    printf foreign > "$workspace/foreign"
    exit 42 ;;
  config)
    /usr/bin/mv "$HOME/.config/hymt/config.toml" "$HOME/.config/hymt/config.toml.saved"
    printf foreign > "$HOME/.config/hymt/config.toml"
    exit 42 ;;
  candidate)
    /usr/bin/mv "$6" "$6.saved"
    printf foreign > "$6"
    printf 'candidate-foreign\n' >> "$FIXTURE_LOG"
    exit 42 ;;
  destination)
    /usr/bin/mv "$FIXTURE_ROOT/README.zh-cn.md" "$FIXTURE_ROOT/old-output"
    printf foreign > "$FIXTURE_ROOT/README.zh-cn.md" ;;
esac
cancel_if_requested translation
if [ "$FIXTURE_PARTIAL" = 1 ]; then
  printf partial > "$6"
  printf 'partial-written\n' >> "$FIXTURE_LOG"
fi
if [ "$FIXTURE_CHINESE_CONCURRENT" = edit ]; then
  printf 'Chinese edited during translation.\n' > "$FIXTURE_ROOT/README.zh-cn.md"
elif [ "$FIXTURE_CHINESE_CONCURRENT" = stage ]; then
  printf 'Chinese staged during translation.\n' > "$FIXTURE_ROOT/staged-chinese"
elif [ "$FIXTURE_CHINESE_CONCURRENT" = real-stage ]; then
  printf 'Chinese staged during translation.\n' > "$FIXTURE_ROOT/README.zh-cn.md"
  /usr/bin/git -C "$FIXTURE_ROOT" add README.zh-cn.md || exit 96
elif [ "$FIXTURE_CHINESE_CONCURRENT" = chmod ]; then
  /usr/bin/chmod 0600 "$FIXTURE_ROOT/README.zh-cn.md" || exit 96
fi
case "$FIXTURE_HEAD_CHANGE" in
  advance)
    printf 'Later committed English.\n' > "$FIXTURE_ROOT/README.md"
    /usr/bin/git -C "$FIXTURE_ROOT" add README.md || exit 97
    /usr/bin/git -C "$FIXTURE_ROOT" commit -qm later || exit 97 ;;
  switch)
    /usr/bin/git -C "$FIXTURE_ROOT" branch later || exit 97
    /usr/bin/git -C "$FIXTURE_ROOT" switch -q later || exit 97 ;;
esac
if [ "$FIXTURE_TRANSLATE_EXIT" = 0 ]; then
  if [ "$FIXTURE_ATOMIC" = 1 ]; then
    printf '%s' "$FIXTURE_RESULT" > "$6.new"
    /usr/bin/mv "$6.new" "$6"
    printf 'atomic-writer\n' >> "$FIXTURE_LOG"
  else
    printf '%s' "$FIXTURE_RESULT" > "$6"
  fi
fi
exit "$FIXTURE_TRANSLATE_EXIT"
"#;

const MKDIR: &str = r#"#!/bin/bash
printf 'mkdir\t%s\n' "$*" >> "$FIXTURE_LOG"
case "$*" in
  *hymt-home*) [ "$FIXTURE_PREP_FAIL" = mkdir ] && exit 42 ;;
  *just-tmp*) [ "$FIXTURE_PREP_FAIL" = followup-mkdir ] && exit 42 ;;
esac
/usr/bin/mkdir "$@" || exit $?
if [ "$FIXTURE_HEAD_CHANGE" = after-publication ] && [[ "$*" == *just-tmp* ]]; then
  printf 'Later committed English.\n' > "$FIXTURE_ROOT/README.md"
  /usr/bin/git -C "$FIXTURE_ROOT" add README.md || exit 97
  /usr/bin/git -C "$FIXTURE_ROOT" commit -qm later || exit 97
fi
"#;

const COPY: &str = r#"#!/bin/bash
printf 'copy\n' >> "$FIXTURE_LOG"
[ "$FIXTURE_PREP_FAIL" = copy ] && exit 42
case "${@: -1}" in
  */.README.zh-cn.md.hymt-*)
    printf 'publication-copy\t%s\n' "${@: -1}" >> "$FIXTURE_LOG"
    if [ "$FIXTURE_REPLACE" = publication ]; then
      /usr/bin/mv "${@: -1}" "${@: -1}.saved"
      printf foreign > "${@: -1}"
      exit 42
    fi
    if [ "$FIXTURE_PREP_FAIL" = publish-copy ]; then
      printf partial > "${@: -1}"
      printf 'publication-partial\n' >> "$FIXTURE_LOG"
      exit 42
    fi ;;
esac
exec /usr/bin/cp "$@"
"#;

const MKTEMP: &str = r#"#!/bin/bash
printf 'mktemp\n' >> "$FIXTURE_LOG"
case "$*" in
  *README-committed*) [ "$FIXTURE_PREP_FAIL" = mktemp ] && exit 42 ;;
  *candidate-*) [ "$FIXTURE_PREP_FAIL" = candidate ] && exit 42 ;;
  *.README.zh-cn.md.hymt-*) [ "$FIXTURE_PREP_FAIL" = publish-mktemp ] && exit 42 ;;
  *) [ "$FIXTURE_PREP_FAIL" = workspace ] && exit 42 ;;
esac
exec /usr/bin/mktemp "$@"
"#;

const MODE: &str = r#"#!/bin/bash
printf 'publication-mode\n' >> "$FIXTURE_LOG"
if [ "$FIXTURE_REPLACE" = publication-mode ]; then
  /usr/bin/mv "${@: -1}" "${@: -1}.saved"
  printf foreign > "${@: -1}"
fi
[ "$FIXTURE_PREP_FAIL" = publish-chmod ] && exit 42
exec /usr/bin/chmod "$@"
"#;

const MOVE: &str = r#"#!/bin/bash
printf 'publication-rename\n' >> "$FIXTURE_LOG"
[ "$FIXTURE_PREP_FAIL" = publish-mv ] && exit 42
exec /usr/bin/mv "$@"
"#;

const STAT: &str = r#"#!/bin/bash
if [ "$(/usr/bin/readlink -f -- "${@: -1}")" = "$(/usr/bin/readlink -f -- "$FIXTURE_ROOT")" ]; then
  if [ "$FIXTURE_ROOT_FAULT" = device ] && [ "$2" = %d ]; then printf 'root-fault\tdevice\n' >> "$FIXTURE_LOG"; printf '0\n'; exit 0; fi
  if [ "$FIXTURE_ROOT_FAULT" = owner ] && [ "$2" = %u ]; then printf 'root-fault\towner\n' >> "$FIXTURE_LOG"; printf '999999\n'; exit 0; fi
fi
exec /usr/bin/stat "$@"
"#;

struct Fixture {
    root: PathBuf,
    installed: bool,
}

impl Fixture {
    fn new(installed: bool, legacy: bool) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "hymt-readme-hooks-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        for dir in ["bin", ".git", "scripts/hooks", "home/.config/hymt"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for (name, text) in [
            ("README.md", SOURCE),
            ("committed.md", SOURCE),
            ("README.zh-cn.md", OLD_CHINESE),
            ("chinese-baseline", OLD_CHINESE),
            ("index-sentinel", "unrelated staged bytes"),
            ("home/.config/hymt/config.toml", "fixture configuration\n"),
        ] {
            std::fs::write(root.join(name), text).unwrap();
        }
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for name in ["auto-translate-readme.sh", "translate-readme-zh.sh"] {
            std::fs::copy(
                repo.join("scripts/hooks").join(name),
                root.join("scripts/hooks").join(name),
            )
            .unwrap();
        }
        for (name, script) in [
            ("git", GIT),
            ("mkdir", MKDIR),
            ("cp", COPY),
            ("mktemp", MKTEMP),
            ("chmod", MODE),
            ("mv", MOVE),
            ("stat", STAT),
        ] {
            Self::script(&root.join("bin").join(name), script);
        }
        for name in ["bash", "grep", "rm", "env", "readlink", "sha256sum"] {
            std::os::unix::fs::symlink(format!("/usr/bin/{name}"), root.join("bin").join(name))
                .unwrap();
        }
        if installed {
            Self::script(&root.join("bin/hymt"), TRANSLATOR);
        }
        if legacy {
            std::fs::create_dir_all(root.join(".venv/bin")).unwrap();
            Self::script(&root.join(".venv/bin/python"), TRANSLATOR);
        }
        Self { root, installed }
    }

    fn script(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn git(&self, args: &[&str]) -> Vec<u8> {
        let output = std::process::Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .env("HOME", self.root.join("home"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        output.stdout
    }

    fn use_real_git(&self) {
        self.git(&["init", "-q", "-b", "main"]);
        self.git(&["config", "user.name", "Fixture"]);
        self.git(&["config", "user.email", "fixture@example.invalid"]);
        self.git(&["add", "README.md", "README.zh-cn.md"]);
        self.git(&["commit", "-qm", "initial"]);
        std::fs::write(self.root.join("README.md"), "Committed English update.\n").unwrap();
        self.git(&["add", "README.md"]);
        self.git(&["commit", "-qm", "English update"]);
        std::fs::remove_file(self.root.join("bin/git")).unwrap();
        std::os::unix::fs::symlink("/usr/bin/git", self.root.join("bin/git")).unwrap();
    }

    async fn run(&self, wrapper: bool, changes: &[(&str, &str)]) -> Output {
        self.run_from(wrapper, changes, &self.root).await
    }

    async fn run_from(&self, wrapper: bool, changes: &[(&str, &str)], cwd: &Path) -> Output {
        let mut command = tokio::process::Command::new("/usr/bin/bash");
        command
            .arg("-c")
            .arg("umask 022; exec /usr/bin/bash \"$1\"")
            .arg("fixture")
            .arg(self.root.join("scripts/hooks").join(if wrapper {
                "auto-translate-readme.sh"
            } else {
                "translate-readme-zh.sh"
            }))
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("PATH", self.root.join("bin"))
            .env("TMPDIR", &self.root)
            .env("TMP", &self.root)
            .env("TEMP", &self.root)
            .env("FIXTURE_ROOT", &self.root)
            .env("FIXTURE_LOG", self.root.join("events"))
            .env("FIXTURE_CHANGED", "README.md")
            .env("FIXTURE_ENGLISH_DIRTY", "0")
            .env("FIXTURE_ENGLISH_STAGED", "0")
            .env("FIXTURE_CHINESE_DIRTY", "0")
            .env("FIXTURE_CHINESE_CONCURRENT", "")
            .env("FIXTURE_HEAD_CHANGE", "")
            .env("FIXTURE_CACHED_DIRTY", "0")
            .env("FIXTURE_PREP_FAIL", "")
            .env("FIXTURE_TRANSLATE_EXIT", "0")
            .env("FIXTURE_PARTIAL", "0")
            .env("FIXTURE_REPLACE", "")
            .env("FIXTURE_CANCEL", "")
            .env("FIXTURE_ROOT_FAULT", "")
            .env("FIXTURE_ATOMIC", "0")
            .env("FIXTURE_CAPABLE", "1")
            .env("FIXTURE_COMMIT_EXIT", "0")
            .env("FIXTURE_RESULT", NEW_CHINESE)
            .envs(changes.iter().copied())
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let mut child = command.spawn().unwrap();
        let hook_pid = child.id().unwrap();
        let status = match tokio::time::timeout(Duration::from_secs(8), child.wait()).await {
            Ok(status) => status.unwrap(),
            Err(_) => {
                self.kill_owned_consumer();
                Self::kill_group(hook_pid);
                tokio::time::timeout(Duration::from_secs(2), child.wait())
                    .await
                    .expect("hook did not reap after kill")
                    .unwrap();
                let _ =
                    tokio::time::timeout(Duration::from_secs(2), child.wait_with_output()).await;
                panic!("hook exceeded cancellation deadline");
            }
        };
        let output =
            match tokio::time::timeout(Duration::from_secs(2), child.wait_with_output()).await {
                Ok(output) => output.unwrap(),
                Err(_) => {
                    self.kill_owned_consumer();
                    Self::kill_group(hook_pid);
                    panic!("hook output remained open after shell exit");
                }
            };
        assert_eq!(output.status, status);
        output
    }

    fn kill_group(pid: u32) {
        let _ = std::process::Command::new("/usr/bin/kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .status();
    }

    fn kill_owned_consumer(&self) {
        let Ok(record) = std::fs::read_to_string(self.root.join("consumer-process")) else {
            return;
        };
        let mut fields = record.split_whitespace();
        let (Some(pid), Some(start)) = (fields.next(), fields.next()) else {
            return;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return;
        };
        if stat.split_whitespace().nth(21) == Some(start) {
            if let Ok(pid) = pid.parse() {
                Self::kill_group(pid);
            }
        }
    }

    fn events(&self) -> String {
        std::fs::read_to_string(self.root.join("events")).unwrap_or_default()
    }

    fn translated(&self) -> usize {
        self.events()
            .lines()
            .filter(|line| line.starts_with("installed\t") || line.starts_with("legacy\t"))
            .count()
    }

    fn assert_translation(&self) {
        assert_eq!(self.translated(), 1, "translation phase must actually run");
        let paths = std::fs::read_to_string(self.root.join("consumer-paths")).unwrap();
        let mut paths = paths.lines();
        let _runtime_home = paths.next().unwrap();
        let output = paths.next().unwrap();
        let source = paths.next().unwrap();
        let expected = format!(
            "{}\ttranslate-doc\t{}\t-l\tzh\t--output\t{}",
            if self.installed {
                "installed"
            } else {
                "legacy"
            },
            source,
            output
        );
        assert!(self.events().lines().any(|line| line == expected));
        assert_eq!(
            std::fs::read_to_string(self.root.join("translated-source.md")).unwrap(),
            SOURCE
        );
    }

    fn workspace(&self) -> PathBuf {
        let paths = std::fs::read_to_string(self.root.join("consumer-paths")).unwrap();
        Path::new(paths.lines().next().unwrap())
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn assert_clean(&self) {
        for entry in std::fs::read_dir(&self.root).unwrap() {
            let name = entry.unwrap().file_name();
            let name = name.to_str().unwrap();
            assert!(
                !name.starts_with("hymt-readme-"),
                "private workspace leaked"
            );
            assert!(
                !name.starts_with(".README.zh-cn.md.hymt-"),
                "publication temp leaked"
            );
        }
        assert_eq!(
            std::fs::read_dir(self.root.join(".git")).unwrap().count(),
            0
        );
    }

    fn commits(&self) -> usize {
        self.events()
            .lines()
            .filter(|line| line.starts_with("git\tcommit\t"))
            .count()
    }

    fn assert_no_followup(&self) {
        assert_eq!(self.commits(), 0);
        assert!(!self.events().contains("git\tstatus\t"));
        assert!(!self.events().contains("git\tadd\t"));
        assert_eq!(
            std::fs::read_to_string(self.root.join("index-sentinel")).unwrap(),
            "unrelated staged bytes"
        );
        self.assert_clean();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

#[tokio::test]
async fn readme_helper_preserves_selected_translator_failures() {
    for installed in [true, false] {
        for code in ["42", "127"] {
            let fixture = Fixture::new(installed, !installed);
            let result = fixture
                .run(false, &[("FIXTURE_TRANSLATE_EXIT", code)])
                .await;
            fixture.assert_translation();
            assert_eq!(
                result.status.code(),
                Some(code.parse().unwrap()),
                "{result:?}"
            );
            let stderr = String::from_utf8(result.stderr).unwrap();
            assert!(stderr.contains(&format!("translation failed (exit {code})")));
            assert!(!stderr.contains("translator unavailable"));
            fixture.assert_clean();
        }
    }
}

#[tokio::test]
async fn readme_auto_hook_warns_and_stops_after_translation_failure() {
    for installed in [true, false] {
        for code in ["42", "127"] {
            let fixture = Fixture::new(installed, !installed);
            let result = fixture.run(true, &[("FIXTURE_TRANSLATE_EXIT", code)]).await;
            fixture.assert_translation();
            assert!(result.status.success(), "{result:?}");
            let stderr = String::from_utf8(result.stderr).unwrap();
            assert!(stderr.contains(&format!("translation failed (exit {code})")));
            assert!(!stderr.contains("already in sync"));
            assert!(!stderr.contains("translator unavailable"));
            fixture.assert_no_followup();
        }
    }
    for installed in [false, true] {
        let fixture = Fixture::new(installed, false);
        let result = fixture.run(true, &[("FIXTURE_CAPABLE", "0")]).await;
        assert!(result.status.success());
        assert_eq!(fixture.translated(), 0);
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains("translator unavailable"));
        assert!(!stderr.contains("already in sync"));
        fixture.assert_no_followup();
        let result = fixture.run(false, &[("FIXTURE_CAPABLE", "0")]).await;
        assert_eq!(result.status.code(), Some(127));
        assert_eq!(fixture.translated(), 0);
        assert!(String::from_utf8(result.stderr)
            .unwrap()
            .contains("translator unavailable"));
    }
}

#[tokio::test]
async fn readme_hook_preparation_failures_stop_before_translation() {
    for phase in ["mkdir", "copy"] {
        let fixture = Fixture::new(true, false);
        let result = fixture.run(false, &[("FIXTURE_PREP_FAIL", phase)]).await;
        assert!(
            fixture.events().contains(phase),
            "failure phase must be reached"
        );
        assert_eq!(result.status.code(), Some(42), "{result:?}");
        assert_eq!(fixture.translated(), 0);
        assert!(!fixture.events().contains("help\n"));
        assert!(String::from_utf8(result.stderr)
            .unwrap()
            .contains("runtime preparation failed"));
        let fixture = Fixture::new(true, false);
        let result = fixture.run(true, &[("FIXTURE_PREP_FAIL", phase)]).await;
        assert!(result.status.success(), "{result:?}");
        assert_eq!(fixture.translated(), 0);
        assert!(String::from_utf8(result.stderr)
            .unwrap()
            .contains("runtime preparation failed"));
        fixture.assert_no_followup();
    }
    for phase in ["mktemp", "snapshot"] {
        let fixture = Fixture::new(true, false);
        let result = fixture
            .run(
                true,
                &[("FIXTURE_PREP_FAIL", phase), ("FIXTURE_ENGLISH_DIRTY", "1")],
            )
            .await;
        assert!(result.status.success(), "{result:?}");
        let phase_marker = if phase == "snapshot" {
            "git\tshow\tfixture-head:README.md"
        } else {
            "mktemp\n"
        };
        assert!(fixture.events().contains(phase_marker));
        assert_eq!(fixture.translated(), 0);
        assert!(String::from_utf8(result.stderr)
            .unwrap()
            .contains("snapshot preparation failed"));
        fixture.assert_no_followup();
    }
    let fixture = Fixture::new(true, false);
    let result = fixture
        .run(true, &[("FIXTURE_PREP_FAIL", "followup-mkdir")])
        .await;
    assert!(result.status.success(), "{result:?}");
    fixture.assert_translation();
    assert_eq!(fixture.commits(), 0);
    assert!(String::from_utf8(result.stderr)
        .unwrap()
        .contains("commit preparation failed"));
}

#[tokio::test]
async fn readme_hook_success_and_guards_preserve_git_state() {
    for installed in [true, false] {
        for result in [NEW_CHINESE, OLD_CHINESE] {
            let fixture = Fixture::new(installed, !installed);
            let output = fixture.run(true, &[("FIXTURE_RESULT", result)]).await;
            assert!(output.status.success(), "{output:?}");
            fixture.assert_translation();
            assert_eq!(fixture.commits(), usize::from(result != OLD_CHINESE));
            if result == OLD_CHINESE {
                assert!(String::from_utf8(output.stderr)
                    .unwrap()
                    .contains("already in sync"));
            } else {
                assert!(fixture.events().contains("git\tcommit\t--only\t-m\tdocs(readme): auto-translate README.zh-cn.md\t--\tREADME.zh-cn.md\n"));
            }
            assert_eq!(
                std::fs::read_to_string(fixture.root.join("index-sentinel")).unwrap(),
                "unrelated staged bytes"
            );
            fixture.assert_clean();
        }
    }
    for guard in [
        ("FIXTURE_CHANGED", "other.txt"),
        ("FIXTURE_CHANGED", "README.md\nREADME.zh-cn.md"),
        ("FIXTURE_CHINESE_DIRTY", "1"),
        ("FIXTURE_CACHED_DIRTY", "1"),
    ] {
        let fixture = Fixture::new(true, false);
        let output = fixture.run(true, &[guard]).await;
        assert!(output.status.success(), "{output:?}");
        assert_eq!(fixture.translated(), 0);
        assert!(!fixture.events().contains("help\n"));
        assert!(!fixture.events().contains("mktemp\n"));
        fixture.assert_no_followup();
    }
    let fixture = Fixture::new(true, false);
    std::fs::write(fixture.root.join("README.md"), "Uncommitted English.\n").unwrap();
    let output = fixture.run(true, &[("FIXTURE_ENGLISH_DIRTY", "1")]).await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fixture.translated(), 1);
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("translated-source.md")).unwrap(),
        SOURCE
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("README.md")).unwrap(),
        "Uncommitted English.\n"
    );
    fixture.assert_clean();
}

#[tokio::test]
async fn readme_hook_uses_committed_english_when_newer_english_is_staged() {
    let fixture = Fixture::new(true, false);
    std::fs::write(fixture.root.join("README.md"), "Newer staged English.\n").unwrap();
    let result = fixture.run(true, &[("FIXTURE_ENGLISH_STAGED", "1")]).await;
    assert!(result.status.success(), "{result:?}");
    assert_eq!(fixture.translated(), 1);
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("translated-source.md")).unwrap(),
        SOURCE
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("README.md")).unwrap(),
        "Newer staged English.\n"
    );
    fixture.assert_clean();
}

#[tokio::test]
async fn readme_hook_preserves_chinese_changed_during_translation() {
    for change in ["edit", "stage"] {
        let fixture = Fixture::new(true, false);
        let result = fixture
            .run(true, &[("FIXTURE_CHINESE_CONCURRENT", change)])
            .await;
        assert!(result.status.success(), "{change}: {result:?}");
        assert_eq!(fixture.translated(), 1);
        assert_eq!(fixture.commits(), 0, "{change}");
        if change == "edit" {
            assert_eq!(
                std::fs::read_to_string(fixture.root.join("README.zh-cn.md")).unwrap(),
                "Chinese edited during translation.\n"
            );
        } else {
            assert_eq!(
                std::fs::read_to_string(fixture.root.join("staged-chinese")).unwrap(),
                "Chinese staged during translation.\n"
            );
            assert_eq!(
                std::fs::read_to_string(fixture.root.join("README.zh-cn.md")).unwrap(),
                OLD_CHINESE
            );
        }
        fixture.assert_clean();
    }
}

#[tokio::test]
async fn readme_hook_preserves_real_git_index_staged_during_translation() {
    let fixture = Fixture::new(true, false);
    fixture.use_real_git();

    let result = fixture
        .run(true, &[("FIXTURE_CHINESE_CONCURRENT", "real-stage")])
        .await;
    assert!(result.status.success(), "{result:?}");
    assert_eq!(fixture.translated(), 1);
    assert_eq!(
        fixture.git(&["show", ":README.zh-cn.md"]),
        b"Chinese staged during translation.\n"
    );
    assert_eq!(
        fixture.git(&["show", "HEAD:README.zh-cn.md"]),
        OLD_CHINESE.as_bytes()
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("README.zh-cn.md")).unwrap(),
        "Chinese staged during translation.\n"
    );
    assert_eq!(
        String::from_utf8(fixture.git(&["rev-list", "--count", "HEAD"])).unwrap(),
        "2\n"
    );
}

#[tokio::test]
async fn readme_helper_from_subdirectory_preserves_staged_root_destination() {
    let fixture = Fixture::new(true, false);
    fixture.use_real_git();
    std::fs::write(
        fixture.root.join("README.zh-cn.md"),
        "Staged Chinese update.\n",
    )
    .unwrap();
    fixture.git(&["add", "README.zh-cn.md"]);
    std::fs::create_dir_all(fixture.root.join("crates/example")).unwrap();
    let result = fixture
        .run_from(false, &[], &fixture.root.join("crates/example"))
        .await;
    assert!(
        !result.status.success(),
        "staged destination must block publication: {result:?}"
    );
    assert_eq!(fixture.translated(), 1);
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("README.zh-cn.md")).unwrap(),
        "Staged Chinese update.\n"
    );
    assert_eq!(
        fixture.git(&["show", ":README.zh-cn.md"]),
        b"Staged Chinese update.\n"
    );
    assert_eq!(
        fixture.git(&["show", "HEAD:README.zh-cn.md"]),
        OLD_CHINESE.as_bytes()
    );
    assert_eq!(fixture.git(&["rev-list", "--count", "HEAD"]), b"2\n");
    assert!(!fixture.workspace().exists());
}

#[tokio::test]
async fn readme_hook_rejects_head_advance_and_same_commit_branch_switch() {
    for change in ["switch", "advance"] {
        let fixture = Fixture::new(true, false);
        fixture.use_real_git();
        let result = fixture.run(true, &[("FIXTURE_HEAD_CHANGE", change)]).await;
        assert!(result.status.success(), "{change}: {result:?}");
        assert_eq!(fixture.translated(), 1);
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("translated-source.md")).unwrap(),
            "Committed English update.\n"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("README.zh-cn.md")).unwrap(),
            OLD_CHINESE,
            "{change}: stale translation published"
        );
        assert_eq!(
            fixture.git(&["show", ":README.zh-cn.md"]),
            OLD_CHINESE.as_bytes()
        );
        assert_eq!(
            fixture.git(&["show", "HEAD:README.zh-cn.md"]),
            OLD_CHINESE.as_bytes()
        );
        assert_eq!(
            String::from_utf8(fixture.git(&["rev-list", "--count", "HEAD"])).unwrap(),
            if change == "advance" { "3\n" } else { "2\n" }
        );
        if change == "switch" {
            assert_eq!(fixture.git(&["branch", "--show-current"]), b"later\n");
        }
        assert!(!fixture.workspace().exists());
    }
}

#[tokio::test]
async fn readme_hook_rejects_head_advance_before_followup_commit() {
    let fixture = Fixture::new(true, false);
    fixture.use_real_git();
    let result = fixture
        .run(true, &[("FIXTURE_HEAD_CHANGE", "after-publication")])
        .await;
    assert!(result.status.success(), "{result:?}");
    assert_eq!(fixture.translated(), 1);
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("README.zh-cn.md")).unwrap(),
        NEW_CHINESE
    );
    assert_eq!(
        fixture.git(&["show", ":README.zh-cn.md"]),
        OLD_CHINESE.as_bytes()
    );
    assert_eq!(
        fixture.git(&["show", "HEAD:README.zh-cn.md"]),
        OLD_CHINESE.as_bytes()
    );
    assert_eq!(
        String::from_utf8(fixture.git(&["rev-list", "--count", "HEAD"])).unwrap(),
        "3\n"
    );
    assert!(!fixture.workspace().exists());
}

#[tokio::test]
async fn readme_hook_preserves_destination_mode_changed_during_translation() {
    let fixture = Fixture::new(true, false);
    fixture.use_real_git();
    let chinese = fixture.root.join("README.zh-cn.md");
    std::fs::set_permissions(&chinese, std::fs::Permissions::from_mode(0o644)).unwrap();
    let result = fixture
        .run(true, &[("FIXTURE_CHINESE_CONCURRENT", "chmod")])
        .await;
    assert!(result.status.success(), "{result:?}");
    assert_eq!(fixture.translated(), 1);
    assert_eq!(std::fs::read_to_string(&chinese).unwrap(), OLD_CHINESE);
    assert_eq!(
        std::fs::metadata(&chinese).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fixture.git(&["show", ":README.zh-cn.md"]),
        OLD_CHINESE.as_bytes()
    );
    assert_eq!(
        String::from_utf8(fixture.git(&["rev-list", "--count", "HEAD"])).unwrap(),
        "2\n"
    );
    assert!(!fixture.workspace().exists());
}

#[tokio::test]
async fn readme_hook_followup_git_failure_is_not_a_translation_skip() {
    let fixture = Fixture::new(true, false);
    let result = fixture.run(true, &[("FIXTURE_COMMIT_EXIT", "68")]).await;
    fixture.assert_translation();
    assert_eq!(fixture.commits(), 1);
    assert_eq!(result.status.code(), Some(68), "{result:?}");
    assert!(!String::from_utf8(result.stderr)
        .unwrap()
        .contains("translation failed"));
    fixture.assert_clean();
}
#[tokio::test]
async fn readme_hook_failed_partial_candidate_preserves_previous_output() {
    for installed in [true, false] {
        let fixture = Fixture::new(installed, !installed);
        std::fs::set_permissions(
            fixture.root.join("README.zh-cn.md"),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        let result = fixture
            .run(
                false,
                &[("FIXTURE_PARTIAL", "1"), ("FIXTURE_TRANSLATE_EXIT", "42")],
            )
            .await;
        fixture.assert_translation();
        assert!(fixture.events().contains("partial-written\n"));
        assert_eq!(result.status.code(), Some(42), "{result:?}");
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("README.zh-cn.md")).unwrap(),
            OLD_CHINESE,
            "failed translator must not replace the previous README"
        );
        assert_eq!(
            std::fs::metadata(fixture.root.join("README.zh-cn.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        fixture.assert_clean();
    }
}
#[tokio::test]
async fn readme_hook_private_workspace_lasts_through_followup_then_cleans() {
    let fixture = Fixture::new(true, false);
    let result = fixture.run(true, &[("FIXTURE_ENGLISH_DIRTY", "1")]).await;
    assert_eq!(
        fixture.translated(),
        1,
        "translation phase must actually run"
    );
    let paths = std::fs::read_to_string(fixture.root.join("consumer-paths")).unwrap();
    let paths: Vec<_> = paths.lines().map(PathBuf::from).collect();
    let workspace = paths[0].parent().unwrap();
    assert!(
        workspace
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("hymt-readme-"),
        "runtime must live in an exclusive private workspace, not .git"
    );
    assert!(result.status.success(), "{result:?}");
    assert!(paths[1].starts_with(workspace));
    assert!(paths[2].starts_with(workspace));
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("consumer-modes")).unwrap(),
        "700\n700\n700\n700\n600\n600\n600\n"
    );
    assert!(
        fixture.events().contains("followup-alive\t"),
        "{}\n{}",
        fixture.events(),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !workspace.exists(),
        "workspace cleanup must follow the final consumer"
    );
    assert_eq!(
        std::fs::read_dir(fixture.root.join(".git"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("home/.config/hymt/config.toml")).unwrap(),
        "fixture configuration\n"
    );
}
#[tokio::test]
async fn readme_hook_failure_and_cancellation_clean_owned_resources() {
    for (phase, marker) in [
        ("workspace", "mktemp\n"),
        ("mkdir", "mkdir\t"),
        ("copy", "copy\n"),
        ("mktemp", "mktemp\n"),
        ("snapshot", "git\tshow\tfixture-head:README.md"),
        ("candidate", "mktemp\n"),
        ("publish-mktemp", "installed\t"),
        ("publish-copy", "publication-partial\n"),
        ("publish-chmod", "publication-mode\n"),
        ("publish-mv", "publication-rename\n"),
        ("followup-mkdir", "mkdir\t"),
    ] {
        let fixture = Fixture::new(true, false);
        let result = fixture
            .run(
                true,
                &[("FIXTURE_PREP_FAIL", phase), ("FIXTURE_ENGLISH_DIRTY", "1")],
            )
            .await;
        assert!(
            fixture.events().contains(marker),
            "{phase}: failure phase was not reached"
        );
        assert!(result.status.success(), "{phase}: {result:?}");
        assert_eq!(fixture.commits(), 0, "{phase}");
        assert!(!String::from_utf8_lossy(&result.stderr).contains("already in sync"));
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("README.zh-cn.md")).unwrap(),
            if phase == "followup-mkdir" {
                NEW_CHINESE
            } else {
                OLD_CHINESE
            },
            "{phase}"
        );
        fixture.assert_clean();
    }
    for phase in ["probe", "translation", "commit"] {
        let fixture = Fixture::new(true, false);
        let result = fixture.run(true, &[("FIXTURE_CANCEL", phase)]).await;
        assert!(fixture.events().contains(&format!("{phase}-cancel\n")));
        assert_eq!(result.status.code(), Some(143), "{result:?}");
        assert_eq!(fixture.translated(), usize::from(phase != "probe"));
        assert_eq!(fixture.commits(), usize::from(phase == "commit"));
        assert!(
            fixture.root.join("consumer-settled").exists(),
            "{phase}: consumer did not settle"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("README.zh-cn.md")).unwrap(),
            if phase == "commit" {
                NEW_CHINESE
            } else {
                OLD_CHINESE
            }
        );
        fixture.assert_clean();
    }
}

#[tokio::test]
async fn readme_hook_preserves_foreign_replacements() {
    for phase in [
        "workspace",
        "config",
        "destination",
        "publication",
        "publication-mode",
        "candidate",
    ] {
        let fixture = Fixture::new(true, false);
        let result = fixture.run(true, &[("FIXTURE_REPLACE", phase)]).await;
        assert_eq!(
            fixture.translated(),
            1,
            "replacement must occur during the real consumer phase"
        );
        assert!(result.status.success(), "{phase}: {result:?}");
        assert_eq!(fixture.commits(), 0);
        let foreign = match phase {
            "workspace" => fixture.workspace().join("foreign"),
            "candidate" => {
                assert!(fixture.events().contains("candidate-foreign\n"));
                assert!(String::from_utf8_lossy(&result.stderr)
                    .contains("translation failed (exit 42)"));
                let paths = std::fs::read_to_string(fixture.root.join("consumer-paths")).unwrap();
                PathBuf::from(paths.lines().nth(1).unwrap())
            }
            "config" => fixture
                .workspace()
                .join("hymt-home/.config/hymt/config.toml"),
            "destination" => fixture.root.join("README.zh-cn.md"),
            "publication" | "publication-mode" => {
                let events = fixture.events();
                PathBuf::from(
                    events
                        .lines()
                        .find_map(|line| line.strip_prefix("publication-copy\t"))
                        .unwrap(),
                )
            }
            _ => unreachable!(),
        };
        assert!(
            foreign.exists(),
            "{phase}: foreign replacement must survive cleanup"
        );
        assert_eq!(std::fs::read_to_string(foreign).unwrap(), "foreign");
        if phase != "destination" {
            assert_eq!(
                std::fs::read_to_string(fixture.root.join("README.zh-cn.md")).unwrap(),
                OLD_CHINESE
            );
        }
        if phase == "destination" || phase.starts_with("publication") {
            assert!(
                !fixture.workspace().exists(),
                "output replacement must not retain runtime configuration"
            );
        }
    }
}

#[tokio::test]
async fn readme_hook_admits_directory_symlinks_and_rejects_unsafe_paths() {
    let fixture = Fixture::new(true, false);
    let temp_link = fixture.root.join("temp-link");
    let repo_link = fixture.root.join("repo-link");
    std::os::unix::fs::symlink(&fixture.root, &temp_link).unwrap();
    std::os::unix::fs::symlink(&fixture.root, &repo_link).unwrap();
    let config = fixture.root.join("home/.config/hymt/config.toml");
    let original = fixture.root.join("home/original-config");
    std::fs::rename(&config, &original).unwrap();
    std::os::unix::fs::symlink(&original, &config).unwrap();
    let result = fixture
        .run(
            true,
            &[
                ("TMPDIR", temp_link.to_str().unwrap()),
                ("FIXTURE_ROOT", repo_link.to_str().unwrap()),
            ],
        )
        .await;
    assert!(result.status.success(), "{result:?}");
    assert_eq!(fixture.translated(), 1);
    assert!(temp_link.is_symlink() && repo_link.is_symlink() && config.is_symlink());
    assert_eq!(
        std::fs::read_to_string(&original).unwrap(),
        "fixture configuration\n"
    );
    fixture.assert_clean();

    for fault in ["mode", "ancestor", "device", "owner", "git"] {
        let fixture = Fixture::new(true, false);
        let mut changes = vec![];
        let temp = fixture.root.join("unsafe/temp");
        let git_dir = fixture.root.join(".git");
        if fault == "mode" {
            std::fs::set_permissions(&fixture.root, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        } else if fault == "git" {
            changes.push(("TMPDIR", git_dir.to_str().unwrap()));
        } else if fault == "ancestor" {
            std::fs::create_dir_all(&temp).unwrap();
            std::fs::set_permissions(
                temp.parent().unwrap(),
                std::fs::Permissions::from_mode(0o777),
            )
            .unwrap();
            changes.push(("TMPDIR", temp.to_str().unwrap()));
        } else {
            changes.push(("FIXTURE_ROOT_FAULT", fault));
        }
        let result = fixture.run(true, &changes).await;
        assert!(result.status.success(), "{fault}: {result:?}");
        assert_eq!(fixture.translated(), 0, "{fault}");
        assert!(!fixture.events().contains("mktemp\n"));
        if fault == "device" || fault == "owner" {
            assert!(fixture.events().contains(&format!("root-fault\t{fault}\n")));
        }
        assert!(String::from_utf8_lossy(&result.stderr).contains("runtime preparation failed"));
        fixture.assert_clean();
    }
    for kind in [
        "symlink",
        "directory",
        "special-mode",
        "sticky-special-mode",
    ] {
        let fixture = Fixture::new(true, false);
        let output = fixture.root.join("README.zh-cn.md");
        if kind == "special-mode" || kind == "sticky-special-mode" {
            let mode = if kind == "special-mode" {
                0o4644
            } else {
                0o1400
            };
            std::fs::set_permissions(&output, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                std::fs::metadata(&output).unwrap().permissions().mode() & 0o7777,
                mode
            );
        } else {
            std::fs::remove_file(&output).unwrap();
            if kind == "symlink" {
                std::os::unix::fs::symlink(fixture.root.join("chinese-baseline"), &output).unwrap();
            } else {
                std::fs::create_dir(&output).unwrap();
            }
        }
        let result = fixture.run(true, &[]).await;
        assert!(result.status.success(), "{kind}: {result:?}");
        assert_eq!(fixture.translated(), 0);
        assert!(String::from_utf8_lossy(&result.stderr).contains("output preparation failed"));
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("chinese-baseline")).unwrap(),
            OLD_CHINESE
        );
        assert_eq!(output.is_symlink(), kind == "symlink");
        if kind == "special-mode" || kind == "sticky-special-mode" {
            assert_eq!(std::fs::read_to_string(&output).unwrap(), OLD_CHINESE);
            assert_eq!(
                std::fs::metadata(&output).unwrap().permissions().mode() & 0o7777,
                if kind == "special-mode" {
                    0o4644
                } else {
                    0o1400
                }
            );
        }
        fixture.assert_clean();
    }
}

#[tokio::test]
async fn readme_hook_publication_preserves_modes_and_new_file_umask() {
    for mode in [None, Some(0o644), Some(0o600), Some(0o640), Some(0o444)] {
        let fixture = Fixture::new(true, false);
        let output = fixture.root.join("README.zh-cn.md");
        if let Some(mode) = mode {
            std::fs::set_permissions(&output, std::fs::Permissions::from_mode(mode)).unwrap();
        } else {
            std::fs::remove_file(&output).unwrap();
        }
        let atomic = if mode == Some(0o600) { "1" } else { "0" };
        let result = fixture.run(false, &[("FIXTURE_ATOMIC", atomic)]).await;
        fixture.assert_translation();
        assert_eq!(fixture.events().contains("atomic-writer\n"), atomic == "1");
        assert!(result.status.success(), "{mode:?}: {result:?}");
        assert_eq!(std::fs::read_to_string(&output).unwrap(), NEW_CHINESE);
        assert_eq!(
            std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            mode.unwrap_or(0o644)
        );
        let events = fixture.events();
        let temp = Path::new(
            events
                .lines()
                .find_map(|line| line.strip_prefix("publication-copy\t"))
                .unwrap(),
        );
        assert_eq!(temp.parent().unwrap(), fixture.root);
        assert_ne!(temp.parent().unwrap(), fixture.workspace());
        assert!(events.contains("publication-rename\n"));
        fixture.assert_clean();
    }
}
