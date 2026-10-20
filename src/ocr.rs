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
    use crate::logic::{group_lines, is_cjk, Rect};
    use windows::core::HSTRING;
    use windows::Globalization::Language;
    use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
    use windows::Media::Ocr::OcrEngine;
    use windows::Security::Cryptography::CryptographicBuffer;

    fn unsupported_language(lang: &Lang) -> String {
        format!("Windows OCR 언어팩이 없어요 · 설정 > 시간 및 언어 > 언어에서 {} 추가", lang.label)
    }

    fn union(a: Rect, b: Rect) -> Rect {
        let x = a.x.min(b.x);
        let y = a.y.min(b.y);
        let right = a.right().max(b.right());
        let bottom = a.bottom().max(b.bottom());
        Rect { x, y, w: right - x, h: bottom - y }
    }

    pub fn ocr(rgb: &[u8], width: u32, height: u32, lang: &Lang) -> Result<Vec<OcrBlock>, String> {
        let language =
            Language::CreateLanguage(&HSTRING::from(lang.bcp47)).map_err(|_| unsupported_language(lang))?;
        if !OcrEngine::IsLanguageSupported(&language).unwrap_or(false) {
            return Err(unsupported_language(lang));
        }
        let engine = OcrEngine::TryCreateFromLanguage(&language).map_err(|_| unsupported_language(lang))?;

        let max_dim = OcrEngine::MaxImageDimension().map_err(|e| format!("failed to query OCR max image size: {e}"))?;
        if width > max_dim || height > max_dim {
            return Err(format!("캡처 영역이 너무 커요 (최대 {max_dim}px) · 더 작은 영역을 선택해 주세요"));
        }

        // SoftwareBitmap wants BGRA8; we only ever carry packed RGB internally.
        let mut bgra = Vec::with_capacity(rgb.len() / 3 * 4);
        for px in rgb.chunks_exact(3) {
            bgra.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
        let buffer =
            CryptographicBuffer::CreateFromByteArray(&bgra).map_err(|e| format!("failed to wrap pixel buffer: {e}"))?;
        let bitmap = SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, width as i32, height as i32)
            .map_err(|e| format!("failed to build software bitmap: {e}"))?;

        let op = engine.RecognizeAsync(&bitmap).map_err(|e| format!("OCR 요청을 시작하지 못했어요: {e}"))?;
        // `IAsyncOperation<T>` (from the separate windows-future crate pulled in transitively)
        // has no public blocking accessor in this windows-rs version (the old `.get()` is gone,
        // and `.join()` lives behind a private module), so poll its public Status()/GetResults()
        // instead. AsyncStatus::Started == 0; any other value means the op is done.
        loop {
            let status = op.Status().map_err(|e| format!("OCR 상태 조회에 실패했어요: {e}"))?;
            if status.0 != 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let result = op.GetResults().map_err(|e| format!("OCR 인식에 실패했어요: {e}"))?;

        let ocr_lines = result.Lines().map_err(|e| format!("failed to read OCR lines: {e}"))?;
        let mut lines: Vec<(Rect, String)> = Vec::new();
        for i in 0..ocr_lines.Size().map_err(|e| format!("failed to read OCR line count: {e}"))? {
            let line = ocr_lines.GetAt(i).map_err(|e| format!("failed to read OCR line {i}: {e}"))?;
            let words = line.Words().map_err(|e| format!("failed to read OCR words: {e}"))?;
            let mut text = String::new();
            let mut rect: Option<Rect> = None;
            for j in 0..words.Size().map_err(|e| format!("failed to read OCR word count: {e}"))? {
                let word = words.GetAt(j).map_err(|e| format!("failed to read OCR word {j}: {e}"))?;
                let word_text = word.Text().map_err(|e| format!("failed to read OCR word text: {e}"))?.to_string();
                if word_text.is_empty() {
                    continue;
                }
                let b = word.BoundingRect().map_err(|e| format!("failed to read OCR word bounds: {e}"))?;
                let word_rect =
                    Rect { x: b.X.round() as i32, y: b.Y.round() as i32, w: b.Width.round() as i32, h: b.Height.round() as i32 };
                rect = Some(match rect {
                    Some(r) => union(r, word_rect),
                    None => word_rect,
                });
                // Windows.Media.Ocr always separates words with a space, including CJK where
                // the source script has none; mirror the join rule `parse_tsv` uses on Linux.
                match (text.chars().last(), word_text.chars().next()) {
                    (Some(last), Some(first)) if is_cjk(last) && is_cjk(first) => {}
                    (Some(_), Some(_)) => text.push(' '),
                    _ => {}
                }
                text.push_str(&word_text);
            }
            if let Some(rect) = rect {
                if !text.is_empty() {
                    lines.push((rect, text));
                }
            }
        }

        Ok(group_lines(&lines))
    }
}
