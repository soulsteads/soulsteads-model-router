use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn sources(directory: &Path, paths: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("source directory") {
        let path = entry.expect("source entry").path();
        if path.is_dir() {
            sources(&path, paths);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            paths.push(path);
        }
    }
}
fn main() {
    let mut paths = vec![
        PathBuf::from("Cargo.toml"),
        PathBuf::from("Cargo.lock"),
        PathBuf::from("build.rs"),
    ];
    sources(Path::new("src"), &mut paths);
    paths.sort();
    let mut hash = Sha256::new();
    println!("cargo:rerun-if-changed=src");
    let mut git_refs = vec!["HEAD".to_owned(), "packed-refs".to_owned()];
    if let Ok(output) = Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        && output.status.success()
        && let Ok(reference) = String::from_utf8(output.stdout)
    {
        git_refs.push(reference.trim().to_owned());
    }
    for reference in git_refs {
        if let Ok(output) = Command::new("git")
            .args(["rev-parse", "--git-path", &reference])
            .output()
            && output.status.success()
            && let Ok(path) = String::from_utf8(output.stdout)
        {
            println!("cargo:rerun-if-changed={}", path.trim());
        }
    }
    for path in paths {
        println!("cargo:rerun-if-changed={}", path.display());
        hash.update(path.to_str().expect("UTF-8 source path").as_bytes());
        hash.update([0]);
        hash.update(fs::read(path).expect("source bytes"));
        hash.update([0]);
    }
    println!(
        "cargo:rustc-env=SLOTLINE_SOURCE_SHA256={:x}",
        hash.finalize()
    );
    let revision = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .unwrap_or_else(|| "unavailable".into());
    let dirty = Command::new("git")
        .args([
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--",
            "src",
            "build.rs",
            "Cargo.toml",
            "Cargo.lock",
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or("unknown", |output| {
            if output.stdout.is_empty() {
                "clean"
            } else {
                "dirty"
            }
        });
    println!(
        "cargo:rustc-env=SLOTLINE_SOURCE_REVISION={}",
        revision.trim()
    );
    println!("cargo:rustc-env=SLOTLINE_SOURCE_STATE={dirty}");
}
