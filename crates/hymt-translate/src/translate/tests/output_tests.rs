use super::*;

const OLD_OUTPUT: &str = "The complete original output must survive a failed replacement.";

#[tokio::test]
async fn new_output_mode_uses_the_isolated_child_umask() {
    use std::os::unix::fs::MetadataExt;
    if let Some(root) = std::env::var_os("HYMT_OUTPUT_UMASK_CHILD") {
        let output = PathBuf::from(root).join("output");
        write_translation_output(&output, "complete output")
            .await
            .unwrap();
        assert_eq!(std::fs::metadata(output).unwrap().mode() & 0o777, 0o640);
        return;
    }
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new("/usr/bin/python3")
        .args([
            "-B",
            "-c",
            "import os,sys; os.umask(0o027); os.execv(sys.argv[1],sys.argv[1:])",
        ])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "translate::tests::output_tests::new_output_mode_uses_the_isolated_child_umask",
            "--nocapture",
        ])
        .env("HYMT_OUTPUT_UMASK_CHILD", root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let status = match tokio::time::timeout(Duration::from_secs(30), child.wait()).await {
        Ok(status) => status.unwrap(),
        Err(_) => {
            child.kill().await.unwrap();
            panic!("isolated umask child killed and reaped after deadline");
        }
    };
    let output = child.wait_with_output().await.unwrap();
    assert!(status.success(), "isolated umask child failed: {output:?}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("output")).unwrap(),
        "complete output"
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn complete_output_replacement_preserves_modes_and_links() {
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("output");
    std::fs::write(&output, OLD_OUTPUT).unwrap();
    std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o640)).unwrap();
    std::fs::hard_link(&output, root.path().join("alias")).unwrap();
    let old_inode = std::fs::metadata(&output).unwrap().ino();
    write_translation_output(&output, "complete replacement")
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&output).unwrap(),
        "complete replacement"
    );
    assert_ne!(std::fs::metadata(&output).unwrap().ino(), old_inode);
    assert_eq!(std::fs::metadata(&output).unwrap().mode() & 0o777, 0o640);
    assert_eq!(
        std::fs::read_to_string(root.path().join("alias")).unwrap(),
        OLD_OUTPUT
    );
    let leaf = root.path().join("leaf");
    symlink("output", &leaf).unwrap();
    let link_inode = std::fs::symlink_metadata(&leaf).unwrap().ino();
    write_translation_output(&leaf, "leaf target replacement")
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&output).unwrap(),
        "leaf target replacement"
    );
    assert_eq!(std::fs::read_link(&leaf).unwrap(), PathBuf::from("output"));
    assert_eq!(std::fs::symlink_metadata(&leaf).unwrap().ino(), link_inode);
    let directory_link = root.path().join("directory-link");
    let directory = root.path().join("directory");
    std::fs::create_dir(&directory).unwrap();
    symlink(&directory, &directory_link).unwrap();
    write_translation_output(&directory_link.join("nested/new"), "new output")
        .await
        .unwrap();
    assert!(std::fs::symlink_metadata(directory_link)
        .unwrap()
        .is_symlink());
    assert_eq!(
        std::fs::read_to_string(directory.join("nested/new")).unwrap(),
        "new output"
    );
    assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".hymt-output-")));
}

#[tokio::test]
async fn output_rejects_dangling_nonregular_and_unwritable_destinations() {
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    use std::os::unix::net::UnixListener;
    assert_ne!(
        std::fs::metadata("/proc/self").unwrap().uid(),
        0,
        "permission checks require an ordinary user"
    );
    let root = tempfile::tempdir().unwrap();
    let dangling = root.path().join("dangling");
    symlink("missing", &dangling).unwrap();
    let directory = root.path().join("directory");
    std::fs::create_dir(&directory).unwrap();
    let socket = root.path().join("socket");
    let _socket = UnixListener::bind(&socket).unwrap();
    let socket_link = root.path().join("socket-link");
    symlink(&socket, &socket_link).unwrap();
    for path in [&dangling, &directory, &socket, &socket_link] {
        write_translation_output(path, "replacement")
            .await
            .expect_err("unsafe leaf must reject without opening it");
    }
    assert_eq!(
        std::fs::read_link(&dangling).unwrap(),
        PathBuf::from("missing")
    );
    assert!(!root.path().join("missing").exists());
    assert!(directory.is_dir());
    assert!(std::fs::symlink_metadata(&socket_link)
        .unwrap()
        .is_symlink());

    let readonly = root.path().join("readonly");
    std::fs::write(&readonly, OLD_OUTPUT).unwrap();
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o400)).unwrap();
    let error = write_translation_output(&readonly, "replacement")
        .await
        .unwrap_err();
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(error.chain().any(|cause| cause
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)));
    assert_eq!(std::fs::read_to_string(&readonly).unwrap(), OLD_OUTPUT);
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o500)).unwrap();
    let result = write_translation_output(&directory.join("output"), "replacement").await;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.unwrap_err().chain().any(|cause| cause
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)));
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 5);
}

#[test]
fn cancelled_output_write_removes_only_its_owned_temporary() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("output");
        std::fs::write(&output, OLD_OUTPUT).unwrap();
        std::fs::hard_link(&output, root.path().join("alias")).unwrap();
        std::fs::write(root.path().join("foreign"), "foreign sentinel").unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            started_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(15));
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut write = Box::pin(write_translation_output(&output, "pending translation"));
        assert!(matches!(
            write.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        let temporaries: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.unwrap();
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".hymt-output-")
                    .then(|| entry.path())
            })
            .collect();
        assert_eq!(
            temporaries.len(),
            1,
            "cancellation must reach actual owned-temp admission"
        );
        drop(write);
        assert!(!temporaries[0].exists());
        release_tx.send(()).unwrap();
        blocker.await.unwrap();
        assert_eq!(std::fs::read_to_string(&output).unwrap(), OLD_OUTPUT);
        assert_eq!(
            std::fs::read_to_string(root.path().join("alias")).unwrap(),
            OLD_OUTPUT
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("foreign")).unwrap(),
            "foreign sentinel"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 3);
    });
}

#[tokio::test]
async fn output_temporary_collisions_and_foreign_replacements_are_preserved() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("temporary");
    std::fs::write(&path, "foreign collision").unwrap();
    let error = OutputTemp::create(path.clone(), None)
        .err()
        .expect("exclusive create must reject the real collision");
    assert!(error.chain().any(|cause| cause
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists)));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "foreign collision");
    std::fs::remove_file(&path).unwrap();
    let (owner, writer) = OutputTemp::create(path.clone(), None).unwrap();
    drop(writer);
    std::fs::rename(&path, root.path().join("owned-inode")).unwrap();
    std::fs::write(&path, "foreign replacement").unwrap();
    drop(owner);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "foreign replacement"
    );
}

#[test]
fn existing_output_permissions_apply_before_pending_content_write() {
    use std::future::Future;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::process::Stdio;
    use std::task::{Context, Poll, Waker};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    if let Some(root) = std::env::var_os("HYMT_OUTPUT_PENDING_MODE_CHILD") {
        runtime.block_on(async {
            let root = PathBuf::from(root);
            let output = root.join("output");
            assert_eq!(std::fs::metadata(&root).unwrap().mode() & 0o777, 0o755);
            assert_eq!(std::fs::metadata(&output).unwrap().mode() & 0o777, 0o600);
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(15));
            });
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let mut write = Box::pin(write_translation_output(
                &output,
                "pending private translation",
            ));
            assert!(matches!(
                write.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                Poll::Pending
            ));
            let temporaries: Vec<_> = std::fs::read_dir(&root)
                .unwrap()
                .filter_map(|entry| {
                    let entry = entry.unwrap();
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".hymt-output-")
                        .then(|| entry.path())
                })
                .collect();
            assert_eq!(
                temporaries.len(),
                1,
                "must observe an admitted pending temporary"
            );
            let pending_mode = std::fs::metadata(&temporaries[0]).unwrap().mode() & 0o777;
            drop(write);
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
            assert!(!temporaries[0].exists());
            assert_eq!(std::fs::read_to_string(&output).unwrap(), OLD_OUTPUT);
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
            assert_eq!(
                pending_mode, 0o600,
                "pending temp must not expose broader permissions"
            );
        });
        return;
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = root.path().join("output");
    std::fs::write(&output, OLD_OUTPUT).unwrap();
    std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o600)).unwrap();
    runtime.block_on(async {
        let mut child = tokio::process::Command::new("/usr/bin/python3")
            .args(["-B", "-c", "import os,sys; os.umask(0o022); os.execv(sys.argv[1],sys.argv[1:])"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "translate::tests::output_tests::existing_output_permissions_apply_before_pending_content_write", "--nocapture"])
            .env("HYMT_OUTPUT_PENDING_MODE_CHILD", root.path())
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .kill_on_drop(true).spawn().unwrap();
        let status = match tokio::time::timeout(Duration::from_secs(30), child.wait()).await {
            Ok(status) => status.unwrap(),
            Err(_) => {
                child.kill().await.unwrap();
                panic!("pending-mode child killed and reaped after deadline");
            }
        };
        let output = child.wait_with_output().await.unwrap();
        assert!(status.success(), "isolated pending-mode child failed: {output:?}");
    });
    assert_eq!(std::fs::read_to_string(output).unwrap(), OLD_OUTPUT);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn partial_output_failure_preserves_destination_and_hard_links() {
    if let Some(root) = std::env::var_os("HYMT_OUTPUT_EFBIG_CHILD") {
        let root = PathBuf::from(root);
        let error = write_translation_output(&root.join("output"), &"translation".repeat(128))
            .await
            .expect_err("the isolated file-size limit must reject the output write");
        let io_error = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<std::io::Error>())
            .expect("the output write must return its actual OS error");
        assert_eq!(io_error.raw_os_error(), Some(27), "Linux EFBIG required");
        eprintln!("Output write returned Linux EFBIG (27)");
        for name in ["output", "source", "hard-link"] {
            assert_eq!(
                std::fs::read_to_string(root.join(name)).unwrap(),
                OLD_OUTPUT
            );
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 3);
        return;
    }

    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("output"), OLD_OUTPUT).unwrap();
    std::fs::write(root.path().join("source"), OLD_OUTPUT).unwrap();
    std::fs::hard_link(root.path().join("output"), root.path().join("hard-link")).unwrap();
    let mut child = tokio::process::Command::new("/usr/bin/python3")
        .args([
            "-B",
            "-c",
            "import os,resource,signal,sys; resource.setrlimit(resource.RLIMIT_FSIZE,(16,16)); signal.signal(signal.SIGXFSZ,signal.SIG_IGN); os.execv(sys.argv[1],sys.argv[1:])",
        ])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "translate::tests::output_tests::partial_output_failure_preserves_destination_and_hard_links",
            "--nocapture",
        ])
        .env("HYMT_OUTPUT_EFBIG_CHILD", root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let status = match tokio::time::timeout(Duration::from_secs(30), child.wait()).await {
        Ok(status) => status.unwrap(),
        Err(_) => {
            child.kill().await.unwrap();
            panic!("isolated output writer exceeded its deadline; child killed and reaped");
        }
    };
    let output = child.wait_with_output().await.unwrap();
    assert!(
        status.success(),
        "isolated output writer failed: {output:?}"
    );
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("Linux EFBIG (27)"));
    for name in ["output", "source", "hard-link"] {
        assert_eq!(
            std::fs::read_to_string(root.path().join(name)).unwrap(),
            OLD_OUTPUT
        );
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 3);
}
