//! QR images → terminal (feature `qr`, on by default).
//!
//! Login QRs from chat services are stylized (rounded modules, coloured eyes),
//! which trips generic detectors. They are, however, axis-aligned and square,
//! so we sample the module grid directly: find the symbol's bounding box, try
//! every legal size (21, 25, … 177) and keep the one whose finder + timing
//! patterns match best. The sampled matrix is drawn as-is, so we never need to
//! understand the payload.

use crate::error::{Error, Result};

/// A sampled QR symbol, row-major, `true` = dark module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QrMatrix {
    /// Modules per side.
    pub size: usize,
    /// `size * size` modules, row-major.
    pub bits: Vec<bool>,
}

impl QrMatrix {
    /// Is the module at column `x`, row `y` dark?
    pub fn get(&self, x: usize, y: usize) -> bool {
        self.bits[y * self.size + x]
    }

    /// Encode `text` (e.g. a `tg://login?token=…` URL) as a QR matrix.
    pub fn from_text(text: &str) -> Result<QrMatrix> {
        let code = qrcode::QrCode::new(text.as_bytes()).map_err(|e| Error::Qr(e.to_string()))?;
        let size = code.width();
        let bits = code.to_colors().iter().map(|c| *c == qrcode::Color::Dark).collect();
        Ok(QrMatrix { size, bits })
    }

    /// Decode the payload (best effort; used to validate sampling).
    pub fn decode(&self) -> Result<String> {
        let grid = rqrr::SimpleGrid::from_func(self.size, |x, y| self.get(x, y));
        let (_meta, content) = rqrr::Grid::new(grid)
            .decode()
            .map_err(|e| Error::Qr(format!("QR decode: {e:?}")))?;
        Ok(content)
    }
}

const DARK_THRESHOLD: f32 = 160.0;
/// Fraction of finder/timing modules that must match to accept a size.
const MIN_PATTERN_MATCH: f32 = 0.9;

fn finder_expected(dx: usize, dy: usize) -> bool {
    // 7x7: dark border (ring 3), light ring 2, dark 3x3 core.
    let d = (dx as i32 - 3).abs().max((dy as i32 - 3).abs());
    d != 2
}

/// Sample the module grid out of a QR PNG (the `png` of [`QrEvent::Generated`](crate::QrEvent::Generated)).
pub fn sample_png(png: &[u8]) -> Result<QrMatrix> {
    let img = image::load_from_memory(png).map_err(|e| Error::Qr(e.to_string()))?.to_rgba8();
    let (w, h) = img.dimensions();
    let dark = |x: u32, y: u32| -> bool {
        let [r, g, b, a] = img.get_pixel(x.min(w - 1), y.min(h - 1)).0;
        let alpha = a as f32 / 255.0;
        let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
        luma * alpha + 255.0 * (1.0 - alpha) < DARK_THRESHOLD
    };

    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for y in 0..h {
        for x in 0..w {
            if dark(x, y) {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if x0 > x1 || y0 > y1 {
        return Err(Error::Qr("image has no dark pixels".into()));
    }
    let side = ((x1 - x0 + 1).max(y1 - y0 + 1)) as f32;

    let mut best: Option<(f32, QrMatrix)> = None;
    for n in (21..=177).step_by(4) {
        let m = side / n as f32;
        if m < 2.0 {
            break;
        }
        let bits: Vec<bool> = (0..n * n)
            .map(|i| {
                let (c, r) = (i % n, i / n);
                dark(
                    (x0 as f32 + (c as f32 + 0.5) * m) as u32,
                    (y0 as f32 + (r as f32 + 0.5) * m) as u32,
                )
            })
            .collect();
        let mx = QrMatrix { size: n, bits };

        let (mut ok, mut total) = (0u32, 0u32);
        for (ox, oy) in [(0, 0), (n - 7, 0), (0, n - 7)] {
            for dy in 0..7 {
                for dx in 0..7 {
                    total += 1;
                    ok += (mx.get(ox + dx, oy + dy) == finder_expected(dx, dy)) as u32;
                }
            }
        }
        for i in 8..n - 8 {
            total += 2;
            ok += (mx.get(i, 6) == (i % 2 == 0)) as u32;
            ok += (mx.get(6, i) == (i % 2 == 0)) as u32;
        }
        let score = ok as f32 / total as f32;
        if best.as_ref().map_or(true, |(s, _)| score > *s) {
            best = Some((score, mx));
        }
    }

    match best {
        Some((score, mx)) if score >= MIN_PATTERN_MATCH => Ok(mx),
        Some((score, _)) => Err(Error::Qr(format!(
            "no QR grid fits well (best match {:.0}%)",
            score * 100.0
        ))),
        None => Err(Error::Qr("QR too small to sample".into())),
    }
}

/// Draw with half blocks (2 module rows per text line) using explicit ANSI
/// black-on-white, so polarity is right on both dark and light themes.
pub fn render(mx: &QrMatrix, quiet: usize) -> String {
    let n = mx.size;
    let full = n + 2 * quiet;
    let at = |x: usize, y: usize| -> bool {
        x >= quiet && y >= quiet && x < quiet + n && y < quiet + n && mx.get(x - quiet, y - quiet)
    };
    let mut out = String::new();
    for row in (0..full).step_by(2) {
        out.push_str("\x1b[30;107m");
        for x in 0..full {
            let top = at(x, row);
            let bottom = row + 1 < full && at(x, row + 1);
            out.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rasterize a QR like a chat service would: big modules, rounded-ish
    /// (inset) dots, coloured finder cores — then sample it back.
    fn stylized_png(payload: &str) -> (Vec<u8>, QrMatrix) {
        let code = qrcode::QrCode::new(payload.as_bytes()).unwrap();
        let n = code.width();
        let want = QrMatrix {
            size: n,
            bits: code
                .to_colors()
                .iter()
                .map(|c| *c == qrcode::Color::Dark)
                .collect(),
        };
        let (scale, margin) = (11u32, 9u32);
        let size = n as u32 * scale + 2 * margin;
        let mut img = image::RgbaImage::from_pixel(size, size, image::Rgba([255, 255, 255, 255]));
        for y in 0..n {
            for x in 0..n {
                if !want.get(x, y) {
                    continue;
                }
                let in_core = |ox: usize, oy: usize| {
                    (2..=4).contains(&(x.wrapping_sub(ox)))
                        && (2..=4).contains(&(y.wrapping_sub(oy)))
                };
                let blue = in_core(0, 0) || in_core(n - 7, 0) || in_core(0, n - 7);
                let px = if blue {
                    image::Rgba([20, 110, 240, 255])
                } else {
                    image::Rgba([0, 0, 0, 255])
                };
                // inset by 1px on each side → gaps between modules, like rounded dots
                for dy in 1..scale - 1 {
                    for dx in 1..scale - 1 {
                        img.put_pixel(
                            margin + x as u32 * scale + dx,
                            margin + y as u32 * scale + dy,
                            px,
                        );
                    }
                }
            }
        }
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        (png, want)
    }

    #[test]
    fn samples_stylized_qr_exactly() {
        let payload = "https://id.zalo.me/qr/some-login-code-1234567890";
        let (png, want) = stylized_png(payload);
        let got = sample_png(&png).unwrap();
        assert_eq!(got.size, want.size);
        assert_eq!(got, want);
        assert_eq!(got.decode().unwrap(), payload);
    }

    #[test]
    fn from_text_roundtrips_through_decode() {
        let url = "tg://login?token=AQID-_8B";
        assert_eq!(QrMatrix::from_text(url).unwrap().decode().unwrap(), url);
    }

    #[test]
    fn render_has_expected_shape() {
        let (png, _) = stylized_png("hello");
        let mx = sample_png(&png).unwrap();
        let art = render(&mx, 2);
        assert_eq!(art.lines().count(), (mx.size + 4 + 1) / 2);
    }

    /// A real QR saved from a login:
    /// `ZALO_RS_QR_SAMPLE=/path/to/qr.png cargo test -p zca-rust real_qr -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_qr_samples_and_decodes() {
        let path = std::env::var("ZALO_RS_QR_SAMPLE").expect("set ZALO_RS_QR_SAMPLE");
        let mx = sample_png(&std::fs::read(path).unwrap()).unwrap();
        let content = mx.decode().unwrap();
        println!("size {} → decoded {} chars", mx.size, content.len());
    }
}
