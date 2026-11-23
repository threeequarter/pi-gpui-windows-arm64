use gpui::{App, Global, Hsla, SharedString, rgba};

/// Semantic color tokens, named after Zed's theme schema so components read
/// the same way as Zed's `ui` crate.
#[derive(Clone)]
pub struct ThemeColors {
    pub title_bar_background: Hsla,
    pub title_bar_inactive_background: Hsla,
    pub panel_background: Hsla,
    pub editor_background: Hsla,
    pub elevated_surface_background: Hsla,
    pub element_background: Hsla,
    pub element_hover: Hsla,
    pub element_active: Hsla,
    pub element_selected: Hsla,
    pub ghost_element_hover: Hsla,
    pub ghost_element_active: Hsla,
    pub ghost_element_selected: Hsla,
    pub border: Hsla,
    pub border_variant: Hsla,
    pub border_focused: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_placeholder: Hsla,
    pub text_disabled: Hsla,
    pub text_accent: Hsla,
    pub icon: Hsla,
    pub icon_muted: Hsla,
    pub icon_disabled: Hsla,
    pub icon_accent: Hsla,
    pub error: Hsla,
    pub error_background: Hsla,
    pub warning: Hsla,
    pub warning_background: Hsla,
    pub success: Hsla,
    pub success_background: Hsla,
    pub info: Hsla,
    pub info_background: Hsla,
    /// Behind fenced code in answers.
    pub code_block_background: Hsla,
    pub syntax: SyntaxColors,
}

/// Code colors, one per scope group (see `highlight.rs`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyntaxColors {
    pub comment: Hsla,
    pub keyword: Hsla,
    pub string: Hsla,
    pub number: Hsla,
    pub function: Hsla,
    pub r#type: Hsla,
    pub constant: Hsla,
    pub operator: Hsla,
    /// `None` keeps the text color.
    pub punctuation: Option<Hsla>,
    pub property: Hsla,
    pub tag: Hsla,
    pub attribute: Hsla,
}

impl SyntaxColors {
    #[allow(clippy::too_many_arguments)]
    const fn palette(
        comment: u32,
        keyword: u32,
        string: u32,
        number: u32,
        function: u32,
        r#type: u32,
        constant: u32,
        operator: u32,
        punctuation: Option<u32>,
        property: u32,
        tag: u32,
        attribute: u32,
    ) -> [Option<u32>; 12] {
        [
            Some(comment),
            Some(keyword),
            Some(string),
            Some(number),
            Some(function),
            Some(r#type),
            Some(constant),
            Some(operator),
            punctuation,
            Some(property),
            Some(tag),
            Some(attribute),
        ]
    }

    fn from_palette(palette: [Option<u32>; 12]) -> Self {
        let color = |index: usize| hex((palette[index].unwrap_or(0) << 8) | 0xff);
        Self {
            comment: color(0),
            keyword: color(1),
            string: color(2),
            number: color(3),
            function: color(4),
            r#type: color(5),
            constant: color(6),
            operator: color(7),
            punctuation: palette[8].map(|value| hex((value << 8) | 0xff)),
            property: color(9),
            tag: color(10),
            attribute: color(11),
        }
    }

    /// GitHub Dark Default.
    pub fn github_dark() -> Self {
        Self::from_palette(Self::palette(
            0x8b949e, 0xff7b72, 0xa5d6ff, 0x79c0ff, 0xd2a8ff, 0xffa657, 0x79c0ff, 0xff7b72, None,
            0x79c0ff, 0x7ee787, 0x79c0ff,
        ))
    }

    /// GitHub Light Default.
    pub fn github_light() -> Self {
        Self::from_palette(Self::palette(
            0x6e7781, 0xcf222e, 0x0a3069, 0x0550ae, 0x8250df, 0x953800, 0x0550ae, 0xcf222e, None,
            0x0550ae, 0x116329, 0x0550ae,
        ))
    }
}

pub struct Theme {
    pub name: SharedString,
    pub colors: ThemeColors,
}

impl Global for Theme {}

impl Theme {
    /// Dark themes have a dark editor background.
    pub fn is_dark(&self) -> bool {
        self.colors.editor_background.l < 0.5
    }
}

impl Theme {
    /// Zed's bundled Ayu Dark and Ayu Light (assets/themes/ayu/ayu.json).
    pub fn ayu(dark: bool) -> Self {
        let name = if dark { "Ayu Dark" } else { "Ayu Light" };
        let color = |dark_value, light_value| hex(if dark { dark_value } else { light_value });
        let editor_background = color(0x0d1016ff, 0xfcfcfcff);
        Self {
            name: SharedString::new_static(name),
            colors: ThemeColors {
                title_bar_background: color(0x313337ff, 0xdcdddeff),
                title_bar_inactive_background: color(0x1f2127ff, 0xececedff),
                panel_background: color(0x1f2127ff, 0xececedff),
                editor_background,
                elevated_surface_background: color(0x1f2127ff, 0xececedff),
                element_background: color(0x1f2127ff, 0xececedff),
                element_hover: color(0x2d2f34ff, 0xdfe0e1ff),
                element_active: color(0x3e4043ff, 0xcfd0d2ff),
                element_selected: color(0x3e4043ff, 0xcfd0d2ff),
                ghost_element_hover: color(0x2d2f34ff, 0xdfe0e1ff),
                ghost_element_active: color(0x3e4043ff, 0xcfd0d2ff),
                ghost_element_selected: color(0x3e4043ff, 0xcfd0d2ff),
                border: color(0x3f4043ff, 0xcfd1d2ff),
                border_variant: color(0x2d2f34ff, 0xdfe0e1ff),
                border_focused: color(0x1b4a6eff, 0xc4daf6ff),
                text: color(0xbfbdb6ff, 0x5c6166ff),
                text_muted: color(0x8a8986ff, 0x8b8e92ff),
                text_placeholder: color(0x696a6aff, 0xa9acaeff),
                text_disabled: color(0x696a6aff, 0xa9acaeff),
                text_accent: color(0x5ac1feff, 0x3b9ee5ff),
                icon: color(0xbfbdb6ff, 0x5c6166ff),
                icon_muted: color(0x8a8986ff, 0x8b8e92ff),
                icon_disabled: color(0x696a6aff, 0xa9acaeff),
                icon_accent: color(0x5ac1feff, 0x3b9ee5ff),
                error: color(0xef7177ff, 0xef7271ff),
                error_background: color(0x48161bff, 0xffe3e1ff),
                warning: color(0xfeb454ff, 0xf1ad49ff),
                warning_background: color(0x572815ff, 0xffeedaff),
                success: color(0xaad84cff, 0x85b304ff),
                success_background: color(0x294113ff, 0xe9efd2ff),
                info: color(0x5ac1feff, 0x3b9ee5ff),
                info_background: color(0x0d2f4eff, 0xdeebfaff),
                code_block_background: editor_background,
                syntax: SyntaxColors {
                    comment: color(0x5c6773ff, 0xabb0b6ff),
                    keyword: color(0xff8f3fff, 0xfa8d3eff),
                    string: color(0xa9d94bff, 0x86b300ff),
                    number: color(0xd2a6ffff, 0xa37accff),
                    function: color(0xffb353ff, 0xf2ad48ff),
                    r#type: color(0x59c2ffff, 0x389ee6ff),
                    constant: color(0xd2a6ffff, 0xa37accff),
                    operator: color(0xf29668ff, 0xed9365ff),
                    punctuation: Some(color(0xa6a5a0ff, 0x73777bff)),
                    property: color(0x5ac1feff, 0x3b9ee5ff),
                    tag: color(0x5ac1feff, 0x3b9ee5ff),
                    attribute: color(0x5ac1feff, 0x3b9ee5ff),
                },
            },
        }
    }

    /// Values from Zed's bundled "One Dark" theme (assets/themes/one/one.json,
    /// MIT, Copyright (c) 2014 GitHub Inc.).
    pub fn one_dark() -> Self {
        Self {
            name: SharedString::new_static("One Dark"),
            colors: ThemeColors {
                title_bar_background: hex(0x3b414dff),
                title_bar_inactive_background: hex(0x2e343eff),
                panel_background: hex(0x2f343eff),
                editor_background: hex(0x282c33ff),
                elevated_surface_background: hex(0x2f343eff),
                element_background: hex(0x2e343eff),
                element_hover: hex(0x363c46ff),
                element_active: hex(0x454a56ff),
                element_selected: hex(0x454a56ff),
                ghost_element_hover: hex(0x363c46ff),
                ghost_element_active: hex(0x454a56ff),
                ghost_element_selected: hex(0x454a56ff),
                border: hex(0x464b57ff),
                border_variant: hex(0x363c46ff),
                border_focused: hex(0x47679eff),
                text: hex(0xdce0e5ff),
                text_muted: hex(0xa9afbcff),
                text_placeholder: hex(0x878a98ff),
                text_disabled: hex(0x878a98ff),
                text_accent: hex(0x74ade8ff),
                icon: hex(0xdce0e5ff),
                icon_muted: hex(0xa9afbcff),
                icon_disabled: hex(0x878a98ff),
                icon_accent: hex(0x74ade8ff),
                error: hex(0xd07277ff),
                error_background: hex(0xd0727726),
                warning: hex(0xdec184ff),
                warning_background: hex(0xdec18426),
                success: hex(0xa1c181ff),
                success_background: hex(0xa1c18126),
                info: hex(0x74ade8ff),
                info_background: hex(0x74ade826),
                code_block_background: hex(0x282c33ff),
                syntax: SyntaxColors::github_dark(),
            },
        }
    }
}

pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}

/// Semantic text and icon colors, as in Zed's `ui::Color`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    Muted,
    Placeholder,
    Disabled,
    Accent,
    Error,
    Warning,
    Success,
}

impl Color {
    pub fn text(self, cx: &App) -> Hsla {
        let colors = &cx.theme().colors;
        match self {
            Color::Default => colors.text,
            Color::Muted => colors.text_muted,
            Color::Placeholder => colors.text_placeholder,
            Color::Disabled => colors.text_disabled,
            Color::Accent => colors.text_accent,
            Color::Error => colors.error,
            Color::Warning => colors.warning,
            Color::Success => colors.success,
        }
    }

    pub fn icon(self, cx: &App) -> Hsla {
        let colors = &cx.theme().colors;
        match self {
            Color::Default => colors.icon,
            Color::Muted => colors.icon_muted,
            Color::Placeholder | Color::Disabled => colors.icon_disabled,
            Color::Accent => colors.icon_accent,
            _ => self.text(cx),
        }
    }
}

fn hex(value: u32) -> Hsla {
    rgba(value).into()
}
