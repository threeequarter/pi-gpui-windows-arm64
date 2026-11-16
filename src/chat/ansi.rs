//! Terminal text from extension widgets: SGR styling becomes GPUI highlight
//! runs, every other escape sequence is dropped.

use std::ops::Range;

use gpui::{
    FontStyle, FontWeight, HighlightStyle, Hsla, Rgba, SharedString, StrikethroughStyle,
    UnderlineStyle, px,
};

const ESCAPE: char = '\x1b';
const BELL: char = '\x07';
const DIM_FADE: f32 = 0.4;
/// xterm's default palette for the 16 basic colors.
const BASIC_COLORS: [u32; 16] = [
    0x000000, 0xcd0000, 0x00cd00, 0xcdcd00, 0x0000ee, 0xcd00cd, 0x00cdcd, 0xe5e5e5, 0x7f7f7f,
    0xff0000, 0x00ff00, 0xffff00, 0x5c5cff, 0xff00ff, 0x00ffff, 0xffffff,
];
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

pub(super) struct StyledLine {
    pub text: SharedString,
    pub highlights: Vec<(Range<usize>, HighlightStyle)>,
}

#[derive(Clone, Copy, Default, PartialEq)]
struct Pen {
    foreground: Option<Hsla>,
    background: Option<Hsla>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
}

impl Pen {
    fn highlight(self) -> Option<HighlightStyle> {
        if self == Pen::default() {
            return None;
        }
        Some(HighlightStyle {
            color: self.foreground,
            background_color: self.background,
            font_weight: self.bold.then_some(FontWeight::BOLD),
            font_style: self.italic.then_some(FontStyle::Italic),
            underline: self.underline.then_some(UnderlineStyle {
                thickness: px(1.),
                ..UnderlineStyle::default()
            }),
            strikethrough: self.strikethrough.then_some(StrikethroughStyle {
                thickness: px(1.),
                ..StrikethroughStyle::default()
            }),
            fade_out: self.dim.then_some(DIM_FADE),
        })
    }

    fn apply(&mut self, parameters: &[u16]) {
        let mut codes = parameters.iter().copied();
        while let Some(code) = codes.next() {
            match code {
                0 => *self = Pen::default(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                9 => self.strikethrough = true,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                29 => self.strikethrough = false,
                30..=37 => self.foreground = palette(code - 30),
                38 => self.foreground = extended_color(&mut codes),
                39 => self.foreground = None,
                40..=47 => self.background = palette(code - 40),
                48 => self.background = extended_color(&mut codes),
                49 => self.background = None,
                90..=97 => self.foreground = palette(code - 90 + 8),
                100..=107 => self.background = palette(code - 100 + 8),
                _ => {}
            }
        }
    }
}

/// One terminal line as text plus highlight runs; trailing blanks are cut.
pub(super) fn parse(line: &str) -> StyledLine {
    let mut text = String::new();
    let mut highlights = Vec::new();
    let mut pen = Pen::default();
    let mut run_start = 0;
    let mut rest = line;
    while let Some(index) = rest.find(ESCAPE) {
        text.push_str(&rest[..index]);
        let (length, parameters) = escape_sequence(&rest[index..]);
        rest = &rest[index + length..];
        let Some(parameters) = parameters else {
            continue;
        };
        if let Some(style) = pen.highlight()
            && text.len() > run_start
        {
            highlights.push((run_start..text.len(), style));
        }
        run_start = text.len();
        pen.apply(&parameters);
    }
    text.push_str(rest);
    if let Some(style) = pen.highlight()
        && text.len() > run_start
    {
        highlights.push((run_start..text.len(), style));
    }

    let end = text.trim_end().len();
    text.truncate(end);
    let highlights = highlights
        .into_iter()
        .filter(|(range, _)| range.start < end)
        .map(|(range, style)| (range.start..range.end.min(end), style))
        .collect();
    StyledLine {
        text: text.into(),
        highlights,
    }
}

/// Byte length of the escape sequence at the start of `sequence`, and its
/// parameters when it is SGR (`ESC [ … m`).
fn escape_sequence(sequence: &str) -> (usize, Option<Vec<u16>>) {
    let body = &sequence[ESCAPE.len_utf8()..];
    match body.chars().next() {
        Some('[') => {
            let Some(final_index) = body[1..].find(|c: char| ('@'..='~').contains(&c)) else {
                return (sequence.len(), None);
            };
            let parameters = &body[1..1 + final_index];
            let length = ESCAPE.len_utf8() + 1 + final_index + 1;
            if !body[1 + final_index..].starts_with('m') {
                return (length, None);
            }
            let codes = parameters
                .split([';', ':'])
                .map(|code| code.parse().unwrap_or(0))
                .collect();
            (length, Some(codes))
        }
        Some(']') => {
            let terminator = body
                .char_indices()
                .find_map(|(index, c)| match c {
                    BELL => Some(index + 1),
                    ESCAPE if body[index + 1..].starts_with('\\') => Some(index + 2),
                    _ => None,
                })
                .unwrap_or(body.len());
            (ESCAPE.len_utf8() + terminator, None)
        }
        Some(c) => (ESCAPE.len_utf8() + c.len_utf8(), None),
        None => (sequence.len(), None),
    }
}

/// The color after 38 or 48: `5;n` (256-color palette) or `2;r;g;b`.
fn extended_color(codes: &mut impl Iterator<Item = u16>) -> Option<Hsla> {
    match codes.next()? {
        5 => palette(codes.next()?),
        2 => {
            let mut channel = || codes.next().map(|value| value.min(255) as u8);
            Some(rgb(channel()?, channel()?, channel()?))
        }
        _ => None,
    }
}

fn palette(index: u16) -> Option<Hsla> {
    match index {
        0..=15 => {
            let [_, r, g, b] = BASIC_COLORS[index as usize].to_be_bytes();
            Some(rgb(r, g, b))
        }
        16..=231 => {
            let cube = index - 16;
            let level = |value: u16| CUBE_LEVELS[value as usize];
            Some(rgb(level(cube / 36), level(cube / 6 % 6), level(cube % 6)))
        }
        232..=255 => {
            let gray = 8 + 10 * (index - 232) as u8;
            Some(rgb(gray, gray, gray))
        }
        _ => None,
    }
}

fn rgb(r: u8, g: u8, b: u8) -> Hsla {
    Rgba {
        r: f32::from(r) / 255.,
        g: f32::from(g) / 255.,
        b: f32::from(b) / 255.,
        a: 1.,
    }
    .into()
}
