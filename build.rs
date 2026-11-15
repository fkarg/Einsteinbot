use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    // Track only build inputs, including untracked source files. Other files do
    // not affect this build and cannot reliably trigger Cargo's build script.
    for path in ["src", "Cargo.toml", "Cargo.lock", "build.rs"] {
        println!("cargo:rerun-if-changed={path}");
    }
    for path in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(head) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&["rev-parse", "--git-path", &head]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(revision) = git(&["rev-parse", "HEAD"]) {
        println!("cargo:rustc-env=EINSTEINBOT_GIT_REVISION={revision}");
    }
    if let Some(status) = git(&[
        "status",
        "--porcelain",
        "--untracked-files=all",
        "--",
        "src",
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
    ]) {
        println!(
            "cargo:rustc-env=EINSTEINBOT_GIT_DIRTY={}",
            !status.is_empty()
        );
    }
}
