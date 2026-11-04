use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let revision = if root.join(".git").exists() {
        let mut references = vec!["HEAD".to_owned(), "packed-refs".to_owned()];
        references.extend(git(&root, &["symbolic-ref", "--quiet", "HEAD"]));
        for reference in references {
            if let Some(path) = git(&root, &["rev-parse", "--git-path", &reference]) {
                let path = root.join(path);
                if path.exists() || reference != "packed-refs" {
                    println!("cargo:rerun-if-changed={}", path.display());
                }
            }
        }
        git(&root, &["rev-parse", "--verify", "HEAD"])
    } else {
        None
    };
    println!(
        "cargo:rustc-env=HYMT_VERSION={} (git {})",
        env!("CARGO_PKG_VERSION"),
        revision.as_deref().unwrap_or("unknown")
    );
}
