//! OCR: Tesseract CLI on Linux, Windows.Media.Ocr on Windows. Both return blocks whose
//! rects are relative to the image.

use crate::config::Lang;
use crate::logic::OcrBlock;

/// `rgb` is packed 3 bytes/pixel, row-major, `width * height * 3` bytes.
pub fn ocr(rgb: &[u8], width: u32, height: u32, lang: &Lang) -> Result<Vec<OcrBlock>, String> {
    if rgb.len() != (width as usize) * (height as usize) * 3 {
        return Err(format!("ocr: rgb buffer size {} does not match {width}x{height}x3", rgb.len()));
    }
    platform::ocr(rgb, width, height, lang)
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use crate::logic::parse_tsv;
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};

    pub fn ocr(rgb: &[u8], width: u32, height: u32, lang: &Lang) -> Result<Vec<OcrBlock>, String> {
        let mut child = Command::new("tesseract")
            .args(["stdin", "stdout", "-l", lang.tesseract, "--psm", "3", "tsv"])
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
        let mut stdin = child.stdin.take().ok_or_else(|| "failed to open tesseract stdin".to_string())?;
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

        let mut stdout = child.stdout.take().ok_or_else(|| "failed to open tesseract stdout".to_string())?;
        let mut tsv = String::new();
        stdout
            .read_to_string(&mut tsv)
            .map_err(|e| format!("failed to read tesseract output: {e}"))?;

        writer.join().map_err(|_| "tesseract stdin writer thread panicked".to_string())??;

        let status = child.wait().map_err(|e| format!("failed to wait for tesseract: {e}"))?;
        if !status.success() {
            let mut stderr_buf = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut stderr_buf);
            }
            let stderr_buf = stderr_buf.trim();
            if stderr_buf.contains("Failed loading language") {
                return Err(format!(
                    "tesseract 언어팩이 없어요 · sudo apt install tesseract-ocr-{}",
                    lang.tesseract.replace('_', "-")
                ));
            }
            return Err(format!("tesseract exited with {status}: {stderr_buf}"));
        }

        Ok(parse_tsv(&tsv))
    }
}

#[cfg(windows)]
mod platform {
    use super::*;

    pub fn ocr(_rgb: &[u8], _width: u32, _height: u32, _lang: &Lang) -> Result<Vec<OcrBlock>, String> {
        Err("Windows OCR not implemented yet".into())
    }
}
