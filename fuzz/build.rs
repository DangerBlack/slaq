//! Regenerates the Chime signaling types for the `signaling` fuzz target, so it
//! can decode a frame without depending on the whole `slaq` crate.
//!
//! `protox` is used instead of `protoc`, exactly like the app's own build.

fn main() {
    println!("cargo:rerun-if-changed=../proto/SignalingProtocol.proto");

    let descriptors = protox::compile(["../proto/SignalingProtocol.proto"], ["../proto"])
        .expect("compile SignalingProtocol.proto");
    prost_build::Config::new()
        .compile_fds(descriptors)
        .expect("generate prost types");
}
