//! Puts the pinned default theme where `rust-embed` will find it, and the agent
//! binaries where `main.rs` embeds them.
//!
//! The hub embeds a built theme, which `scripts/theme.sh` fetches. Running it
//! here lets a plain `cargo build` proceed without a setup step, taking its
//! input from `web-theme.pin` rather than a hand-maintained directory.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the release job puts the agents it built from `agent/`. Inside the
/// project, because `cross` mounts the project and nothing else.
const AGENTS: &str = "target/agents";

fn main() {
    // Cargo reruns this only when one of these changes. The stamp is listed as
    // well, so deleting `target/theme` triggers a refetch rather than a later
    // failure inside rust-embed.
    println!("cargo:rerun-if-changed=web-theme.pin");
    println!("cargo:rerun-if-changed=target/theme/.pin");
    println!("cargo:rerun-if-changed=scripts/theme.sh");

    match Command::new("sh").arg("scripts/theme.sh").status() {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("scripts/theme.sh failed ({status}); see the message above"),
        Err(e) => panic!("could not run scripts/theme.sh: {e}"),
    }
    agents();
}

/// Writes `agents.rs`: both agents and the version they were built at, or
/// empty slices for a development build, whose `/agent/{arch}` then says so.
fn agents() {
    println!("cargo:rerun-if-changed={AGENTS}");
    println!("cargo:rerun-if-changed=agent/Cargo.toml");
    println!("cargo:rerun-if-env-changed=NODEHELM_REQUIRE_AGENTS");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut code = String::new();
    let mut found = 0;
    // ELF e_machine: what keeps one architecture's binary out of the other's slot.
    for (arch, machine) in [("x86_64", 62u16), ("aarch64", 183)] {
        let source = Path::new(AGENTS).join(format!("monitor-agent-{arch}-unknown-linux-musl"));
        println!("cargo:rerun-if-changed={}", source.display());
        let name = arch.to_uppercase();
        match std::fs::read(&source) {
            Ok(binary) => {
                let built_for =
                    binary.starts_with(b"\x7fELF") && binary.get(18..20) == Some(&machine.to_le_bytes()[..]);
                assert!(built_for, "{} is not an {arch} executable", source.display());
                let copy = out.join(format!("agent-{arch}"));
                std::fs::write(&copy, &binary).unwrap();
                code += &format!("pub static {name}: &[u8] = include_bytes!({:?});\n", copy.display());
                found += 1;
            }
            Err(_) => code += &format!("pub static {name}: &[u8] = &[];\n"),
        }
    }
    let required = std::env::var("NODEHELM_REQUIRE_AGENTS").is_ok_and(|v| v == "1");
    assert!(!required || found == 2, "NODEHELM_REQUIRE_AGENTS is set but {AGENTS} lacks an agent");
    assert!(found != 1, "{AGENTS} holds one agent; a hub serves both architectures or neither");
    let version = if found == 2 { agent_version() } else { String::new() };
    code += &format!("pub const VERSION: &str = {version:?};\n");
    std::fs::write(out.join("agents.rs"), code).unwrap();
}

/// The version in `agent/Cargo.toml`, which the release job builds from.
fn agent_version() -> String {
    let manifest = std::fs::read_to_string("agent/Cargo.toml").unwrap();
    manifest
        .lines()
        .find_map(|line| line.strip_prefix("version = \"")?.strip_suffix('"'))
        .expect("agent/Cargo.toml has a version")
        .to_owned()
}
