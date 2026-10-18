//! Exercises the production mod pipeline through the real binary.
//!
//! The unit tests compile in-process (the test harness has no worker `main`), so
//! this is the only place that covers the worker hook, the rlimits/timeout path,
//! the parent's `Component::deserialize` and instantiation — by driving the
//! public `mod` command, which does all of that.
#![cfg(feature = "mods")]

#[test]
fn mod_command_runs_a_component_end_to_end() {
    let exe = env!("CARGO_BIN_EXE_slaq");
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/hello-mod");
    let out = std::process::Command::new(exe)
        .args([
            "mod", "--dir", dir, "--channel", "C0T", "--ts", "1.000000", "--author", "U0",
            "--text", "hi", "--grant-all",
        ])
        .output()
        .expect("run `slaq mod`");

    assert!(
        out.status.success(),
        "`slaq mod` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hello #1"),
        "the production pipeline (worker + deserialize + run) did not decorate: {stdout}"
    );
}

/// Without a grant the mod loads but receives no events: the host denies the
/// read (and every later host call), so nothing is decorated. This is the
/// end-to-end default-deny path.
#[test]
fn mod_command_denies_without_grants() {
    let exe = env!("CARGO_BIN_EXE_slaq");
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/hello-mod");
    let out = std::process::Command::new(exe)
        .args([
            "mod", "--dir", dir, "--channel", "C0T", "--ts", "1.000000", "--author", "U0",
            "--text", "hi",
        ])
        .output()
        .expect("run `slaq mod`");

    assert!(
        out.status.success(),
        "`slaq mod` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("(no decorations)"),
        "an ungranted mod must decorate nothing: {stdout}"
    );
}

#[test]
fn a_stray_env_var_does_not_hijack_invocations() {
    // Without the argv sentinel, the env vars must NOT turn the binary into a
    // compiler: `--version` still prints the version and exits normally.
    let exe = env!("CARGO_BIN_EXE_slaq");
    let out = std::env::temp_dir().join(format!("slaq-hijack-{}.cwasm", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let status = std::process::Command::new(exe)
        .arg("--version")
        .env("SLAQ_MOD_COMPILE_IN", "/nonexistent")
        .env("SLAQ_MOD_COMPILE_OUT", &out)
        .status()
        .expect("run the binary");
    assert!(status.success(), "unexpected exit: {status:?}");
    assert!(!out.exists(), "the env vars hijacked a normal invocation");
}

/// The consent CLI is the only shipped way to change grants: exercise
/// grant → list → revoke end to end, with isolated `SLAQ_MODS_DIR` /
/// `SLAQ_MODS_GRANTS`, covering the lock/load/mutate/save path.
#[test]
fn mods_cli_grant_list_revoke_end_to_end() {
    let exe = env!("CARGO_BIN_EXE_slaq");
    let root = std::env::temp_dir().join(format!("slaq-mods-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mods = root.join("mods");
    let pkg = mods.join("hello");
    std::fs::create_dir_all(&pkg).expect("create mods dir");
    for f in ["manifest.json", "mod.wasm"] {
        std::fs::copy(
            format!("{}/tests/data/hello-mod/{f}", env!("CARGO_MANIFEST_DIR")),
            pkg.join(f),
        )
        .expect("copy fixture");
    }
    let grants = root.join("grants.json");
    let run = |args: &[&str]| {
        std::process::Command::new(exe)
            .args(args)
            .env("SLAQ_MODS_DIR", &mods)
            .env("SLAQ_MODS_GRANTS", &grants)
            .output()
            .expect("run slaq")
    };

    // grant: messages.read (scope snapshotted from the manifest) + store.kv
    let out = run(&["mods", "grant", "dev.slaq.hello", "messages.read", "store.kv"]);
    assert!(
        out.status.success(),
        "grant failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("read scope: all"), "grant output: {stdout}");

    // list: requested + granted + the effective scope
    let out = run(&["mods", "list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("dev.slaq.hello"), "{stdout}");
    assert!(stdout.contains("read scope: all"), "{stdout}");
    assert!(
        stdout.contains("granted:   messages.read, store.kv"),
        "list output: {stdout}"
    );

    // revoke messages.read: the scope disappears and only store.kv remains
    let out = run(&["mods", "revoke", "dev.slaq.hello", "messages.read"]);
    assert!(
        out.status.success(),
        "revoke failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run(&["mods", "list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("read scope:"), "scope must be gone: {stdout}");
    let persisted = std::fs::read_to_string(&grants).expect("read store");
    assert!(persisted.contains("store.kv"), "{persisted}");
    assert!(!persisted.contains("messages.read"), "{persisted}");

    // An invalid id is refused up front (nothing is written).
    let out = run(&["mods", "grant", "Bad Id", "store.kv"]);
    assert!(!out.status.success(), "an invalid id must be refused");

    let _ = std::fs::remove_dir_all(&root);
}
