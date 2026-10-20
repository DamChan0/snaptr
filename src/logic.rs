//! Pure, side-effect-free logic: geometry, text wrapping, tesseract TSV parsing,
//! Google translate response parsing, and translation box placement.
//! Kept separate from X11/IO code so it can be unit tested without a display.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn overlaps(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

/// CJK ranges (Hangul jamo/syllables, Hiragana/Katakana, CJK Unified Ideographs + extensions).
/// These scripts are written without inter-word spaces, so OCR word joins and text
/// wrapping both treat them specially (no space glue, char-level line breaks).
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11FF |   // Hangul Jamo
        0x3040..=0x30FF |   // Hiragana + Katakana
        0x3400..=0x4DBF |   // CJK Ext A
        0x4E00..=0x9FFF |   // CJK Unified Ideographs
        0xA960..=0xA97F |   // Hangul Jamo Extended-A
        0xAC00..=0xD7A3 |   // Hangul Syllables
        0xF900..=0xFAFF |   // CJK Compat Ideographs
        0xFF00..=0xFFEF // Halfwidth/Fullwidth forms
    )
}

/// Greedy word-wrap. Words are split on whitespace; a word is char-broken (instead of
/// pushed whole) when it contains CJK text (no natural word boundary) or is itself wider
/// than `max_width` (nowhere else to break it).
pub fn wrap_text(text: &str, max_width: f32, char_width: impl Fn(char) -> f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0.0f32;
    let space_w = char_width(' ');

    for word in text.split_whitespace() {
        let word_w: f32 = word.chars().map(&char_width).sum();
        let char_break = word.chars().any(is_cjk) || word_w > max_width;

        if !char_break {
            let extra = if cur.is_empty() { 0.0 } else { space_w };
            if !cur.is_empty() && cur_w + extra + word_w > max_width {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0.0;
            }
            if !cur.is_empty() {
                cur.push(' ');
                cur_w += space_w;
            }
            cur.push_str(word);
            cur_w += word_w;
            continue;
        }

        // Word boundary before a char-broken run: keep the space if it fits, otherwise
        // start a fresh line instead of leaving a dangling trailing space.
        if !cur.is_empty() {
            if cur_w + space_w <= max_width {
                cur.push(' ');
                cur_w += space_w;
            } else {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0.0;
            }
        }
        for c in word.chars() {
            let cw = char_width(c);
            if !cur.is_empty() && cur_w + cw > max_width {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0.0;
            }
            cur.push(c);
            cur_w += cw;
        }
    }

    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// One tesseract TSV row (level/block_num/par_num plus geometry, conf, text).
struct TsvRow<'a> {
    level: i32,
    block_num: i32,
    par_num: i32,
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    conf: f32,
    text: &'a str,
}

fn parse_tsv_row(line: &str) -> Option<TsvRow<'_>> {
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() < 12 {
        return None;
    }
    Some(TsvRow {
        level: f[0].parse().ok()?,
        block_num: f[2].parse().ok()?,
        par_num: f[3].parse().ok()?,
        left: f[6].parse().ok()?,
        top: f[7].parse().ok()?,
        width: f[8].parse().ok()?,
        height: f[9].parse().ok()?,
        conf: f[10].parse().ok()?,
        text: f[11],
    })
}

pub struct OcrBlock {
    pub rect: Rect,
    pub text: String,
    /// Mean height (px) of the level-4 (line) rows inside this block; drives the
    /// translation box's starting font size.
    pub avg_line_height: f32,
}

/// Group tesseract TSV output into text blocks. A block is one (block_num, par_num) pair:
/// its bbox comes from the level-3 (paragraph) row, its text from concatenating level-5
/// (word) rows with conf >= 30, skipping empty words. Words are space-joined except where
/// both the preceding and following characters are CJK (those scripts have no inter-word
/// spaces in the source image).
pub fn parse_tsv(tsv: &str) -> Vec<OcrBlock> {
    let rows: Vec<TsvRow> = tsv.lines().skip(1).filter_map(parse_tsv_row).collect();

    let mut order: Vec<(i32, i32)> = Vec::new();
    let mut rects: std::collections::HashMap<(i32, i32), Rect> = std::collections::HashMap::new();
    for r in rows.iter().filter(|r| r.level == 3) {
        let key = (r.block_num, r.par_num);
        if !rects.contains_key(&key) {
            order.push(key);
        }
        rects.insert(
            key,
            Rect { x: r.left, y: r.top, w: r.width, h: r.height },
        );
    }

    let mut texts: std::collections::HashMap<(i32, i32), String> = std::collections::HashMap::new();
    for r in rows.iter().filter(|r| r.level == 5 && r.conf >= 30.0 && !r.text.trim().is_empty()) {
        let key = (r.block_num, r.par_num);
        let entry = texts.entry(key).or_default();
        if let (Some(last), Some(first)) = (entry.chars().last(), r.text.chars().next()) {
            if !(is_cjk(last) && is_cjk(first)) {
                entry.push(' ');
            }
        }
        entry.push_str(r.text);
    }

    let mut line_height_sum: std::collections::HashMap<(i32, i32), (f32, u32)> =
        std::collections::HashMap::new();
    for r in rows.iter().filter(|r| r.level == 4) {
        let e = line_height_sum.entry((r.block_num, r.par_num)).or_insert((0.0, 0));
        e.0 += r.height as f32;
        e.1 += 1;
    }

    order
        .into_iter()
        .filter_map(|key| {
            let text = texts.get(&key)?.clone();
            if text.is_empty() {
                return None;
            }
            let rect = *rects.get(&key).unwrap();
            let avg_line_height = line_height_sum
                .get(&key)
                .map(|(sum, count)| sum / *count as f32)
                .unwrap_or(rect.h as f32);
            Some(OcrBlock { rect, text, avg_line_height })
        })
        .collect()
}

/// Parse a `translate.googleapis.com` gtx response: a JSON array whose first element is
/// itself an array of `[translated, original, ...]` segments. The full translation is the
/// concatenation of every segment's first string.
pub fn parse_google_translation(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let segments = v.get(0)?.as_array()?;
    let mut out = String::new();
    for seg in segments {
        if let Some(s) = seg.get(0).and_then(|x| x.as_str()) {
            out.push_str(s);
        }
    }
    Some(out)
}

pub struct PlacedBox {
    pub rect: Rect,
    pub font_px: i32,
}

/// Choose where to put a translation box relative to its original `block`, inside `monitor`,
/// avoiding overlap with already-placed boxes / other blocks in `avoid`.
/// Prefers below the block, then above, then whichever side has more room while shrinking
/// the font from `preferred_font_px` down to `min_font_px` until it fits (or gives up and
/// places it best-effort at the minimum size).
#[allow(clippy::too_many_arguments)]
pub fn place_box(
    block: Rect,
    monitor: Rect,
    box_w: i32,
    preferred_font_px: i32,
    min_font_px: i32,
    gap: i32,
    avoid: &[Rect],
    height_for_font: impl Fn(i32) -> i32,
) -> PlacedBox {
    let x = (block.x).clamp(monitor.x, (monitor.x + monitor.w - box_w).max(monitor.x));
    let space_below = monitor.bottom() - block.bottom() - gap;
    let space_above = block.y - monitor.y - gap;

    let fits = |rect: &Rect, space: i32| rect.h <= space && !avoid.iter().any(|a| rect.overlaps(a));

    let mut font_px = preferred_font_px;
    loop {
        let h = height_for_font(font_px);
        let below = Rect { x, y: block.bottom() + gap, w: box_w, h };
        let above = Rect { x, y: block.y - gap - h, w: box_w, h };

        if fits(&below, space_below) {
            return PlacedBox { rect: below, font_px };
        }
        if fits(&above, space_above) {
            return PlacedBox { rect: above, font_px };
        }
        if font_px <= min_font_px {
            return if space_below >= space_above {
                PlacedBox { rect: below, font_px }
            } else {
                PlacedBox { rect: above, font_px }
            };
        }
        font_px -= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor() -> Rect {
        Rect { x: 0, y: 0, w: 1000, h: 1000 }
    }

    // 10px of box height per font px, for a deterministic height_for_font stand-in.
    fn height_at(px: i32) -> i32 {
        px * 10
    }

    #[test]
    fn below_chosen_when_room_below() {
        let block = Rect { x: 100, y: 100, w: 200, h: 50 };
        let placed = place_box(block, monitor(), 200, 20, 12, 8, &[], height_at);
        assert_eq!(placed.font_px, 20);
        assert_eq!(placed.rect.y, block.bottom() + 8);
    }

    #[test]
    fn above_chosen_at_screen_bottom_edge() {
        // Block sits right at the bottom edge of the monitor: no room below, plenty above.
        let block = Rect { x: 100, y: 950, w: 200, h: 40 };
        let placed = place_box(block, monitor(), 200, 20, 12, 8, &[], height_at);
        assert_eq!(placed.rect.y + placed.rect.h, block.y - 8);
        assert!(placed.rect.y >= monitor().y);
    }

    #[test]
    fn shrinks_when_neither_side_fits_at_preferred_size() {
        // Small monitor: only 37px above and below the block. height_at(20)=200 and even
        // height_at(12)=120 are both far too tall to fit either side -> shrink bottoms out
        // at min_font_px and places best-effort on the side with more room.
        let block = Rect { x: 100, y: 45, w: 200, h: 10 };
        let m = Rect { x: 0, y: 0, w: 1000, h: 100 };
        let placed = place_box(block, m, 200, 20, 12, 8, &[], height_at);
        assert_eq!(placed.font_px, 12, "should shrink all the way to min_font_px");
    }

    #[test]
    fn tsv_groups_block_and_joins_cjk_without_space() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   3\t1\t1\t0\t0\t0\t10\t20\t100\t30\t-1\t\n\
                   4\t1\t1\t0\t1\t0\t10\t20\t100\t30\t-1\t\n\
                   5\t1\t1\t0\t1\t1\t10\t20\t40\t30\t95.5\tHello\n\
                   5\t1\t1\t0\t1\t2\t55\t20\t30\t30\t92.0\tworld\n\
                   3\t1\t2\t0\t0\t0\t10\t60\t100\t30\t-1\t\n\
                   4\t1\t2\t0\t1\t0\t10\t60\t100\t30\t-1\t\n\
                   5\t1\t2\t0\t1\t1\t10\t60\t20\t30\t90.0\t\u{D55C}\n\
                   5\t1\t2\t0\t1\t2\t30\t60\t20\t30\t88.0\t\u{AD6D}\n\
                   5\t1\t2\t0\t1\t3\t50\t60\t10\t30\t5.0\tjunk\n";
        let blocks = parse_tsv(tsv);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "Hello world");
        assert_eq!(blocks[0].rect, Rect { x: 10, y: 20, w: 100, h: 30 });
        assert_eq!(blocks[0].avg_line_height, 30.0);
        // CJK chars join without a space; the low-confidence "junk" word is dropped.
        assert_eq!(blocks[1].text, "\u{D55C}\u{AD6D}");
    }

    #[test]
    fn google_response_concatenates_segments() {
        let json = r#"[[["\uc548\ub155","hello",null,null,1],[" \uc138\uacc4","world",null,null,1]],null,"en"]"#;
        let translated = parse_google_translation(json).unwrap();
        assert_eq!(translated, "\u{C548}\u{B155} \u{C138}\u{ACC4}");
    }
}
