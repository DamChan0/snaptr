//! Side-effecting I/O: spawning tesseract for OCR, hitting Google's unofficial
//! translate endpoint, and pushing text to the X clipboard via xclip.
//! Kept separate from `logic` so the pure parsing/placement code stays testable
//! without a display, network, or external binaries.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

use crate::logic::{parse_google_translation, parse_tsv, OcrBlock};

/// Run tesseract over a raw RGB buffer and parse its TSV output into blocks.
///
/// `rgb` is packed 3 bytes/pixel, row-major, `width * height * 3` bytes. The
/// returned rects are relative to the image (i.e. to the captured region).
pub fn ocr(rgb: &[u8], width: u32, height: u32, langs: &str) -> Result<Vec<OcrBlock>, String> {
    if rgb.len() != (width as usize) * (height as usize) * 3 {
        return Err(format!(
            "ocr: rgb buffer size {} does not match {}x{}x3",
            rgb.len(),
            width,
            height
        ));
    }

    let mut child = Command::new("tesseract")
        .args(["stdin", "stdout", "-l", langs, "--psm", "3", "tsv"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "tesseract가 없어요 · sudo apt install tesseract-ocr tesseract-ocr-kor".to_string()
            } else {
                format!("failed to spawn tesseract: {e}")
            }
        })?;

    // Write the PPM (P6) to stdin on a separate thread so we don't deadlock
    // if tesseract starts producing stdout before we've finished writing.
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open tesseract stdin".to_string())?;
    let header = format!("P6\n{width} {height}\n255\n");
    let rgb_owned = rgb.to_vec();
    let writer = std::thread::spawn(move || -> Result<(), String> {
        stdin
            .write_all(header.as_bytes())
            .map_err(|e| format!("failed to write PPM header to tesseract: {e}"))?;
        stdin
            .write_all(&rgb_owned)
            .map_err(|e| format!("failed to write pixel data to tesseract: {e}"))?;
        // Dropping `stdin` here closes the pipe so tesseract sees EOF.
        Ok(())
    });

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to open tesseract stdout".to_string())?;
    let mut tsv = String::new();
    stdout
        .read_to_string(&mut tsv)
        .map_err(|e| format!("failed to read tesseract output: {e}"))?;

    writer
        .join()
        .map_err(|_| "tesseract stdin writer thread panicked".to_string())??;

    let status = child
        .wait()
        .map_err(|e| format!("failed to wait for tesseract: {e}"))?;

    if !status.success() {
        let mut stderr_buf = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut stderr_buf);
        }
        return Err(format!(
            "tesseract exited with {status}: {}",
            stderr_buf.trim()
        ));
    }

    Ok(parse_tsv(&tsv))
}

/// Translate `text` to language `to` via Google's unofficial gtx endpoint
/// (`sl=auto`). Returns the human-readable translation.
pub fn translate(text: &str, to: &str) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();

    let resp = agent
        .get("https://translate.googleapis.com/translate_a/single")
        .query("client", "gtx")
        .query("sl", "auto")
        .query("tl", to)
        .query("dt", "t")
        .query("q", text)
        .call()
        .map_err(|e| format!("translation request failed: {e}"))?;

    let body = resp
        .into_string()
        .map_err(|e| format!("failed to read translation response: {e}"))?;

    parse_google_translation(&body)
        .ok_or_else(|| "failed to parse translation response".to_string())
}

/// Copy `text` to the X clipboard via `xclip -selection clipboard`.
///
/// `xclip` forks and keeps serving the clipboard selection after this
/// process exits, so we don't need to wait for it to finish serving.
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
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

    let status = child
        .wait()
        .map_err(|e| format!("failed to wait for xclip: {e}"))?;

    if !status.success() {
        let mut stderr_buf = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut stderr_buf);
        }
        return Err(format!("xclip exited with {status}: {}", stderr_buf.trim()));
    }

    Ok(())
}
