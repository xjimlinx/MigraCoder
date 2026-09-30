#![cfg(unix)]

use anyhow::Result;
use serde_json::json;
use std::fs;
use std::process::Command;

#[test]
fn next_page_command_preserves_custom_data_directories() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let codex_home = temporary.path().join("Codex data's $directory");
    let session_dir = codex_home.join("sessions/2026/01/02");
    let opencode_home = temporary.path().join("OpenCode data's $directory");
    fs::create_dir_all(&session_dir)?;
    fs::create_dir_all(&opencode_home)?;
    let records = [
        json!({"type":"session_meta","payload":{"id":"pagination-fixture","cwd":"/example/project"}}),
        json!({"type":"event_msg","payload":{"type":"user_message","message":"first page user message"}}),
        json!({"type":"event_msg","payload":{"type":"agent_message","message":"second page assistant message"}}),
    ];
    let content = records
        .iter()
        .map(serde_json::to_string)
        .collect::<serde_json::Result<Vec<_>>>()?
        .join("\n")
        + "\n";
    fs::write(session_dir.join("rollout.jsonl"), content)?;
    let executable = env!("CARGO_BIN_EXE_migracoder");

    for enable_opencode in [false, true] {
        let mut command = Command::new(executable);
        command.arg("--codex-home").arg(&codex_home);
        if enable_opencode {
            command.arg("--opencode-home").arg(&opencode_home);
        } else {
            command.arg("--no-opencode");
        }
        let first = command
            .args(["show-session", "pagination-fixture", "--limit", "1"])
            .output()?;
        assert!(
            first.status.success(),
            "{}",
            String::from_utf8_lossy(&first.stderr)
        );
        let first_text = String::from_utf8(first.stdout)?;
        assert!(first_text.contains("first page user message"));
        assert!(!first_text.contains("second page assistant message"));
        let next_command = first_text
            .lines()
            .find_map(|line| line.strip_prefix("下一页："))
            .expect("next page command");
        assert!(next_command.contains(if enable_opencode {
            "--opencode-home"
        } else {
            "--no-opencode"
        }));

        let arguments = next_command
            .strip_prefix("migracoder ")
            .expect("CLI command");
        let shell_command = format!("'{}' {arguments}", executable.replace('\'', "'\\''"));
        let next = Command::new("/bin/sh")
            .args(["-c", &shell_command])
            .output()?;
        assert!(
            next.status.success(),
            "{}",
            String::from_utf8_lossy(&next.stderr)
        );
        let next_text = String::from_utf8(next.stdout)?;
        assert!(next_text.contains("second page assistant message"));
        assert!(!next_text.contains("first page user message"));
        assert!(next_text.contains("已到会话末尾"));
    }
    Ok(())
}
