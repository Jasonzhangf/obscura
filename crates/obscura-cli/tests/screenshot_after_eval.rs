#![cfg(feature = "render")]

use std::process::Command;

fn output_directory() -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "obscura-screenshot-after-eval-{}-{unique}",
        std::process::id()
    ))
}

#[test]
fn screenshot_is_captured_after_eval_scrolls_the_live_page() {
    let directory = output_directory();
    std::fs::create_dir_all(&directory).expect("temporary output directory");
    let top = directory.join("top.png");
    let scrolled = directory.join("scrolled.png");
    let url = concat!(
        "data:text/html,<html style=\"margin:0\"><body style=\"margin:0\">",
        "<div style=\"height:80px;background:red\"></div>",
        "<div style=\"height:80px;background:blue\"></div>",
        "</body></html>"
    );

    let run = |path: &std::path::Path, eval: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_obscura"));
        command
            .args(["fetch", url, "--screenshot"])
            .arg(path)
            .args(["--wait", "0", "--timeout", "5", "--quiet"])
            .env("OBSCURA_SHOT_W", "100")
            .env("OBSCURA_SHOT_H", "80");
        if let Some(expression) = eval {
            command.args(["--eval", expression]);
        }
        command.output().expect("run obscura fetch")
    };

    let top_output = run(&top, None);
    assert!(
        top_output.status.success(),
        "top capture failed: {}",
        String::from_utf8_lossy(&top_output.stderr)
    );

    let scrolled_output = run(
        &scrolled,
        Some(
            "(()=>{window.scrollTo(0,80);return JSON.stringify({y:window.scrollY})})()",
        ),
    );
    assert!(
        scrolled_output.status.success(),
        "scrolled capture failed: {}",
        String::from_utf8_lossy(&scrolled_output.stderr)
    );
    let state: serde_json::Value =
        serde_json::from_slice(&scrolled_output.stdout).expect("evaluation state JSON");
    assert_eq!(state["captureState"]["scrollY"].as_f64(), Some(80.0));

    let top_bytes = std::fs::read(&top).expect("top PNG");
    let scrolled_bytes = std::fs::read(&scrolled).expect("scrolled PNG");
    assert_ne!(
        top_bytes, scrolled_bytes,
        "the post-eval screenshot must paint the scrolled viewport"
    );

    std::fs::remove_dir_all(directory).expect("remove temporary output directory");
}

#[test]
fn paired_capture_reasserts_scroll_after_the_post_eval_settle() {
    let directory = output_directory();
    std::fs::create_dir_all(&directory).expect("temporary output directory");
    let screenshot = directory.join("reasserted.png");
    let url = concat!(
        "data:text/html,<html style=\"margin:0;scroll-behavior:smooth\">",
        "<body style=\"margin:0\"><div style=\"height:400px\"></div>",
        "</body></html>"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_obscura"))
        .args(["fetch", url, "--screenshot"])
        .arg(&screenshot)
        .args([
            "--eval",
            "(()=>{scrollTo(0,40);setTimeout(()=>scrollTo(0,10),10);return 'started'})()",
            "--wait",
            "1",
            "--timeout",
            "5",
            "--quiet",
        ])
        .env("OBSCURA_SHOT_W", "100")
        .env("OBSCURA_SHOT_H", "80")
        .env("OBSCURA_SHOT_SCROLL_X", "0")
        .env("OBSCURA_SHOT_SCROLL_Y", "80")
        .output()
        .expect("run obscura fetch");
    assert!(
        output.status.success(),
        "capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("capture report JSON");
    assert_eq!(
        report["controlledScroll"]["preReassertActual"]["y"].as_f64(),
        Some(10.0),
        "the diagnostic offset must be sampled after the post-eval settle"
    );
    assert_eq!(
        report["controlledScroll"]["finalReassertActual"]["y"].as_f64(),
        Some(80.0)
    );
    assert_eq!(report["captureState"]["scrollY"].as_f64(), Some(80.0));
    assert_eq!(
        report["controlledScroll"]["phase"].as_str(),
        Some("immediately-before-capture-state-and-screenshot")
    );

    std::fs::remove_dir_all(directory).expect("remove temporary output directory");
}
