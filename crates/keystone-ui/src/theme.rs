use gpui::{App, Global, Hsla, Pixels, SharedString, hsla, px, rgb, rgba};

/// The only file allowed to contain color literals. `scripts/check.sh` enforces it.
pub struct Theme {
    pub appearance: Appearance,
    pub density: Density,
    pub colors: Colors,
    pub fonts: Fonts,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Appearance {
    Light,
    #[default]
    Dark,
}

/// How tightly controls and rows are packed. Components size themselves through
/// [`Density::control`] and [`Density::pad`], so call sites never branch on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Density {
    Compact,
    #[default]
    Default,
    Spacious,
}

impl Density {
    pub const ALL: [Density; 3] = [Density::Compact, Density::Default, Density::Spacious];

    /// A control or bar height. Each step away from `Default` changes it by 4 px.
    pub fn control(self, base: Pixels) -> Pixels {
        base + px(match self {
            Density::Compact => -4.,
            Density::Default => 0.,
            Density::Spacious => 4.,
        })
    }

    /// Vertical padding inside a row.
    pub fn pad(self, base: Pixels) -> Pixels {
        base * match self {
            Density::Compact => 0.5,
            Density::Default => 1.,
            Density::Spacious => 1.5,
        }
    }
}

impl Global for Theme {}

pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}

#[derive(Clone, Copy)]
pub struct Colors {
    pub background: Hsla,
    pub rail: Hsla,
    pub surface: Hsla,
    pub surface_raised: Hsla,
    pub overlay: Hsla,

    pub element_hover: Hsla,
    pub element_active: Hsla,
    pub element_selected: Hsla,
    pub element_filled: Hsla,
    pub element_filled_hover: Hsla,

    pub border: Hsla,
    pub border_strong: Hsla,
    pub border_focused: Hsla,
    pub shadow: Hsla,
    /// The wash over the window behind a modal dialog.
    pub scrim: Hsla,

    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_faint: Hsla,
    pub text_placeholder: Hsla,

    pub accent: Hsla,
    pub danger: Hsla,
    pub success: Hsla,
    pub warning: Hsla,

    pub code_text: Hsla,
    pub selection: Hsla,
    pub caret: Hsla,
}

#[derive(Clone)]
pub struct Fonts {
    pub ui: SharedString,
    pub mono: SharedString,
}

impl Default for Fonts {
    fn default() -> Self {
        Self {
            ui: "Inter".into(),
            mono: "GeistMono Nerd Font Mono".into(),
        }
    }
}

fn white(alpha: f32) -> Hsla {
    hsla(0., 0., 1., alpha)
}

fn black(alpha: f32) -> Hsla {
    hsla(0., 0., 0., alpha)
}

impl Theme {
    pub fn new(appearance: Appearance, density: Density, fonts: Fonts) -> Self {
        Self {
            appearance,
            density,
            colors: match appearance {
                Appearance::Light => Colors::light(),
                Appearance::Dark => Colors::dark(),
            },
            fonts,
        }
    }

    pub fn dark() -> Self {
        Self::new(Appearance::Dark, Density::Default, Fonts::default())
    }

    pub fn light() -> Self {
        Self::new(Appearance::Light, Density::Default, Fonts::default())
    }
}

impl Colors {
    /// The charcoal MonoCode palette. Interaction states are translucent white
    /// washes so they read the same over the rail, the sidebar, and popovers.
    pub fn dark() -> Self {
        Self {
            background: rgb(0x171717).into(),
            rail: rgb(0x141414).into(),
            surface: rgb(0x1e1e1e).into(),
            surface_raised: rgb(0x232323).into(),
            overlay: rgb(0x252525).into(),

            element_hover: white(0.06),
            element_active: white(0.12),
            element_selected: white(0.09),
            element_filled: white(0.12),
            element_filled_hover: white(0.18),

            border: white(0.08),
            border_strong: white(0.14),
            border_focused: rgb(0x467bb0).into(),
            shadow: hsla(0., 0., 0., 0.45),
            scrim: black(0.5),

            text: rgb(0xc9c9c9).into(),
            text_muted: rgb(0x858585).into(),
            text_faint: rgb(0x626262).into(),
            text_placeholder: rgb(0x858585).into(),

            accent: rgb(0x6b9bd2).into(),
            danger: rgb(0xea9990).into(),
            success: rgb(0x8fc79a).into(),
            warning: rgb(0xe0b86a).into(),

            code_text: rgb(0x82a9ce).into(),
            selection: rgba(0x467bb070).into(),
            caret: rgb(0xc9c9c9).into(),
        }
    }

    /// A warm paper palette with the same structure as the dark one. Interaction
    /// states are translucent black washes.
    pub fn light() -> Self {
        Self {
            background: rgb(0xfafaf9).into(),
            rail: rgb(0xf2f2f0).into(),
            surface: rgb(0xffffff).into(),
            surface_raised: rgb(0xf6f6f4).into(),
            overlay: rgb(0xffffff).into(),

            element_hover: black(0.045),
            element_active: black(0.09),
            element_selected: black(0.065),
            element_filled: black(0.08),
            element_filled_hover: black(0.12),

            border: black(0.09),
            border_strong: black(0.16),
            border_focused: rgb(0x3a6ea5).into(),
            shadow: black(0.14),
            scrim: black(0.24),

            text: rgb(0x262626).into(),
            text_muted: rgb(0x6b6b6b).into(),
            text_faint: rgb(0x8f8f8f).into(),
            text_placeholder: rgb(0x8f8f8f).into(),

            accent: rgb(0x2f6db3).into(),
            danger: rgb(0xb53b2f).into(),
            success: rgb(0x2e7d43).into(),
            warning: rgb(0x9a6a12).into(),

            code_text: rgb(0x2f5f8f).into(),
            selection: rgba(0x3a6ea540).into(),
            caret: rgb(0x262626).into(),
        }
    }
}
