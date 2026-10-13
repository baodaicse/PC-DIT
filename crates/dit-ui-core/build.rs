//! The DIT-01 version shown on every screen (`DIT_VERSION`, see `VERSION`). Image builds pass
//! `tools/dit-version.sh`'s answer in the environment; otherwise the script is asked here.

use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=DIT_VERSION");
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default()).join("../..");
    // A commit or checkout moves the number: rebuild when one happens.
    for watched in [
        "VERSION",
        "tools/dit-version.sh",
        ".git/HEAD",
        ".git/index",
        ".git/logs/HEAD",
    ] {
        let path = root.join(watched);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    let version = env::var("DIT_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            Command::new("bash")
                .arg(root.join("tools/dit-version.sh"))
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
        })
        .map_or_else(|| "unknown".to_owned(), |value| value.trim().to_owned());
    println!("cargo:rustc-env=DIT_VERSION={version}");
}
