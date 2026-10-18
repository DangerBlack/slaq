//! Generates the Rust types from the Chime signaling schema.
//!
//! `protox` is used instead of `protoc` so that the build does not depend on
//! an external binary installed by hand.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Version stamp for the profile popover: build date and short git hash.
    let date = std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=SLAQ_BUILD_DATE={date}");

    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=SLAQ_GIT_HASH={hash}");
    println!("cargo:rerun-if-changed=.git/HEAD");

    println!("cargo:rerun-if-changed=proto/SignalingProtocol.proto");

    let descriptors = protox::compile(["proto/SignalingProtocol.proto"], ["proto"])?;

    prost_build::Config::new()
        .compile_fds(descriptors)?;

    Ok(())
}
