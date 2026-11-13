//! A picture drawn in the terminal, on `p`.
//!
//! **The one exception to "Stories are handed to the system viewer"**
//! (`AGENTS.md`), and a narrow one: a still picture, in a box over the view,
//! when its key is pressed. Enter still hands the file to the viewer the
//! person already uses; this is for knowing which picture a row is before
//! opening it.
//!
//! **Never as the selection moves.** A preview is the very read Enter makes:
//! the file is fetched from the CDN into the session's scratch directory,
//! where Enter and `d` find it afterwards, and is fetched once. Drawn for
//! every row the cursor passes it would be a download per keystroke of
//! pictures nobody asked to see. Nothing marks anything as seen, here or
//! there: looking is a download, not the call that says "seen".
//!
//! **Two pixels to a cell.** Each cell is an upper half block, its
//! foreground the pixel above and its background the pixel below; a cell is
//! about twice as tall as it is wide, so the halves come out square. It is
//! 24-bit color, the one place a view names a color that is not a role of
//! `ui/theme.rs`: the colors are the picture's. Not sixel and not kitty's
//! protocol, which draw far better: to know that a terminal takes either it
//! has to be asked, and the answer comes back on the terminal's input in the
//! middle of the keys.
//!
//! **Behind the `preview` feature, which is not a default**, until what it
//! costs the binary is measured. A build without it binds `p` to nothing and
//! names it in no help ([`AVAILABLE`]); everything here still compiles, less
//! the decoder, so the drawing is tested in every build.
//!
//! **What is said instead of drawn**, in the note line and before anything
//! is fetched: a run that draws no colors, a terminal that does not say it
//! shows 24-bit color (`theme::truecolor`), a video. And after: a file that
//! is no JPEG, which is what the decoder reads and what Instagram serves
//! almost everywhere (`media::extension_of`).
//!
//! **The bytes are somebody else's**, and the release profile aborts on a
//! panic. So: at most [`MAX_BYTES`] in, at most [`MAX_SIDE`] pixels a side,
//! refused from the header before a pixel is decoded, and a picture that
//! will not decode is a note. The decoder is built without its SIMD, which
//! is the whole of its `unsafe` (`zune-jpeg` 0.5.15 forbids `unsafe_code`
//! with neither `x86` nor `neon` on): one picture is decoded per key press,
//! and nobody waits on it.

use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Color;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType};

use crate::media::Kind;
use crate::ui::browser::input::{self, Raw};
use crate::ui::help;
use crate::ui::theme;
use crate::ui::tui::{self, Tui};

/// Whether this build draws previews at all.
pub const AVAILABLE: bool = cfg!(feature = "preview");

/// The most bytes of a file that are decoded: the cap the CDN client puts on
/// a picture it downloads (`snob_ig::client::cdn`, `MAX_ASSET_BYTES`). A
/// story's video may be larger, and is not a picture.
const MAX_BYTES: usize = 8 * 1024 * 1024;

/// The most pixels a side. A bound on memory and not a claim about what
/// Instagram serves: 4096 x 4096 in RGB is 48 MiB, and the decoder refuses
/// more from the header.
const MAX_SIDE: usize = 4096;

/// `p`, for the tables `?` shows.
pub const KEY: help::Key = help::key("p", "draw the picture here", KeyCode::Char('p'));

/// `keys` with [`KEY`] after the key that saves, in a build that draws
/// previews; `keys` as they are in one that does not, where `p` is nothing
/// to name.
pub fn with_key(keys: &[help::Key]) -> Vec<help::Key> {
    let mut all = keys.to_vec();
    if AVAILABLE {
        let after_save = all
            .iter()
            .position(|key| key.press.code == KeyCode::Char('d'))
            .map_or(all.len(), |at| at + 1);
        all.insert(after_save, KEY);
    }
    all
}

/// Why `p` draws nothing here, when it does not, decided before anything is
/// fetched; `None` is go on. `kind` is what the row is said to be, for the
/// rows that say. In a build without previews the reason is empty: a note
/// with nothing in it, which is `p` doing nothing.
pub fn refused(kind: Option<Kind>) -> Option<String> {
    refusal(AVAILABLE, tui::colors_enabled(), theme::truecolor(), kind).map(str::to_string)
}

fn refusal(
    available: bool,
    colors: bool,
    truecolor: bool,
    kind: Option<Kind>,
) -> Option<&'static str> {
    if !available {
        Some("")
    } else if kind == Some(Kind::Video) {
        Some("A video has no preview: enter opens it in the viewer")
    } else if !colors {
        Some("A preview is drawn in colors, and this run draws none")
    } else if !truecolor {
        Some("A preview needs 24-bit color: the terminal does not say it has it (COLORTERM)")
    } else {
        None
    }
}

/// A decoded picture: `width * height` pixels of red, green and blue.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pixels {
    width: usize,
    height: usize,
    rgb: Vec<u8>,
}

impl Pixels {
    /// From what a decoder handed over: one, three or four samples a pixel,
    /// gray, RGB, or RGB and something after it. Anything else is not a
    /// picture this draws.
    fn of(width: usize, height: usize, samples: Vec<u8>) -> Option<Self> {
        let pixels = width.checked_mul(height).filter(|n| *n > 0)?;
        let rgb = match samples.len() / pixels {
            _ if !samples.len().is_multiple_of(pixels) => return None,
            3 => samples,
            1 => samples.iter().flat_map(|gray| [*gray; 3]).collect(),
            4 => samples
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|[red, green, blue, _]| [*red, *green, *blue])
                .collect(),
            _ => return None,
        };
        Some(Self { width, height, rgb })
    }

    fn at(&self, x: usize, y: usize) -> (u8, u8, u8) {
        let at = (y * self.width + x) * 3;
        match self.rgb.get(at..at + 3) {
            Some(&[red, green, blue]) => (red, green, blue),
            _ => (0, 0, 0),
        }
    }

    /// The picture at `width` by `height`, each pixel the mean of the ones
    /// it stands for. A mean and not the nearest one: a photograph taken
    /// down to a hundred columns by dropping pixels is noise.
    fn scaled(&self, width: usize, height: usize) -> Pixels {
        let width = width.clamp(1, self.width);
        let height = height.clamp(1, self.height);
        let mut rgb = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            let top = y * self.height / height;
            let bottom = ((y + 1) * self.height / height).max(top + 1);
            for x in 0..width {
                let left = x * self.width / width;
                let right = ((x + 1) * self.width / width).max(left + 1);
                let mut sum = [0u64; 3];
                for from_y in top..bottom {
                    for from_x in left..right {
                        let (red, green, blue) = self.at(from_x, from_y);
                        sum[0] += u64::from(red);
                        sum[1] += u64::from(green);
                        sum[2] += u64::from(blue);
                    }
                }
                let count = ((bottom - top) * (right - left)) as u64;
                rgb.extend(sum.map(|channel| (channel / count) as u8));
            }
        }
        Pixels { width, height, rgb }
    }
}

/// The size, in pixels, a picture of `width` by `height` is drawn at in
/// `cols` columns and `rows` rows of cells: two pixels to a row, its shape
/// kept, as large as fits and never larger than it is.
fn fitted(width: usize, height: usize, cols: usize, rows: usize) -> (usize, usize) {
    let (cols, lines) = (cols.min(width), (rows * 2).min(height));
    if width == 0 || height == 0 || cols == 0 || lines == 0 {
        return (0, 0);
    }
    // Whichever of the two bounds is met first decides.
    if width * lines <= height * cols {
        ((width * lines / height).max(1), lines)
    } else {
        (cols, (height * cols / width).max(1))
    }
}

/// Draws `pixels` from the top left of `area`, two rows of them to a row of
/// cells. A last row with no pixel under it keeps the background it had.
fn paint(buffer: &mut Buffer, area: Rect, pixels: &Pixels) {
    let color = |(red, green, blue)| Color::Rgb(red, green, blue);
    for row in 0..usize::from(area.height) {
        let upper = row * 2;
        if upper >= pixels.height {
            break;
        }
        for col in 0..usize::from(area.width).min(pixels.width) {
            let Some(cell) = buffer.cell_mut((area.x + col as u16, area.y + row as u16)) else {
                continue;
            };
            cell.set_char('▀');
            cell.set_fg(color(pixels.at(col, upper)));
            if upper + 1 < pixels.height {
                cell.set_bg(color(pixels.at(col, upper + 1)));
            }
        }
    }
}

/// The box: the picture as large as the screen allows inside a border, its
/// name and its size in pixels on the top edge. `drawn` keeps the picture at
/// the size it was last drawn, so that an idle tick does not scale it again.
fn modal(
    frame: &mut Frame<'_>,
    title: &str,
    pixels: &Pixels,
    drawn: &mut Option<Pixels>,
    colors: bool,
) {
    let dim = tui::paint(colors, tui::DIM);
    let bold = tui::paint(colors, tui::BOLD);
    tui::veil(frame, colors);

    // The border, and a column and a row of the view left showing around it.
    let screen = frame.area();
    let cols = usize::from(screen.width.saturating_sub(2 + 2));
    let rows = usize::from(screen.height.saturating_sub(2 + 2));
    let (width, height) = fitted(pixels.width, pixels.height, cols, rows);
    if width == 0 || height == 0 {
        return;
    }
    if drawn
        .as_ref()
        .is_none_or(|was| (was.width, was.height) != (width, height))
    {
        *drawn = Some(pixels.scaled(width, height));
    }
    let Some(scaled) = drawn.as_ref() else {
        return;
    };

    // Wide enough for its two edges to be read, however thin the picture.
    let closes = " any key closes ";
    let named = format!(" {title} · {}x{} ", pixels.width, pixels.height);
    let wide = width
        .max(closes.len())
        .max(console::measure_text_width(&named))
        .min(cols);
    let area = screen.centered(
        Constraint::Length(wide as u16 + 2),
        Constraint::Length(height.div_ceil(2) as u16 + 2),
    );
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim)
        .title_top(Line::from(Span::styled(named, bold)))
        .title_bottom(Line::from(Span::styled(closes, dim)).centered());
    let inner = tui::open_modal(frame, area, block);
    let picture = inner.centered_horizontally(Constraint::Length(width as u16));
    paint(frame.buffer_mut(), picture, scaled);
}

/// Draws the picture in `bytes` over the view `under` draws, until a key is
/// pressed, and hands back the note to leave: empty when it was drawn, the
/// reason when it could not be. `title` is the picture's name, already safe
/// to print. The key that closes the box is spent closing it.
pub fn show(
    tui: &mut Tui,
    title: &str,
    bytes: &[u8],
    mut under: impl FnMut(&mut Frame<'_>),
) -> Result<String> {
    let pixels = match decode(bytes) {
        Ok(pixels) => pixels,
        Err(why) => return Ok(why),
    };
    let colors = tui::colors_enabled();
    let mut drawn = None;
    loop {
        tui.terminal.draw(|frame| {
            under(frame);
            modal(frame, title, &pixels, &mut drawn, colors);
        })?;
        if let Raw::Key(_) = input::read(input::tick()).map_err(input::unreadable)? {
            return Ok(String::new());
        }
    }
}

/// [`show`] for a file a fetch may have left in the scratch directory: the
/// fetch's failure, or the file's, is the note.
pub fn show_file(
    tui: &mut Tui,
    fetched: Result<PathBuf>,
    under: impl FnMut(&mut Frame<'_>),
) -> Result<String> {
    let path = match fetched {
        Ok(path) => path,
        Err(e) => return Ok(format!("Could not fetch it: {e}")),
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => return Ok(format!("Could not read {}: {e}", path.display())),
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    show(tui, &name, &bytes, under)
}

/// What the bytes are, for the sentence that says they are no JPEG.
fn named(bytes: &[u8]) -> &'static str {
    match crate::media::extension_of(bytes) {
        "mp4" => "a video",
        "webp" => "a WebP",
        "png" => "a PNG",
        _ => "something else",
    }
}

/// The picture in `bytes`, or the note that says why there is none.
#[cfg(feature = "preview")]
fn decode(bytes: &[u8]) -> std::result::Result<Pixels, String> {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    if bytes.len() > MAX_BYTES {
        return Err(format!(
            "Too large to draw here: {} MiB, and the most is {}",
            bytes.len() / (1024 * 1024),
            MAX_BYTES / (1024 * 1024)
        ));
    }
    if !bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Err(format!(
            "Only a JPEG is drawn here, and this is {}: enter opens it in the viewer",
            named(bytes)
        ));
    }
    let options = DecoderOptions::default()
        .set_max_width(MAX_SIDE)
        .set_max_height(MAX_SIDE)
        .jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    let samples = decoder
        .decode()
        .map_err(|e| format!("Could not decode the picture: {e}"))?;
    decoder
        .info()
        .and_then(|info| Pixels::of(usize::from(info.width), usize::from(info.height), samples))
        .ok_or_else(|| "Could not decode the picture: it has no pixels to draw".to_string())
}

/// A build without the decoder draws nothing and says nothing: `p` is bound
/// to nothing there ([`refused`] answers before anything gets this far).
#[cfg(not(feature = "preview"))]
fn decode(bytes: &[u8]) -> std::result::Result<Pixels, String> {
    let _ = (
        bytes,
        MAX_BYTES,
        MAX_SIDE,
        named(bytes),
        Pixels::of(0, 0, Vec::new()),
    );
    Err(String::new())
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::ui::browser::input::{Action, action_of};

    const RED: (u8, u8, u8) = (255, 0, 0);
    const GREEN: (u8, u8, u8) = (0, 255, 0);
    const BLUE: (u8, u8, u8) = (0, 0, 255);
    const WHITE: (u8, u8, u8) = (255, 255, 255);

    fn picture(width: usize, rows: &[&[(u8, u8, u8)]]) -> Pixels {
        let rgb = rows
            .iter()
            .flat_map(|row| row.iter())
            .flat_map(|(red, green, blue)| [*red, *green, *blue])
            .collect();
        Pixels::of(width, rows.len(), rgb).unwrap()
    }

    fn rgb((red, green, blue): (u8, u8, u8)) -> Color {
        Color::Rgb(red, green, blue)
    }

    /// One cell holds two pixels: the upper one is the half block's
    /// foreground and the lower one its background.
    #[test]
    fn a_picture_is_drawn_two_pixels_to_a_cell() {
        let pixels = picture(2, &[&[RED, GREEN], &[BLUE, WHITE], &[WHITE, RED]]);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 3));
        paint(&mut buffer, Rect::new(1, 0, 3, 3), &pixels);

        let cell = &buffer[(1, 0)];
        assert_eq!(cell.symbol(), "▀");
        assert_eq!((cell.fg, cell.bg), (rgb(RED), rgb(BLUE)));
        let cell = &buffer[(2, 0)];
        assert_eq!((cell.fg, cell.bg), (rgb(GREEN), rgb(WHITE)));
        // The third row of pixels has none under it: the cell keeps the
        // background it had.
        let cell = &buffer[(1, 1)];
        assert_eq!(cell.symbol(), "▀");
        assert_eq!((cell.fg, cell.bg), (rgb(WHITE), Color::Reset));
        // Nothing is drawn past the picture, in either direction.
        assert_eq!(buffer[(0, 0)].symbol(), " ");
        assert_eq!(buffer[(3, 0)].symbol(), " ");
        assert_eq!(buffer[(1, 2)].symbol(), " ");
    }

    /// A picture is drawn as large as fits, in its own shape, and never
    /// larger than it is.
    #[test]
    fn a_picture_keeps_its_shape_and_fits_the_cells() {
        // 1080 by 1350 in 100 columns and 40 rows: the 80 half rows decide.
        assert_eq!(fitted(1080, 1350, 100, 40), (64, 80));
        // A wide one in the same cells: the columns decide.
        assert_eq!(fitted(1600, 400, 100, 40), (100, 25));
        // A small one is not blown up.
        assert_eq!(fitted(10, 6, 100, 40), (10, 6));
        // And one too thin to have a column still gets one.
        assert_eq!(fitted(1, 1000, 100, 10), (1, 20));
        assert_eq!(fitted(1080, 1350, 0, 40), (0, 0));
        assert_eq!(fitted(0, 0, 100, 40), (0, 0));
    }

    /// Each pixel of the smaller picture is the mean of the ones it stands
    /// for.
    #[test]
    fn a_smaller_picture_is_the_mean_of_the_larger() {
        let pixels = picture(
            4,
            &[
                &[RED, RED, (0, 0, 0), (0, 0, 0)],
                &[BLUE, BLUE, WHITE, WHITE],
            ],
        );
        let half = pixels.scaled(2, 1);
        assert_eq!((half.width, half.height), (2, 1));
        assert_eq!(half.at(0, 0), (127, 0, 127));
        assert_eq!(half.at(1, 0), (127, 127, 127));
        // Asked for more than it has, it is itself.
        assert_eq!(pixels.scaled(9, 9), pixels);
    }

    /// Gray samples and samples with a fourth channel are pictures too; a
    /// count that is no whole number of pixels is not.
    #[test]
    fn what_a_decoder_hands_over_is_read_by_its_samples_a_pixel() {
        let gray = Pixels::of(2, 1, vec![10, 200]).unwrap();
        assert_eq!(gray.at(1, 0), (200, 200, 200));
        let with_alpha = Pixels::of(1, 1, vec![1, 2, 3, 255]).unwrap();
        assert_eq!(with_alpha.at(0, 0), (1, 2, 3));
        assert!(Pixels::of(2, 2, vec![0; 7]).is_none());
        assert!(
            Pixels::of(2, 2, vec![0; 8]).is_none(),
            "two samples a pixel"
        );
        assert!(Pixels::of(0, 2, Vec::new()).is_none());
    }

    /// The box holds the picture, names it and its size, and says how to
    /// close it.
    #[test]
    fn the_box_names_the_picture_and_its_size() {
        let pixels = picture(2, &[&[RED, GREEN], &[BLUE, WHITE]]);
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let mut drawn = None;
        terminal
            .draw(|frame| modal(frame, "someone-3.jpg", &pixels, &mut drawn, true))
            .unwrap();
        let screen: String = (0..12)
            .map(|y| tui::row_text(&terminal, y) + "\n")
            .collect();
        assert!(screen.contains("someone-3.jpg · 2x2"), "{screen}");
        assert!(screen.contains("any key closes"), "{screen}");
        assert_eq!(screen.matches('▀').count(), 2, "{screen}");
        assert_eq!(drawn, Some(pixels));
    }

    /// What `p` says instead of drawing, before anything is fetched: a
    /// video, a run with no colors, a terminal that does not say truecolor.
    #[test]
    fn a_terminal_that_does_not_say_truecolor_gets_a_note_and_no_picture() {
        assert_eq!(refusal(true, true, true, Some(Kind::Photo)), None);
        assert_eq!(refusal(true, true, true, None), None);
        let video = refusal(true, true, true, Some(Kind::Video)).unwrap();
        assert!(video.contains("video"), "{video}");
        let plain = refusal(true, false, true, Some(Kind::Photo)).unwrap();
        assert!(plain.contains("colors"), "{plain}");
        let few = refusal(true, true, false, Some(Kind::Photo)).unwrap();
        assert!(few.contains("24-bit"), "{few}");
    }

    /// A build without the feature binds `p` to nothing: no note, no key in
    /// any help, whatever the terminal is.
    #[test]
    fn without_the_feature_p_does_nothing() {
        assert_eq!(refusal(false, true, true, Some(Kind::Photo)), Some(""));
        assert_eq!(refusal(false, false, false, Some(Kind::Video)), Some(""));
        let keys = [help::MOVE, crate::ui::stories::SAVE_KEY, help::QUIT];
        let shown = with_key(&keys);
        if AVAILABLE {
            assert_eq!(shown.len(), 4);
            assert_eq!(shown[2].keys, "p", "after the key that saves");
        } else {
            assert_eq!(shown.len(), 3);
            assert!(shown.iter().all(|key| key.keys != "p"));
            assert_eq!(decode(b"\xff\xd8\xff"), Err(String::new()));
        }
        // The key the table names is the one the shared binding reads.
        assert_eq!(action_of(KEY.press), Action::Preview);
    }

    /// The smallest JPEG there is to write by hand: 8 by 8, one gray
    /// component, a quantization table of ones, and Huffman tables of one
    /// code each, with one block whose every coefficient is zero. It decodes
    /// to mid gray.
    #[cfg(feature = "preview")]
    fn gray_jpeg() -> Vec<u8> {
        let mut jpeg = vec![0xff, 0xd8];
        // The quantization table.
        jpeg.extend([0xff, 0xdb, 0x00, 0x43, 0x00]);
        jpeg.extend([1u8; 64]);
        // The frame: 8 bits, 8 by 8, one component sampled 1x1, table 0.
        jpeg.extend([
            0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x08, 0x00, 0x08, 0x01, 0x01, 0x11, 0x00,
        ]);
        // Two Huffman tables, DC and AC: one code, of one bit, for symbol 0.
        for class in [0x00, 0x10] {
            jpeg.extend([0xff, 0xc4, 0x00, 0x14, class, 0x01]);
            jpeg.extend([0u8; 15]);
            jpeg.push(0x00);
        }
        // The scan, and its one byte: a zero DC difference, the end of the
        // block, and ones to the byte's end.
        jpeg.extend([0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00]);
        jpeg.extend([0x3f, 0xff, 0xd9]);
        jpeg
    }

    /// A JPEG is decoded to its pixels, in RGB whatever it was stored in.
    #[cfg(feature = "preview")]
    #[test]
    fn a_jpeg_is_decoded_to_its_pixels() {
        let pixels = decode(&gray_jpeg()).unwrap();
        assert_eq!((pixels.width, pixels.height), (8, 8));
        assert_eq!(pixels.rgb.len(), 8 * 8 * 3);
        assert_eq!(pixels.at(0, 0), (128, 128, 128));
        assert_eq!(pixels.at(7, 7), (128, 128, 128));
    }

    /// Bytes that are no JPEG are said, by what they are, and not drawn.
    #[cfg(feature = "preview")]
    #[test]
    fn a_file_that_is_no_jpeg_is_said_not_drawn() {
        let png = decode(b"\x89PNG\r\n\x1a\n").unwrap_err();
        assert!(
            png.contains("Only a JPEG") && png.contains("a PNG"),
            "{png}"
        );
        let webp = decode(b"RIFF\0\0\0\0WEBPVP8 ").unwrap_err();
        assert!(webp.contains("a WebP"), "{webp}");
        let mut mp4 = vec![0, 0, 0, 0x18];
        mp4.extend_from_slice(b"ftypmp42");
        assert!(decode(&mp4).unwrap_err().contains("a video"));
        assert!(decode(b"").unwrap_err().contains("something else"));
        // A JPEG cut short is a JPEG that will not decode, and says so.
        let cut = decode(&gray_jpeg()[..40]).unwrap_err();
        assert!(cut.starts_with("Could not decode"), "{cut}");
    }

    /// A file past the byte bound is refused unread, and a picture past the
    /// pixel bound from its header, before a pixel is decoded.
    #[cfg(feature = "preview")]
    #[test]
    fn a_picture_past_the_bounds_is_refused_before_it_is_decoded() {
        let mut huge = vec![0xff, 0xd8, 0xff];
        huge.resize(MAX_BYTES + 1, 0);
        let said = decode(&huge).unwrap_err();
        assert!(said.starts_with("Too large"), "{said}");

        // The gray JPEG's frame, saying 5000 by 5000: nothing after the
        // header could be decoded, and nothing has to be.
        let mut wide = gray_jpeg();
        let frame = wide
            .windows(2)
            .position(|pair| pair == [0xff, 0xc0])
            .unwrap();
        wide[frame + 5..frame + 9].copy_from_slice(&[0x13, 0x88, 0x13, 0x88]);
        let said = decode(&wide).unwrap_err();
        assert!(said.starts_with("Could not decode"), "{said}");
        assert!(said.contains("5000"), "{said}");
    }
}
