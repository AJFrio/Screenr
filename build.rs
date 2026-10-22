use std::env;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // On Ubuntu upgrades (e.g. 24.04 -> 26.04), libdbus-1.so.3 may be installed
    // without the unversioned libdbus-1.so development symlink needed by the linker.
    let candidates = [
        "/usr/lib/x86_64-linux-gnu/libdbus-1.so",
        "/lib/x86_64-linux-gnu/libdbus-1.so",
        "/usr/lib/libdbus-1.so",
    ];
    if candidates.iter().any(|p| Path::new(p).exists()) {
        return;
    }

    let versioned_candidates = [
        "/usr/lib/x86_64-linux-gnu/libdbus-1.so.3",
        "/lib/x86_64-linux-gnu/libdbus-1.so.3",
        "/usr/lib/libdbus-1.so.3",
    ];
    if let Some(target) = versioned_candidates.iter().find(|p| Path::new(p).exists()) {
        if let Ok(out_dir) = env::var("OUT_DIR") {
            let link_path = PathBuf::from(&out_dir).join("libdbus-1.so");
            let _ = std::fs::remove_file(&link_path);
            if symlink(target, &link_path).is_ok() {
                println!("cargo:rustc-link-search=native={}", out_dir);
            }
        }
    }
}
