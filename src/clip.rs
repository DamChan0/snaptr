//! Clipboard: `xclip` on Linux (it forks and keeps serving the selection after we exit),
//! the native clipboard via arboard on Windows (Windows keeps the data after we exit).

#[cfg(target_os = "linux")]
pub fn copy(text: &str) -> Result<(), String> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};

    let mut child = Command::new("xclip")
        .args(["-selection", "clipboard"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "xclip이 없어요 · sudo apt install xclip".to_string()
            } else {
                format!("failed to spawn xclip: {e}")
            }
        })?;

    child
        .stdin
        .take()
        .ok_or_else(|| "failed to open xclip stdin".to_string())?
        .write_all(text.as_bytes())
        .map_err(|e| format!("failed to write to xclip: {e}"))?;

    let status = child.wait().map_err(|e| format!("failed to wait for xclip: {e}"))?;
    if !status.success() {
        let mut stderr_buf = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut stderr_buf);
        }
        return Err(format!("xclip exited with {status}: {}", stderr_buf.trim()));
    }
    Ok(())
}

#[cfg(windows)]
pub fn copy(text: &str) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_text(text.to_owned()))
        .map_err(|e| format!("clipboard: {e}"))
}
