use anyhow::{bail, Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub async fn copy(text: &str) -> Result<()> {
    let mut command = clipboard_command()?;
    copy_with_command(&mut command, text).await
}

fn clipboard_command() -> Result<Command> {
    if cfg!(target_os = "macos") {
        return Ok(Command::new("pbcopy"));
    }
    if cfg!(target_os = "windows") {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Console]::InputEncoding = [System.Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())",
        ]);
        return Ok(command);
    }
    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (program, args) in candidates {
        if (*program != "wl-copy" || std::env::var_os("WAYLAND_DISPLAY").is_some())
            && which::which(program).is_ok()
        {
            let mut command = Command::new(program);
            command.args(*args);
            return Ok(command);
        }
    }
    bail!("Install wl-copy, xclip, or xsel to copy to the clipboard")
}

async fn copy_with_command(command: &mut Command, text: &str) -> Result<()> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("Cannot start clipboard command")?;
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut stdin = child.stdin.take().context("Clipboard input unavailable")?;
        stdin.write_all(text.as_bytes()).await?;
        stdin.shutdown().await?;
        drop(stdin);
        let status = child.wait().await?;
        if !status.success() {
            bail!("Clipboard command failed ({status})");
        }
        Ok(())
    })
    .await
    .context("Clipboard command timed out")?
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn copy_preserves_unicode_whitespace_and_final_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clipboard");
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "cat > \"$1\"", "clipboard-test"]);
        command.arg(&path);
        let text = "  日本語\n\tcode\n";
        copy_with_command(&mut command, text).await.unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
    }

    #[tokio::test]
    async fn copy_reports_a_command_failure() {
        let mut command = Command::new("/usr/bin/false");
        assert!(copy_with_command(&mut command, "").await.is_err());
    }
}
