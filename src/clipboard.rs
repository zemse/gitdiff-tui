use anyhow::{Context, Result, anyhow};
use std::io::Write;
use std::process::{Command, Stdio};

pub fn copy(text: &str) -> Result<()> {
    let cmd = if cfg!(target_os = "macos") {
        "pbcopy"
    } else if cfg!(target_os = "linux") {
        // wl-copy if Wayland session, else xclip
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            "wl-copy"
        } else {
            "xclip"
        }
    } else if cfg!(target_os = "windows") {
        "clip"
    } else {
        return Err(anyhow!("clipboard not supported on this platform"));
    };

    let mut child = Command::new(cmd)
        .args(if cmd == "xclip" {
            vec!["-selection", "clipboard"]
        } else {
            vec![]
        })
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("could not spawn `{cmd}` — is it installed?"))?;

    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("failed to open clipboard stdin"))?
        .write_all(text.as_bytes())?;

    let status = child.wait()?;
    if !status.success() {
        return Err(anyhow!("clipboard tool `{cmd}` failed"));
    }
    Ok(())
}
