//! Pictures as the terminal's own pixels, where the terminal draws them.
//!
//! Half blocks are two pixels to a cell, and a face in sixteen of them is a
//! guess (`ui::preview`). Most terminals people run today draw real
//! pictures, each through one of three protocols, and a picture sent that
//! way is as sharp as the screen:
//!
//! | Protocol | Terminals taken to speak it |
//! |---|---|
//! | kitty's graphics protocol | kitty, Ghostty |
//! | iTerm2's inline images | iTerm2, WezTerm |
//! | sixel | Windows Terminal (from 1.22), foot |
//!
//! Everything else gets the half blocks, which need nothing but 24-bit
//! color.
//!
//! **Which one is decided from the environment and never by asking the
//! terminal** ([`protocol_of`]). A terminal says what it supports when it
//! is asked, and the answer arrives on its input, among the keys a view is
//! reading. So the list above is the terminals whose names in the
//! environment are known and whose support is not in doubt; inside tmux or
//! screen the name is the outer terminal's and the multiplexer decides what
//! passes, so there it is half blocks. **`SNOB_IMAGES`** says otherwise for
//! a terminal not on the list, or one on it that draws wrong: `kitty`,
//! `iterm`, `sixel` or `blocks`.
//!
//! **A picture is not a cell, and ratatui only knows cells.** A frame
//! places a picture over cells it leaves blank ([`place`]), and the bytes
//! go out after the frame's own, in the same synchronized update
//! (`tui::Screen`, through [`Painter`]). Two things keep the two in step:
//!
//! - The blank cells under a picture carry the picture's number as a
//!   foreground color ([`under`]), which a blank cell does not show. When
//!   the picture changes or goes, those cells differ from the frame before,
//!   so ratatui writes them again, and for sixel and iTerm2, where a
//!   picture lives in the cells it covers, that is what erases it.
//! - kitty keeps pictures in a layer of their own that text does not erase,
//!   so there each one is sent once, placed by its number, and every
//!   placement is deleted and made again when what is placed changes.
//!
//! A box over a view dims the cells under it (`tui::veil`), and calls
//! [`hide`]: a picture is not drawn under a box, and comes back with the
//! first frame after it.
//!
//! **What was not seen working.** The encoders are tested against what the
//! protocols say; none of this was looked at on a screen while writing it,
//! on any terminal. Windows Terminal is taken to draw sixel on the VT340's
//! cell of 10 by 20 pixels whatever its font, which is how its
//! implementation was described when 1.22 shipped; where a picture comes
//! out the wrong size or in the wrong place, `SNOB_IMAGES=blocks` is the
//! way back.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use base64::Engine;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::ui::preview::Pixels;

/// How a picture reaches the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Half blocks, in the cells themselves (`ui::preview`).
    Blocks,
    Sixel,
    Kitty,
    Iterm,
}

/// The protocol the environment names. `var` is `std::env::var` outside the
/// tests.
fn protocol_of(var: impl Fn(&str) -> Option<String>) -> Protocol {
    if let Some(asked) = var("SNOB_IMAGES") {
        return match asked.to_ascii_lowercase().as_str() {
            "kitty" => Protocol::Kitty,
            "iterm" => Protocol::Iterm,
            "sixel" => Protocol::Sixel,
            _ => Protocol::Blocks,
        };
    }
    if var("TMUX").is_some() || var("STY").is_some() {
        return Protocol::Blocks;
    }
    let program = var("TERM_PROGRAM").unwrap_or_default();
    let term = var("TERM").unwrap_or_default();
    let is = |name: &str| program.eq_ignore_ascii_case(name);
    if var("KITTY_WINDOW_ID").is_some() || term == "xterm-kitty" || is("ghostty") {
        Protocol::Kitty
    } else if is("iTerm.app") || is("WezTerm") {
        Protocol::Iterm
    } else if var("WT_SESSION").is_some() || term.starts_with("foot") {
        Protocol::Sixel
    } else {
        Protocol::Blocks
    }
}

/// The protocol of this run, read from the environment once. Half blocks in
/// a test, whatever terminal the tests are run from: a test draws into a
/// buffer and counts cells.
pub fn protocol() -> Protocol {
    if cfg!(test) {
        return Protocol::Blocks;
    }
    static HERE: OnceLock<Protocol> = OnceLock::new();
    *HERE.get_or_init(|| protocol_of(|name| std::env::var(name).ok()))
}

/// The VT340's cell, which is the one Windows Terminal draws sixel on
/// whatever its font, and the guess for any terminal that does not say.
const CELL: (usize, usize) = (10, 20);

/// The pixels of one cell, for sixel, which is drawn in pixels and has to
/// land on cells: what the terminal reports where it reports any (foot
/// does, through the size of its window), and [`CELL`] otherwise.
fn cell_pixels() -> (usize, usize) {
    static CELLS: OnceLock<(usize, usize)> = OnceLock::new();
    *CELLS.get_or_init(|| {
        if std::env::var_os("WT_SESSION").is_some() {
            return CELL;
        }
        match crossterm::terminal::window_size() {
            Ok(size) if size.width > 0 && size.height > 0 && size.columns > 0 && size.rows > 0 => (
                usize::from(size.width / size.columns).max(1),
                usize::from(size.height / size.rows).max(1),
            ),
            _ => CELL,
        }
    })
}

/// A picture a frame put over some of its cells.
#[derive(Debug, Clone)]
pub struct Placement {
    area: Rect,
    picture: Pixels,
}

impl Placement {
    fn key(&self) -> (Rect, u64) {
        (self.area, self.picture.id())
    }
}

/// What the frame being drawn has placed. A frame is drawn by one thread at
/// a time, the one that holds the terminal; the lock is for the type system.
static PLACED: Mutex<Vec<Placement>> = Mutex::new(Vec::new());

fn placed() -> std::sync::MutexGuard<'static, Vec<Placement>> {
    PLACED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Puts `picture` over the cells of `area`, which the frame leaves blank
/// ([`under`]): it is on screen when the frame is.
pub fn place(area: Rect, picture: &Pixels) {
    if area.width > 0 && area.height > 0 {
        placed().push(Placement {
            area,
            picture: picture.clone(),
        });
    }
}

/// Takes back every picture the frame has placed so far: a box is going up
/// over the view, and a picture is not drawn under one.
pub fn hide() {
    placed().clear();
}

/// What a blank cell under `picture` carries: the picture's number, as a
/// color a blank cell does not show.
pub fn under(picture: &Pixels) -> Color {
    let [red, green, blue, ..] = picture.id().to_le_bytes();
    Color::Rgb(red, green, blue)
}

/// The most encoded pictures kept: a view shows one or two at a time, and
/// an encoded one is some hundreds of kilobytes.
const ENCODED: usize = 6;

/// The most pictures kitty is left holding before all of them are dropped
/// and sent again as they are needed.
const SENT: usize = 32;

/// Sends the frame's pictures after the frame, and remembers what is on
/// screen.
pub struct Painter {
    protocol: Protocol,
    /// What the screen shows: where, and which picture.
    shown: Vec<(Rect, u64)>,
    /// Pictures encoded for the cells they were last drawn in.
    encoded: VecDeque<((u64, u16, u16), Vec<u8>)>,
    /// The numbers of the pictures kitty holds.
    sent: Vec<u32>,
}

impl Default for Painter {
    fn default() -> Self {
        Self::new()
    }
}

impl Painter {
    pub fn new() -> Self {
        Self::speaking(protocol())
    }

    fn speaking(protocol: Protocol) -> Self {
        Self {
            protocol,
            shown: Vec::new(),
            encoded: VecDeque::new(),
            sent: Vec::new(),
        }
    }

    /// After a frame: appends to `out` what puts the frame's pictures on
    /// screen. `wrote` is whether the frame wrote any cell.
    pub fn after_frame(&mut self, wrote: bool, out: &mut Vec<u8>) {
        if self.protocol == Protocol::Blocks {
            return;
        }
        let placed = std::mem::take(&mut *placed());
        self.paint(&placed, wrote, out);
    }

    fn paint(&mut self, placed: &[Placement], wrote: bool, out: &mut Vec<u8>) {
        let keys: Vec<(Rect, u64)> = placed.iter().map(Placement::key).collect();
        let same = keys == self.shown;
        match self.protocol {
            Protocol::Blocks => {}
            // A layer of its own: nothing a frame writes touches it, so
            // there is something to do only when what is placed changed.
            Protocol::Kitty if same => {}
            Protocol::Kitty => {
                out.extend_from_slice(b"\x1b7\x1b_Ga=d,d=a,q=2\x1b\\");
                if self.sent.len() > SENT {
                    out.extend_from_slice(KITTY_FORGET);
                    self.sent.clear();
                }
                for placement in placed {
                    let number = kitty_number(&placement.picture);
                    move_to(placement.area, out);
                    if !self.sent.contains(&number) {
                        kitty_send(&placement.picture, placement.area, number, out);
                        self.sent.push(number);
                        KITTY_HOLDS.store(true, Ordering::Relaxed);
                    }
                    out.extend_from_slice(
                        format!(
                            "\x1b_Ga=p,i={number},c={},r={},C=1,q=2\x1b\\",
                            placement.area.width, placement.area.height
                        )
                        .as_bytes(),
                    );
                }
                out.extend_from_slice(b"\x1b8");
            }
            // In the cells: a frame that wrote cells may have written
            // these, and one that wrote none left them as they were.
            Protocol::Sixel | Protocol::Iterm if same && !wrote => {}
            Protocol::Sixel | Protocol::Iterm => {
                if !placed.is_empty() {
                    out.extend_from_slice(b"\x1b7");
                    for placement in placed {
                        move_to(placement.area, out);
                        out.extend_from_slice(self.encoded(placement));
                    }
                    out.extend_from_slice(b"\x1b8");
                }
            }
        }
        self.shown = keys;
    }

    /// After something that is no frame, the screen cleared or left: what
    /// was on it is gone, and kitty is told to let go of what it holds.
    pub fn after_anything_else(&mut self, out: &mut Vec<u8>) {
        if self.protocol == Protocol::Kitty && !(self.shown.is_empty() && self.sent.is_empty()) {
            out.extend_from_slice(KITTY_FORGET);
            self.sent.clear();
            KITTY_HOLDS.store(false, Ordering::Relaxed);
        }
        self.shown.clear();
    }

    /// The picture of `placement` as this terminal takes it, encoded for
    /// its cells once and kept while it is being shown.
    fn encoded(&mut self, placement: &Placement) -> &[u8] {
        let Rect { width, height, .. } = placement.area;
        let key = (placement.picture.id(), width, height);
        if let Some(at) = self.encoded.iter().position(|(kept, _)| *kept == key) {
            return &self.encoded[at].1;
        }
        let bytes = match self.protocol {
            Protocol::Sixel => {
                let (across, down) = cell_pixels();
                sixel(
                    &placement
                        .picture
                        .sized(usize::from(width) * across, usize::from(height) * down),
                )
            }
            _ => iterm(
                &for_cells(&placement.picture, placement.area),
                placement.area,
            ),
        };
        if self.encoded.len() >= ENCODED {
            self.encoded.pop_front();
        }
        self.encoded.push_back((key, bytes));
        &self.encoded[self.encoded.len() - 1].1
    }
}

/// What tells kitty to delete every picture and free it.
pub const KITTY_FORGET: &[u8] = b"\x1b_Ga=d,d=A,q=2\x1b\\";

/// Whether kitty was handed a picture since it was last told to forget.
static KITTY_HOLDS: AtomicBool = AtomicBool::new(false);

/// What to write as the screen is left for good, where no frame follows to
/// carry it (`ui::restore_terminal`): kitty told to forget, if it was ever
/// handed anything. Nothing on any other terminal, and nothing twice.
pub fn on_the_way_out() -> Option<&'static [u8]> {
    KITTY_HOLDS
        .swap(false, Ordering::Relaxed)
        .then_some(KITTY_FORGET)
}

/// The cursor to the top left cell of `area`, as the terminal counts cells:
/// from one.
fn move_to(area: Rect, out: &mut Vec<u8>) {
    out.extend_from_slice(format!("\x1b[{};{}H", area.y + 1, area.x + 1).as_bytes());
}

/// `picture` at the size it is sent to a terminal that scales it into the
/// cells itself (kitty, iTerm2): as many pixels as the cells have on a
/// dense screen, twice [`CELL`], and no more than the picture has.
fn for_cells(picture: &Pixels, area: Rect) -> Pixels {
    let width = (usize::from(area.width) * CELL.0 * 2).min(picture.width());
    let height = (usize::from(area.height) * CELL.1 * 2).min(picture.height());
    picture.sized(width, height)
}

fn base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// kitty names a picture with a number that is not zero.
fn kitty_number(picture: &Pixels) -> u32 {
    ((picture.id() >> 32) as u32 ^ picture.id() as u32).max(1)
}

/// The picture's pixels, handed to kitty under `number` and not shown yet:
/// 24-bit RGB, in base64, 4096 characters to an escape as the protocol asks,
/// and with `q=2`, which keeps kitty from answering on the keyboard's input.
fn kitty_send(picture: &Pixels, area: Rect, number: u32, out: &mut Vec<u8>) {
    let picture = for_cells(picture, area);
    let data = base64(picture.rgb());
    let mut chunks = data.as_bytes().chunks(4096).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        if first {
            out.extend_from_slice(
                format!(
                    "\x1b_Ga=t,f=24,s={},v={},i={number},q=2,m={more};",
                    picture.width(),
                    picture.height()
                )
                .as_bytes(),
            );
            first = false;
        } else {
            out.extend_from_slice(format!("\x1b_Gm={more};").as_bytes());
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
}

/// The picture as iTerm2's inline image, stretched over the cells of
/// `area`, which already have its shape. The file inside is a BMP: the one
/// format every reader of this protocol decodes that takes no compressor to
/// write.
fn iterm(picture: &Pixels, area: Rect) -> Vec<u8> {
    let file = bmp(picture);
    let mut out = format!(
        "\x1b]1337;File=inline=1;size={};width={};height={};preserveAspectRatio=0:",
        file.len(),
        area.width,
        area.height
    )
    .into_bytes();
    out.extend_from_slice(base64(&file).as_bytes());
    out.push(0x07);
    out
}

/// A 24-bit BMP of the picture: the two headers, then the rows from the
/// bottom up, blue first, each padded to four bytes.
fn bmp(picture: &Pixels) -> Vec<u8> {
    let (width, height) = (picture.width(), picture.height());
    let row = (width * 3).div_ceil(4) * 4;
    let size = 54 + row * height;
    let mut file = Vec::with_capacity(size);
    file.extend_from_slice(b"BM");
    file.extend_from_slice(&(size as u32).to_le_bytes());
    file.extend_from_slice(&[0; 4]);
    file.extend_from_slice(&54u32.to_le_bytes());
    file.extend_from_slice(&40u32.to_le_bytes());
    file.extend_from_slice(&(width as i32).to_le_bytes());
    file.extend_from_slice(&(height as i32).to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&24u16.to_le_bytes());
    file.extend_from_slice(&[0; 4]);
    file.extend_from_slice(&((row * height) as u32).to_le_bytes());
    // 72 dots an inch, in dots a meter, and no palette.
    file.extend_from_slice(&2835u32.to_le_bytes());
    file.extend_from_slice(&2835u32.to_le_bytes());
    file.extend_from_slice(&[0; 8]);
    for y in (0..height).rev() {
        let line = &picture.rgb()[y * width * 3..(y + 1) * width * 3];
        for pixel in line.as_chunks::<3>().0 {
            file.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
        file.resize(file.len() + row - width * 3, 0);
    }
    file
}

/// The most colors a sixel picture is drawn in: the registers a VT340
/// successor has, less one left alone.
const COLORS: usize = 255;

/// The picture in at most [`COLORS`] colors: the colors, and for each pixel
/// which one.
///
/// The colors are chosen from the picture (median cut over its colors at
/// five bits a channel), and each pixel's error is passed to its neighbors
/// (Floyd and Steinberg's weights), which is what makes a face in 255
/// colors look like a photograph and not like a map.
fn quantized(picture: &Pixels) -> (Vec<[u8; 3]>, Vec<u8>) {
    let rgb = picture.rgb();
    let bin = |red: u8, green: u8, blue: u8| {
        (usize::from(red >> 3) << 10) | (usize::from(green >> 3) << 5) | usize::from(blue >> 3)
    };
    let mut counts = vec![0u32; 1 << 15];
    // What the pixels of each bin add up to, so that a color the picture
    // has few of comes back as it was and not as the middle of its bin.
    let mut sums = vec![[0u64; 3]; 1 << 15];
    for pixel in rgb.as_chunks::<3>().0 {
        let at = bin(pixel[0], pixel[1], pixel[2]);
        counts[at] += 1;
        for channel in 0..3 {
            sums[at][channel] += u64::from(pixel[channel]);
        }
    }
    let channels = |at: usize| [(at >> 10) & 31, (at >> 5) & 31, at & 31];
    let mut boxes: Vec<Vec<usize>> = vec![(0..1 << 15).filter(|at| counts[*at] > 0).collect()];
    // Split the box with the widest spread of a channel, at the middle of
    // its pixels, until there is a box a color or nothing left to split.
    while boxes.len() < COLORS {
        let spread = |colors: &Vec<usize>| {
            (0..3)
                .map(|channel| {
                    let values = colors.iter().map(|at| channels(*at)[channel]);
                    let (low, high) = values.fold((31, 0), |(low, high), value| {
                        (low.min(value), high.max(value))
                    });
                    (high.saturating_sub(low), channel)
                })
                .max()
                .unwrap_or((0, 0))
        };
        let Some((at, (_, channel))) = boxes
            .iter()
            .enumerate()
            .filter(|(_, colors)| colors.len() > 1)
            .map(|(at, colors)| (at, spread(colors)))
            .max_by_key(|(_, (wide, _))| *wide)
        else {
            break;
        };
        let mut colors = std::mem::take(&mut boxes[at]);
        colors.sort_unstable_by_key(|color| channels(*color)[channel]);
        let half = colors
            .iter()
            .map(|color| u64::from(counts[*color]))
            .sum::<u64>()
            / 2;
        let mut seen = 0u64;
        let cut = colors
            .iter()
            .position(|color| {
                seen += u64::from(counts[*color]);
                seen > half
            })
            .unwrap_or(0)
            .clamp(1, colors.len() - 1);
        let upper = colors.split_off(cut);
        boxes[at] = colors;
        boxes.push(upper);
    }
    let palette: Vec<[u8; 3]> = boxes
        .iter()
        .map(|colors| {
            let mut sum = [0u64; 3];
            let mut pixels = 0u64;
            for color in colors {
                for channel in 0..3 {
                    sum[channel] += sums[*color][channel];
                }
                pixels += u64::from(counts[*color]);
            }
            sum.map(|channel| (channel / pixels.max(1)) as u8)
        })
        .collect();

    // Which color of the palette each bin is nearest to, found the first
    // time a pixel falls in the bin.
    let mut nearest = vec![u8::MAX; 1 << 15];
    let (width, height) = (picture.width(), picture.height());
    let mut pixels = Vec::with_capacity(width * height);
    // The error owed to this row and to the next, three channels a pixel,
    // with a pixel of margin each side.
    let mut owed = vec![[0i32; 3]; width + 2];
    let mut owed_below = vec![[0i32; 3]; width + 2];
    for y in 0..height {
        for x in 0..width {
            let at = (y * width + x) * 3;
            let mut wanted = [0i32; 3];
            for channel in 0..3 {
                wanted[channel] =
                    (i32::from(rgb[at + channel]) + owed[x + 1][channel] / 16).clamp(0, 255);
            }
            let slot = bin(wanted[0] as u8, wanted[1] as u8, wanted[2] as u8);
            if nearest[slot] == u8::MAX {
                nearest[slot] = palette
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, color)| {
                        (0..3)
                            .map(|channel| (i32::from(color[channel]) - wanted[channel]).pow(2))
                            .sum::<i32>()
                    })
                    .map_or(0, |(index, _)| index as u8);
            }
            let chosen = nearest[slot];
            pixels.push(chosen);
            for channel in 0..3 {
                let error = wanted[channel] - i32::from(palette[usize::from(chosen)][channel]);
                owed[x + 2][channel] += error * 7;
                owed_below[x][channel] += error * 3;
                owed_below[x + 1][channel] += error * 5;
                owed_below[x + 2][channel] += error;
            }
        }
        std::mem::swap(&mut owed, &mut owed_below);
        owed_below.fill([0; 3]);
    }
    (palette, pixels)
}

/// The picture as a sixel string, a pixel of it a pixel of the screen.
///
/// `P1` of 9 and the raster attributes both say the pixels are square; `P2`
/// of 1 says a pixel no color names is left as it was, so nothing around
/// the picture is filled. Six rows go to a band: for each color in a band,
/// its register and then a character a column whose six low bits are the
/// rows that color is on, with runs counted.
fn sixel(picture: &Pixels) -> Vec<u8> {
    let (palette, pixels) = quantized(picture);
    let (width, height) = (picture.width(), picture.height());
    let mut out = format!("\x1bP9;1;0q\"1;1;{width};{height}").into_bytes();
    for (register, [red, green, blue]) in palette.iter().enumerate() {
        let percent = |value: u8| (u32::from(value) * 100 + 127) / 255;
        out.extend_from_slice(
            format!(
                "#{register};2;{};{};{}",
                percent(*red),
                percent(*green),
                percent(*blue)
            )
            .as_bytes(),
        );
    }
    let mut columns = vec![0u8; palette.len() * width];
    let mut used = vec![false; palette.len()];
    for band in 0..height.div_ceil(6) {
        columns.fill(0);
        used.fill(false);
        for row in 0..6 {
            let y = band * 6 + row;
            if y >= height {
                break;
            }
            for x in 0..width {
                let color = usize::from(pixels[y * width + x]);
                columns[color * width + x] |= 1 << row;
                used[color] = true;
            }
        }
        let mut first = true;
        for (color, _) in used.iter().enumerate().filter(|(_, used)| **used) {
            if !first {
                // Back to the left of the band, for the next color.
                out.push(b'$');
            }
            first = false;
            out.extend_from_slice(format!("#{color}").as_bytes());
            let line = &columns[color * width..(color + 1) * width];
            let mut x = 0;
            while x < width {
                let run = line[x..]
                    .iter()
                    .take_while(|bits| **bits == line[x])
                    .count();
                let character = 63 + line[x];
                if run > 3 {
                    out.extend_from_slice(format!("!{run}").as_bytes());
                    out.push(character);
                } else {
                    out.extend(std::iter::repeat_n(character, run));
                }
                x += run;
            }
        }
        // Down to the next band.
        out.push(b'-');
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(set: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            set.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    /// Each terminal known to draw pictures gets its own protocol, by the
    /// names it leaves in the environment; the rest, and anything inside a
    /// multiplexer, get half blocks; and `SNOB_IMAGES` overrides all of it.
    #[test]
    fn the_protocol_is_the_one_the_terminal_is_known_to_speak() {
        let of = |set: &[(&str, &str)]| protocol_of(env(set));
        assert_eq!(of(&[]), Protocol::Blocks);
        assert_eq!(of(&[("TERM", "xterm-256color")]), Protocol::Blocks);
        assert_eq!(of(&[("TERM_PROGRAM", "Apple_Terminal")]), Protocol::Blocks);
        assert_eq!(of(&[("TERM_PROGRAM", "vscode")]), Protocol::Blocks);
        assert_eq!(of(&[("KITTY_WINDOW_ID", "1")]), Protocol::Kitty);
        assert_eq!(of(&[("TERM", "xterm-kitty")]), Protocol::Kitty);
        assert_eq!(of(&[("TERM_PROGRAM", "ghostty")]), Protocol::Kitty);
        assert_eq!(of(&[("TERM_PROGRAM", "iTerm.app")]), Protocol::Iterm);
        assert_eq!(of(&[("TERM_PROGRAM", "WezTerm")]), Protocol::Iterm);
        assert_eq!(of(&[("WT_SESSION", "0a1b")]), Protocol::Sixel);
        assert_eq!(of(&[("TERM", "foot")]), Protocol::Sixel);
        assert_eq!(
            of(&[("WT_SESSION", "0a1b"), ("TMUX", "/tmp/tmux-1/default")]),
            Protocol::Blocks
        );
        assert_eq!(
            of(&[("KITTY_WINDOW_ID", "1"), ("STY", "1.pts-0.host")]),
            Protocol::Blocks
        );
        assert_eq!(
            of(&[("WT_SESSION", "0a1b"), ("SNOB_IMAGES", "blocks")]),
            Protocol::Blocks
        );
        assert_eq!(of(&[("SNOB_IMAGES", "KITTY")]), Protocol::Kitty);
        assert_eq!(of(&[("SNOB_IMAGES", "iterm")]), Protocol::Iterm);
        assert_eq!(
            of(&[("SNOB_IMAGES", "sixel"), ("TMUX", "x")]),
            Protocol::Sixel
        );
        assert_eq!(of(&[("SNOB_IMAGES", "nonsense")]), Protocol::Blocks);
    }

    fn placement(x: u16, picture: &Pixels) -> Placement {
        Placement {
            area: Rect::new(x, 2, 4, 2),
            picture: picture.clone(),
        }
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// A picture that lives in the cells is sent when it is placed, sent
    /// again after a frame that wrote cells, and not after one that wrote
    /// none; one that is gone is not sent, the cells having been written
    /// over.
    #[test]
    fn a_picture_in_the_cells_follows_the_frames_that_write_cells() {
        let red = Pixels::plain(8, 8, (255, 0, 0));
        let mut painter = Painter::speaking(Protocol::Sixel);
        let mut out = Vec::new();
        painter.paint(&[placement(3, &red)], true, &mut out);
        let sent = text(&out);
        assert!(sent.starts_with("\x1b7\x1b[3;4H\x1bP9;1;0q"), "{sent:?}");
        assert!(sent.ends_with("\x1b\\\x1b8"), "{sent:?}");

        out.clear();
        painter.paint(&[placement(3, &red)], false, &mut out);
        assert!(out.is_empty(), "nothing was written over it");
        painter.paint(&[placement(3, &red)], true, &mut out);
        assert_eq!(text(&out), sent, "the same bytes, kept and sent again");

        out.clear();
        painter.paint(&[], true, &mut out);
        assert!(out.is_empty(), "{:?}", text(&out));
        // Placed again after it was gone, with no cell written: it is new
        // to the screen, and is sent.
        painter.paint(&[placement(3, &red)], false, &mut out);
        assert_eq!(text(&out), sent);
    }

    /// kitty is handed a picture once, places it by its number, and has its
    /// placements deleted and made again only when what is placed changes.
    #[test]
    fn kitty_is_sent_a_picture_once_and_places_it_by_number() {
        let red = Pixels::plain(8, 8, (255, 0, 0));
        let blue = Pixels::plain(8, 8, (0, 0, 255));
        let mut painter = Painter::speaking(Protocol::Kitty);
        let mut out = Vec::new();
        painter.paint(&[placement(0, &red)], true, &mut out);
        let first = text(&out);
        let number = kitty_number(&red);
        assert!(first.contains("\x1b_Ga=d,d=a,q=2\x1b\\"), "{first:?}");
        assert!(
            first.contains(&format!("a=t,f=24,s=8,v=8,i={number},q=2,m=0;")),
            "{first:?}"
        );
        assert!(
            first.contains(&format!("a=p,i={number},c=4,r=2,C=1,q=2")),
            "{first:?}"
        );

        out.clear();
        painter.paint(&[placement(0, &red)], true, &mut out);
        assert!(out.is_empty(), "text does not erase it: {:?}", text(&out));

        painter.paint(&[placement(0, &red), placement(9, &blue)], false, &mut out);
        let second = text(&out);
        assert_eq!(second.matches("a=t,").count(), 1, "only the new one");
        assert_eq!(second.matches("a=p,").count(), 2, "{second:?}");

        out.clear();
        painter.paint(&[], false, &mut out);
        assert!(text(&out).contains("a=d,d=a"), "{:?}", text(&out));
        out.clear();
        painter.after_anything_else(&mut out);
        assert_eq!(out, KITTY_FORGET);
    }

    /// Pixels go to kitty in pieces of 4096 characters of base64, all but
    /// the last saying that more follows.
    #[test]
    fn a_large_picture_goes_to_kitty_in_pieces() {
        let picture = Pixels::plain(64, 64, (1, 2, 3));
        let mut out = Vec::new();
        kitty_send(&picture, Rect::new(0, 0, 40, 20), 7, &mut out);
        let sent = text(&out);
        // 64 * 64 * 3 bytes are 16,384 characters of base64: four pieces.
        assert_eq!(sent.matches("\x1b_G").count(), 4, "{}", sent.len());
        assert_eq!(sent.matches("m=1;").count(), 3);
        assert!(sent.contains("\x1b_Gm=0;"));
        for piece in sent.split("\x1b\\").filter(|piece| !piece.is_empty()) {
            let data = piece.split_once(';').unwrap().1;
            assert!(data.len() <= 4096, "{}", data.len());
        }
    }

    /// The inline image names the cells it covers and carries a BMP whose
    /// headers say the picture's size, its rows from the bottom up and blue
    /// first.
    #[test]
    fn an_inline_image_carries_a_bmp_of_the_picture() {
        let picture = Pixels::plain(3, 2, (10, 20, 30));
        let sent = text(&iterm(&picture, Rect::new(0, 0, 6, 3)));
        assert!(sent.starts_with("\x1b]1337;File=inline=1;size=78;width=6;height=3;"));
        assert!(sent.ends_with('\x07'));

        let file = bmp(&picture);
        // Two rows of three pixels, each row padded from nine bytes to twelve.
        assert_eq!(file.len(), 54 + 2 * 12);
        assert_eq!(&file[..2], b"BM");
        assert_eq!(file[18..22], 3i32.to_le_bytes());
        assert_eq!(file[22..26], 2i32.to_le_bytes());
        assert_eq!(file[28..30], 24u16.to_le_bytes());
        assert_eq!(&file[54..57], [30, 20, 10], "blue first");
        assert_eq!(&file[63..66], [0, 0, 0], "the padding");
    }

    /// A picture of few colors keeps them exactly, and every pixel is one
    /// of the palette's.
    #[test]
    fn a_picture_of_few_colors_keeps_its_colors() {
        let mut rgb = Vec::new();
        for y in 0..12 {
            for x in 0..12 {
                rgb.extend(if (x + y) % 2 == 0 {
                    [255u8, 0, 0]
                } else {
                    [0u8, 0, 255]
                });
            }
        }
        let picture = Pixels::plain(12, 12, (0, 0, 0)).with(rgb);
        let (palette, pixels) = quantized(&picture);
        assert_eq!(palette.len(), 2);
        assert_eq!(pixels.len(), 144);
        let color = |at: usize| palette[usize::from(pixels[at])];
        // A bin is eight values wide: the color comes back within it.
        assert!(color(0)[0] >= 248 && color(0)[2] <= 7, "{:?}", color(0));
        assert!(color(1)[2] >= 248 && color(1)[0] <= 7, "{:?}", color(1));
        assert_ne!(pixels[0], pixels[1]);
        assert_eq!(pixels[0], pixels[2]);
    }

    /// A photograph is drawn in no more colors than a sixel terminal has.
    #[test]
    fn a_picture_of_many_colors_is_drawn_in_the_registers_there_are() {
        let mut rgb = Vec::new();
        for y in 0..64u32 {
            for x in 0..64u32 {
                rgb.extend([(x * 4) as u8, (y * 4) as u8, ((x + y) * 2) as u8]);
            }
        }
        let picture = Pixels::plain(64, 64, (0, 0, 0)).with(rgb);
        let (palette, pixels) = quantized(&picture);
        assert_eq!(palette.len(), COLORS);
        assert!(pixels.iter().all(|pixel| usize::from(*pixel) < COLORS));
    }

    /// The sixel of a small picture, read back: its size, its colors in
    /// percent, a band of six rows as a run of full columns and a band of
    /// what is left.
    #[test]
    fn a_sixel_says_the_size_the_colors_and_the_rows() {
        let picture = Pixels::plain(5, 8, (255, 0, 0));
        let sent = text(&sixel(&picture));
        assert!(sent.starts_with("\x1bP9;1;0q\"1;1;5;8"), "{sent:?}");
        assert!(sent.ends_with("-\x1b\\"), "{sent:?}");
        // One color, a red within a bin of full: 98 to 100 percent.
        let color = sent.split('#').nth(1).unwrap();
        assert!(
            color.starts_with("0;2;") && color.ends_with(";0;0"),
            "{color:?}"
        );
        // Six rows on in five columns: `~` is 63 + 0b111111, counted.
        assert!(sent.contains("#0!5~-"), "{sent:?}");
        // The two rows left: 63 + 0b000011 is `B`.
        assert!(sent.contains("#0!5B-"), "{sent:?}");
    }
}
