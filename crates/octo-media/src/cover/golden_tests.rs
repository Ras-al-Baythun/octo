//! The golden covers from the design's reference (tools/cover-art/reference.py in the Octo
//! app's repo, copied as crates/octo-media/tests/fixtures/cover-golden/samples.json): the words' sizes and boxes, and
//! the veiled background before any words, sampled across each cover.
//!
//! Beyond those, the reference renderer's output (docs/rust-migration/fixtures/covers): every
//! layout's sizes, lines and boxes, the fonts it chose, its veiled backgrounds pixel for pixel,
//! and its finished covers, compared by SSIM.

use std::path::PathBuf;
use std::sync::LazyLock;

use image::{Rgb, RgbImage};
use serde_json::Value;

use super::cover_backgrounds;
use super::cover_book::CoverBook;
use super::cover_colors;
use super::cover_fonts::{self, CoverTypesetter};
use super::cover_layout::{self, CoverAlign, CoverSpec, CoverType, CoverWords, Measured, WordsRole};
use super::cover_painter::{self, CoverArt};
use super::cover_veil;
use super::test_support::{HEAVY, repo};

fn book() -> &'static CoverBook {
    CoverBook::shipped()
}

static SAMPLES: LazyLock<Value> = LazyLock::new(|| {
    let text = std::fs::read_to_string(repo("crates/octo-media/tests/fixtures/cover-golden/samples.json"))
        .expect("samples.json reads");
    serde_json::from_str(&text).expect("samples.json parses")
});

static REFERENCE: LazyLock<Value> = LazyLock::new(|| {
    let text = std::fs::read_to_string(repo("docs/rust-migration/fixtures/covers/reference.json"))
        .expect("the C# cover reference reads");
    serde_json::from_str(&text).expect("the C# cover reference parses")
});

fn golden(index: usize) -> &'static Value {
    &SAMPLES["covers"][index]
}

fn text(value: &Value, key: &str) -> Option<String> {
    value[key].as_str().map(str::to_string)
}

fn num(value: &Value, key: &str) -> f64 {
    value[key]
        .as_f64()
        .unwrap_or_else(|| panic!("{key} is a number in {value}"))
}

fn role_of(name: &str) -> WordsRole {
    match name {
        "title" | "Title" => WordsRole::Title,
        "line" | "Line" => WordsRole::Line,
        _ => WordsRole::Footer,
    }
}

fn background_index(file: &str) -> usize {
    book()
        .backgrounds
        .iter()
        .position(|b| b.file == file)
        .expect("the golden's background is in the library")
}

fn compose(golden: &Value) -> CoverArt {
    let side = num(golden, "side") as i32;
    let name = text(golden, "name").expect("a name");
    let spec = CoverSpec {
        id: name.clone(),
        name,
        line: text(golden, "line"),
        footer: text(golden, "footer"),
        music: None,
        glyph: None,
    };
    let index = background_index(golden["background"].as_str().expect("a background"));
    CoverArt {
        side,
        background: index,
        orientation: 0,
        words: cover_layout::words(&spec, side, &CoverTypesetter, book()),
        glyph: None,
    }
}

fn max_channel_difference(a: &Rgb<u8>, b: [i64; 3]) -> i64 {
    (0..3).map(|k| (a.0[k] as i64 - b[k]).abs()).max().unwrap_or(0)
}

// ------------------------------------------------------------ the design's goldens

/// The text metrics tolerance, in pixels, for a right edge against the reference: max(2, 2% of
/// the line), because Pillow sets Inter without kerning and this renderer with it.
fn right_slack(x: f64, right: f64) -> f64 {
    2f64.max(0.02 * (right - x))
}

/// Sizes, lines, left edges, tops and heights match the reference to the pixel. Right edges
/// come from each engine's own widths: the reference's Pillow sets Inter without its kerning,
/// this renderer with it, so a right edge may sit a few pixels short of the reference's.
#[test]
fn words_match_the_reference() {
    for index in 0..6 {
        let golden = golden(index);
        let file = text(golden, "file").unwrap_or_default();
        let art = compose(golden);
        let expected = golden["words"].as_array().expect("words");
        assert_eq!(
            expected.len(),
            art.words.len(),
            "{file}: how many blocks of words"
        );
        for want in expected {
            let role = role_of(want["role"].as_str().unwrap_or_default());
            let got = art
                .words
                .iter()
                .find(|w| w.role == role)
                .expect("the block is laid out");
            let (lines, _) = CoverTypesetter.lines(&got.text, &got.type_, got.width);
            assert_eq!(
                num(want, "size") as i32,
                got.type_.size_px as i32,
                "{file} {role:?}: size"
            );
            let want_lines: Vec<&str> = want["lines"]
                .as_array()
                .expect("lines")
                .iter()
                .filter_map(Value::as_str)
                .collect();
            let got_lines: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
            assert_eq!(want_lines, got_lines, "{file} {role:?}: lines");
            let inked = got.inked();
            assert!(
                (num(want, "x") - inked[0] as f64).abs() <= 0.01,
                "{file} {role:?}: x {}",
                inked[0]
            );
            assert!(
                (num(want, "top") - got.top as f64).abs() <= 0.01,
                "{file} {role:?}: top {}",
                got.top
            );
            assert!(
                (num(want, "height") - got.measured.height as f64).abs() <= 0.01,
                "{file} {role:?}: height {}",
                got.measured.height
            );
            // Kerning moves a right edge by up to about 2% of the line.
            let right = num(want, "right");
            let slack = right_slack(num(want, "x"), right);
            println!("{file} {role:?}: right {:.1}, reference {right}", inked[2]);
            assert!(
                (inked[2] as f64 - right).abs() <= slack,
                "{file} {role:?}: right {} against {right} (slack {slack:.1})",
                inked[2]
            );
        }
    }
}

/// The reference's own word boxes, as blocks of words with nothing to draw.
fn reference_boxes(golden: &Value, side: i32) -> Vec<CoverWords> {
    golden["words"]
        .as_array()
        .expect("words")
        .iter()
        .map(|w| {
            let (x, top, height, right) = (
                num(w, "x") as f32,
                num(w, "top") as f32,
                num(w, "height") as f32,
                num(w, "right") as f32,
            );
            let type_ = CoverType {
                size_px: num(w, "size") as f32,
                weight: 400,
                tracking: 0.0,
                line_height: 1.0,
                max_lines: 1,
            };
            CoverWords {
                text: String::new(),
                type_,
                left: x,
                top,
                width: side as f32,
                align: CoverAlign::Left,
                ink: cover_colors::WHITE,
                measured: Measured {
                    lines: 1,
                    width: right - x,
                    height,
                    cut: false,
                },
                role: role_of(w["role"].as_str().unwrap_or_default()),
            }
        })
        .collect()
}

fn samples(golden: &Value) -> Vec<(u32, u32, [i64; 3])> {
    golden["samples"]
        .as_array()
        .expect("samples")
        .iter()
        .map(|s| {
            let rgb: Vec<i64> = s["rgb"]
                .as_array()
                .expect("rgb")
                .iter()
                .filter_map(Value::as_i64)
                .collect();
            (num(s, "x") as u32, num(s, "y") as u32, [rgb[0], rgb[1], rgb[2]])
        })
        .collect()
}

/// The veiled background matches the reference within 2 levels a channel at every sampled
/// point, over the reference's own word boxes, so the veil's maths is checked apart from the
/// few pixels kerning moves a right edge.
#[test]
fn veil_matches_the_reference() {
    let _heavy = HEAVY.read();
    for index in 0..6 {
        let golden = golden(index);
        let file = text(golden, "file").unwrap_or_default();
        let art = compose(golden);
        let side = art.side;
        let boxes = reference_boxes(golden, side);
        let mut veiled = cover_backgrounds::load(book(), art.background, side).expect("the background loads");
        cover_veil::apply(
            &mut veiled,
            &cover_veil::regions(book(), &boxes, side),
            &book().veil,
        );
        let mut worst = 0;
        for (x, y, want) in samples(golden) {
            let got = veiled.get_pixel(x, y);
            let off = max_channel_difference(got, want);
            worst = worst.max(off);
            assert!(off <= 2, "{file} at {x},{y}: {:?} against {want:?}", got.0);
        }
        println!("{file}: worst channel difference {worst}");
    }
}

/// The whole cover as the server lays it out: with its own (kerned) widths the veil's edge
/// moves by a few pixels, which moves a sample by a few levels at most.
#[test]
fn veil_with_the_servers_own_widths_stays_close() {
    let _heavy = HEAVY.read();
    for index in 0..6 {
        let golden = golden(index);
        let file = text(golden, "file").unwrap_or_default();
        let art = compose(golden);
        let veiled = cover_painter::paint(book(), &art, &CoverTypesetter, false).expect("paints");
        for (x, y, want) in samples(golden) {
            let got = veiled.get_pixel(x, y);
            assert!(
                max_channel_difference(got, want) <= 4,
                "{file} at {x},{y}: {:?} against {want:?}",
                got.0
            );
        }
    }
}

// ------------------------------------------------------------ the reference renderer's output

/// How far a reference text measurement may be from this renderer's, in pixels. cosmic-text
/// (harfrust) shapes Inter and the fallback fonts to the same advances as the reference, so
/// widths, left edges and baselines agree to rounding.
const METRIC_TOLERANCE: f64 = 0.01;

/// The least SSIM a finished cover may reach against the reference one.
const MIN_SSIM: f64 = 0.98;

/// How far apart a veiled background (no words) may be from the reference one, in levels a
/// channel. The background's decode, resampling and veil are the same maths in the same order.
const VEIL_TOLERANCE: i64 = 1;

fn reference_music(layout: &Value) -> Option<cover_colors::CoverMusic> {
    let music = &layout["music"];
    (!music.is_null()).then(|| cover_colors::CoverMusic {
        hue: num(music, "hue") as i32,
        chroma: num(music, "chroma"),
        lightness: num(music, "lightness"),
    })
}

fn reference_spec(layout: &Value) -> CoverSpec {
    CoverSpec {
        id: text(layout, "id").expect("an id"),
        name: text(layout, "name").expect("a name"),
        line: text(layout, "line"),
        footer: text(layout, "footer"),
        music: reference_music(layout),
        glyph: None,
    }
}

/// A reference layout's cover as this renderer composes it: its own background pick, except for
/// the goldens, which name theirs.
fn reference_art(layout: &Value) -> CoverArt {
    let side = num(layout, "side") as i32;
    let spec = reference_spec(layout);
    let (background, orientation) = if layout["group"] == "golden" {
        (
            num(layout, "background") as usize,
            num(layout, "orientation") as i32,
        )
    } else {
        (
            cover_backgrounds::choose(book(), spec.music.as_ref(), &spec.id),
            cover_backgrounds::orientation(book(), &spec.id),
        )
    };
    CoverArt {
        side,
        background,
        orientation,
        words: cover_layout::words(&spec, side, &CoverTypesetter, book()),
        glyph: None,
    }
}

fn layouts() -> &'static [Value] {
    REFERENCE["layouts"].as_array().expect("layouts")
}

/// Whether this machine has the system fonts the reference was made with (it was made on a
/// desktop with Noto CJK, Noto Arabic and Hebrew, DejaVu and Noto Sans, and no Symbola).
fn same_fonts_as_reference() -> bool {
    let want: Vec<&str> = REFERENCE["fallbacks"]
        .as_array()
        .expect("fallbacks")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    want == cover_fonts::fallbacks()
}

/// Whether Inter alone sets the text, so it measures the same whatever fonts are installed.
fn inter_only(text: &str) -> bool {
    let inter = cover_fonts::for_weight(600);
    text.chars()
        .all(|c| c.is_whitespace() || cover_fonts::has(&inter, c))
}

/// Whether a reference layout can be compared here: always when only Inter sets it, else only
/// with the same system fonts as the reference's.
fn comparable(layout: &Value) -> bool {
    same_fonts_as_reference()
        || layout["words"]
            .as_array()
            .expect("words")
            .iter()
            .all(|w| inter_only(w["text"].as_str().unwrap_or_default()))
}

fn report_skipped(skipped: usize) {
    if skipped > 0 {
        println!(
            "skipped {skipped} cases set in system fonts other than the reference's {}; installed: {:?}",
            REFERENCE["fallbacks"],
            cover_fonts::fallbacks()
        );
    }
}

/// Every layout the reference renderer made (the goldens, long, cut, right-to-left and foreign
/// names, the contact sheet's lists, at 600 and 1200): the same background and turn, the same
/// blocks, sizes, lines, cuts, boxes and baselines, and each line set in the same font.
#[test]
fn layouts_match_the_reference_renderer() {
    let mut worst = 0f64;
    let mut skipped = 0;
    for layout in layouts() {
        if !comparable(layout) {
            skipped += 1;
            continue;
        }
        let side = num(layout, "side") as i32;
        let art = reference_art(layout);
        let id = text(layout, "id").unwrap_or_default();
        let case = format!("{id} at {side}");
        assert_eq!(
            num(layout, "background") as usize,
            art.background,
            "{case}: background"
        );
        assert_eq!(
            num(layout, "orientation") as i32,
            art.orientation,
            "{case}: orientation"
        );
        let want = layout["words"].as_array().expect("words");
        assert_eq!(want.len(), art.words.len(), "{case}: blocks of words");
        for (w, got) in want.iter().zip(&art.words) {
            assert_eq!(
                role_of(w["role"].as_str().unwrap_or_default()),
                got.role,
                "{case}: role"
            );
            assert_eq!(w["text"].as_str(), Some(got.text.as_str()), "{case}: text");
            assert_eq!(
                num(w, "size") as f32,
                got.type_.size_px,
                "{case} {:?}: size",
                got.role
            );
            assert_eq!(
                num(w, "maxLines") as i32,
                got.type_.max_lines,
                "{case} {:?}: max lines",
                got.role
            );
            assert_eq!(
                num(w, "lines") as i32,
                got.measured.lines,
                "{case} {:?}: lines",
                got.role
            );
            assert_eq!(
                w["cut"].as_bool(),
                Some(got.measured.cut),
                "{case} {:?}: cut",
                got.role
            );
            assert_eq!(
                w["align"].as_str() == Some("Right"),
                got.align == CoverAlign::Right,
                "{case}: align"
            );
            assert_eq!(num(w, "ink") as i32, got.ink, "{case} {:?}: ink", got.role);
            for (key, value) in [
                ("top", got.top),
                ("left", got.left),
                ("height", got.measured.height),
                ("lineHeight", got.type_.line_height),
                ("measuredWidth", got.measured.width),
            ] {
                let off = (num(w, key) - value as f64).abs();
                worst = worst.max(off);
                assert!(
                    off <= METRIC_TOLERANCE,
                    "{case} {:?}: {key} {value} against {}",
                    got.role,
                    w[key]
                );
            }
            let placed = CoverTypesetter.place(got);
            let want_placed = w["placed"].as_array().expect("placed");
            assert_eq!(
                want_placed.len(),
                placed.len(),
                "{case} {:?}: placed lines",
                got.role
            );
            for (a, b) in want_placed.iter().zip(&placed) {
                let line = format!("{case} {:?} '{}'", got.role, b.text);
                assert_eq!(a["text"].as_str(), Some(b.text.as_str()), "{line}: text");
                assert_eq!(a["family"].as_str(), Some(b.family.as_str()), "{line}: font");
                assert_eq!(a["bold"].as_bool(), Some(b.bold), "{line}: bold");
                for (key, value) in [("x", b.x), ("baseline", b.baseline)] {
                    let off = (num(a, key) - value as f64).abs();
                    worst = worst.max(off);
                    assert!(
                        off <= METRIC_TOLERANCE,
                        "{line}: {key} {value} against {}",
                        a[key]
                    );
                }
            }
        }
    }
    println!("worst text metric difference from C#: {worst:.5} px");
    report_skipped(skipped);
}

/// Widths of text on one line, as fitting asks for them, in each weight and script.
#[test]
fn advances_match_the_reference_renderer() {
    let mut worst = 0f64;
    let mut skipped = 0;
    for advance in REFERENCE["advances"].as_array().expect("advances") {
        let text = advance["text"].as_str().expect("text");
        if !same_fonts_as_reference() && !inter_only(text) {
            skipped += 1;
            continue;
        }
        let type_ = CoverType {
            size_px: num(advance, "size") as f32,
            weight: num(advance, "weight") as i32,
            tracking: 0.0,
            line_height: 1.0,
            max_lines: 1,
        };
        let got = cover_fonts::advance(text, &type_) as f64;
        let off = (got - num(advance, "width")).abs();
        worst = worst.max(off);
        assert!(
            off <= METRIC_TOLERANCE,
            "'{text}' at {} weight {}: {got} against {}",
            type_.size_px,
            type_.weight,
            advance["width"]
        );
    }
    println!("worst advance difference from C#: {worst:.5} px");
    report_skipped(skipped);
}

/// The fonts each text is set in, the families behind it, and which system fonts count.
#[test]
fn font_choices_match_the_reference_renderer() {
    if !same_fonts_as_reference() {
        report_skipped(REFERENCE["choices"].as_array().map_or(0, Vec::len));
        return;
    }
    let want: Vec<&str> = REFERENCE["fallbacks"]
        .as_array()
        .expect("fallbacks")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(want, cover_fonts::fallbacks(), "installed fallback families");
    for choice in REFERENCE["choices"].as_array().expect("choices") {
        let text = choice["text"].as_str().expect("text");
        let weight = num(choice, "weight") as i32;
        let got = cover_fonts::for_text(text, weight);
        let fallbacks: Vec<&str> = choice["fallbacks"]
            .as_array()
            .expect("fallbacks")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(
            choice["family"].as_str(),
            Some(got.family.as_str()),
            "'{text}' at {weight}: font"
        );
        assert_eq!(
            choice["bold"].as_bool(),
            Some(got.bold),
            "'{text}' at {weight}: bold"
        );
        assert_eq!(fallbacks, got.fallbacks, "'{text}' at {weight}: fallbacks");
    }
}

fn reference_image(file: &str) -> RgbImage {
    let path = repo("docs/rust-migration/fixtures/covers/images").join(file);
    image::open(&path)
        .unwrap_or_else(|e| panic!("{} does not open: {e}", path.display()))
        .to_rgb8()
}

/// Where failing comparisons leave their pictures: target/cover-golden-diffs/.
fn diff_dir() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo("target"));
    target.join("cover-golden-diffs")
}

/// Writes this renderer's picture, the reference, and their difference (four times as strong)
/// side by side, for looking at a failure.
fn write_diff(name: &str, got: &RgbImage, want: &RgbImage) -> PathBuf {
    let (w, h) = got.dimensions();
    let mut sheet = RgbImage::new(w * 3, h);
    image::imageops::replace(&mut sheet, got, 0, 0);
    image::imageops::replace(&mut sheet, want, i64::from(w), 0);
    for (x, y, p) in got.enumerate_pixels() {
        let q = want.get_pixel(x, y);
        let d = |k: usize| ((p.0[k] as i32 - q.0[k] as i32).unsigned_abs() * 4).min(255) as u8;
        sheet.put_pixel(2 * w + x, y, Rgb([d(0), d(1), d(2)]));
    }
    let dir = diff_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!(
        "{}.png",
        name.trim_end_matches(".webp").trim_end_matches(".jpg")
    ));
    let _ = sheet.save(&path);
    path
}

/// The mean structural similarity of two pictures: per channel over every 8 by 8 window (the
/// usual constants for 8-bit levels), the least of the three channels.
fn ssim(a: &RgbImage, b: &RgbImage) -> f64 {
    assert_eq!(
        a.dimensions(),
        b.dimensions(),
        "SSIM compares pictures of one size"
    );
    let (w, h) = (a.width() as usize, a.height() as usize);
    const WINDOW: usize = 8;
    let c1 = (0.01f64 * 255.0).powi(2);
    let c2 = (0.03f64 * 255.0).powi(2);
    let mut least = f64::INFINITY;
    for channel in 0..3 {
        // Summed-area tables of x, y, x², y² and xy.
        let stride = w + 1;
        let mut tables = vec![[0f64; 5]; stride * (h + 1)];
        for y in 0..h {
            let mut row = [0f64; 5];
            for x in 0..w {
                let p = a.get_pixel(x as u32, y as u32).0[channel] as f64;
                let q = b.get_pixel(x as u32, y as u32).0[channel] as f64;
                for (k, v) in [p, q, p * p, q * q, p * q].into_iter().enumerate() {
                    row[k] += v;
                }
                let above = tables[y * stride + x + 1];
                tables[(y + 1) * stride + x + 1] = std::array::from_fn(|k| above[k] + row[k]);
            }
        }
        let n = (WINDOW * WINDOW) as f64;
        let (mut sum, mut count) = (0f64, 0usize);
        for y in 0..=h - WINDOW {
            for x in 0..=w - WINDOW {
                let at = |yy: usize, xx: usize| tables[yy * stride + xx];
                let (s1, s2, s3, s4) = (
                    at(y + WINDOW, x + WINDOW),
                    at(y, x + WINDOW),
                    at(y + WINDOW, x),
                    at(y, x),
                );
                let s: [f64; 5] = std::array::from_fn(|k| s1[k] - s2[k] - s3[k] + s4[k]);
                let (mx, my) = (s[0] / n, s[1] / n);
                let vx = s[2] / n - mx * mx;
                let vy = s[3] / n - my * my;
                let cov = s[4] / n - mx * my;
                sum +=
                    ((2.0 * mx * my + c1) * (2.0 * cov + c2)) / ((mx * mx + my * my + c1) * (vx + vy + c2));
                count += 1;
            }
        }
        least = least.min(sum / count as f64);
    }
    least
}

fn max_difference(a: &RgbImage, b: &RgbImage) -> i64 {
    a.pixels()
        .zip(b.pixels())
        .map(|(p, q)| {
            (0..3)
                .map(|k| (p.0[k] as i64 - q.0[k] as i64).abs())
                .max()
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0)
}

/// Compares a picture with a reference by SSIM, leaving the pictures under
/// target/cover-golden-diffs/ when it falls short (or always, with OCTO_COVER_DIFFS set).
fn assert_looks_like(name: &str, got: &RgbImage, want: &RgbImage) -> f64 {
    assert_eq!(got.dimensions(), want.dimensions(), "{name}: size");
    let score = ssim(got, want);
    if score < MIN_SSIM || std::env::var_os("OCTO_COVER_DIFFS").is_some() {
        let path = write_diff(name, got, want);
        assert!(
            score >= MIN_SSIM,
            "{name}: SSIM {score:.4} under {MIN_SSIM}; see {}",
            path.display()
        );
    }
    score
}

/// The veiled backgrounds of the goldens, before any words, against the reference ones pixel for
/// pixel.
#[test]
fn veils_match_the_reference_renderer() {
    let _heavy = HEAVY.read();
    for layout in layouts()
        .iter()
        .filter(|l| !l["veilImage"].is_null() && comparable(l))
    {
        let file = layout["veilImage"].as_str().expect("a file");
        let got =
            cover_painter::paint(book(), &reference_art(layout), &CoverTypesetter, false).expect("paints");
        let want = reference_image(file);
        let off = max_difference(&got, &want);
        println!("{file}: worst channel difference {off}");
        if off > VEIL_TOLERANCE {
            let path = write_diff(file, &got, &want);
            panic!("{file}: a channel {off} levels from C#; see {}", path.display());
        }
    }
}

/// The finished covers (background, veil and words) against the reference ones, by SSIM: the
/// words' edges are smoothed by another rasterizer (tiny-skia), so they may differ by
/// a few levels where they meet the colors.
#[test]
fn covers_look_like_the_reference_renderer() {
    let _heavy = HEAVY.read();
    let mut least = f64::INFINITY;
    let skipped = layouts()
        .iter()
        .filter(|l| !l["image"].is_null() && !comparable(l))
        .count();
    for layout in layouts()
        .iter()
        .filter(|l| !l["image"].is_null() && comparable(l))
    {
        let file = layout["image"].as_str().expect("a file");
        let got =
            cover_painter::paint(book(), &reference_art(layout), &CoverTypesetter, true).expect("paints");
        let score = assert_looks_like(file, &got, &reference_image(file));
        println!(
            "{file}: SSIM {score:.5}, worst channel difference {}",
            max_difference(&got, &reference_image(file))
        );
        least = least.min(score);
    }
    println!("least SSIM against C#: {least:.5}");
    report_skipped(skipped);
}

/// A cover as the server serves it, JPEG and all, against the reference JPEG.
#[test]
fn served_cover_looks_like_the_reference_one() {
    let _heavy = HEAVY.read();
    let service = super::CoverArtService::new(None);
    let spec = super::CoverArtService::spec(
        "Rock Radio",
        None,
        Some(super::list_kinds::RADIO),
        None,
        service.fallback_music("Rock Radio", "Rock Radio"),
    );
    let served = service.render(&spec, 600, true).expect("renders");
    let got = image::load_from_memory(&served)
        .expect("the served cover decodes")
        .to_rgb8();
    let score = assert_looks_like("rock-radio-600.jpg", &got, &reference_image("rock-radio-600.jpg"));
    println!("rock-radio-600.jpg: SSIM {score:.5}");
}
