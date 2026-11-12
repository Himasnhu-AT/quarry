#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `quarry --headless` renders without a window; every DESIGN.md screen has a scene; `quarry-cli`
//! lists the agent tools and none of the human-only actions.

use std::process::Command;

use quarry::ids;
use quarry::scenes::SCENES;

fn png_size(bytes: &[u8]) -> (u32, u32) {
    assert_eq!(bytes.get(..8), Some(&b"\x89PNG\r\n\x1a\n"[..]), "not a PNG");
    let be = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
    (be(16), be(20))
}

#[test]
fn headless_snap_and_tree_exit_zero() {
    let dir = tempfile::tempdir().unwrap();
    let (snap, tree) = (dir.path().join("out.png"), dir.path().join("tree.json"));
    let out = Command::new(env!("CARGO_BIN_EXE_quarry"))
        .args(["--headless", "--size", "1280x800", "--scale", "1", "--theme", "dark"])
        .arg("--snap")
        .arg(&snap)
        .arg("--tree")
        .arg(&tree)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert_eq!(png_size(&std::fs::read(&snap).unwrap()), (1280, 800));
    let tree = std::fs::read_to_string(&tree).unwrap();
    serde_json::from_str::<serde_json::Value>(&tree).unwrap();
    for id in ["quarry.empty", "quarry.toolbar.run", "quarry.sidebar.new_connection", "quarry.status"] {
        assert!(tree.contains(id), "{id} in {tree}");
    }
}

#[test]
fn every_screen_has_a_scene_that_renders() {
    let out = Command::new(env!("CARGO_BIN_EXE_quarry")).arg("--list-scenes").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let listed: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect();
    assert_eq!(listed, SCENES);
    let dir = tempfile::tempdir().unwrap();
    for scene in SCENES {
        let tree = dir.path().join(format!("{scene}.json"));
        let out = Command::new(env!("CARGO_BIN_EXE_quarry")).args(["--headless", "--scene", scene, "--tree"]).arg(&tree).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{scene}: {}", String::from_utf8_lossy(&out.stderr));
        let t = std::fs::read_to_string(&tree).unwrap();
        assert!(t.contains(&format!("quarry.screen.{scene}")), "{scene}: {t}");
        if *scene == "approval-pending" {
            assert!(t.contains("quarry.approval.approve") && t.contains("quarry.approval.deny"), "{t}");
        }
    }
}

#[test]
fn cli_lists_agent_tools_but_no_human_only_action() {
    let out = Command::new(env!("CARGO_BIN_EXE_quarry-cli")).arg("commands").output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    for t in ids::AGENT_TOOLS {
        assert!(text.contains(t.id), "{} in {text}", t.id);
    }
    for id in ids::HUMAN_ONLY {
        assert!(!text.contains(id), "{id} listed by quarry-cli");
    }
    let out = Command::new(env!("CARGO_BIN_EXE_quarry-cli")).args(["exec", ids::PROPOSAL_APPROVE, "{\"proposal_id\": 1}"]).output().unwrap();
    assert_ne!(out.status.code(), Some(0), "approve over the CLI must fail");
}
