use gpui::{App, FontFeatures, Global, Hsla, Pixels, SharedString, hsla, px, rgb, rgba};
use std::sync::Arc;

/// The only file allowed to contain color literals. `scripts/check.sh` enforces it.
pub struct Theme {
    pub appearance: Appearance,
    pub density: Density,
    pub colors: Colors,
    pub fonts: Fonts,
    /// Multiplies every [`crate::TextSize`]. 1.0 keeps the built-in sizes.
    pub text_scale: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Appearance {
    Light,
    #[default]
    Dark,
}

/// How tightly controls and rows are packed. Components size themselves through
/// [`Density::control`], [`Density::pad`] and [`Density::space`], so call sites never
/// branch on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Density {
    Compact,
    #[default]
    Default,
    Spacious,
}

impl Density {
    pub const ALL: [Density; 3] = [Density::Compact, Density::Default, Density::Spacious];

    /// A control or bar height: `base` at `Spacious`, seven eighths of it at `Default` and
    /// three quarters at `Compact`, rounded to an even number of pixels so content centers
    /// on whole pixels. Nothing shrinks below 20 px, so controls keep a usable target.
    pub fn control(self, base: Pixels) -> Pixels {
        let scale = match self {
            Density::Compact => 0.75,
            Density::Default => 0.875,
            Density::Spacious => 1.,
        };
        ((base * scale / 2.).round() * 2.).max(base.min(px(20.)))
    }

    /// Vertical padding inside a row, in whole pixels.
    pub fn pad(self, base: Pixels) -> Pixels {
        (base
            * match self {
                Density::Compact => 0.5,
                Density::Default => 0.75,
                Density::Spacious => 1.,
            })
        .round()
    }

    /// Space between and around blocks of content, such as transcript rows and the
    /// composer's padding: `base` at `Default`, three quarters at `Compact` and five
    /// quarters at `Spacious`, rounded to the 4 px grid. Pass a base on that grid.
    pub fn space(self, base: Pixels) -> Pixels {
        let scale = match self {
            Density::Compact => 0.75,
            Density::Default => 1.,
            Density::Spacious => 1.25,
        };
        (base * scale / 4.).round() * 4.
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
    pub sidebar: Hsla,
    pub surface: Hsla,
    pub surface_raised: Hsla,
    pub overlay: Hsla,
    /// The floating composer card, with its own edge and soft shadow.
    pub composer: Hsla,
    pub composer_border: Hsla,
    pub composer_shadow: Hsla,

    pub element_hover: Hsla,
    pub element_active: Hsla,
    pub element_selected: Hsla,
    pub element_filled: Hsla,
    pub element_filled_hover: Hsla,
    pub element_disabled: Hsla,
    /// The strongest fill, for one primary action such as Send. Pairs with `text_inverse`.
    pub element_inverse: Hsla,
    /// Neutral selector fill and foreground. Model rows also use the inset ring
    /// and provider edge. Generic buttons retain their existing focus geometry.
    pub selection_fill: Hsla,
    pub selection_ring: Hsla,
    pub selection_edge: Hsla,

    pub border: Hsla,
    pub border_strong: Hsla,
    pub border_focused: Hsla,
    pub shadow: Hsla,
    /// The wash over the window behind a modal dialog.
    pub scrim: Hsla,

    pub text: Hsla,
    pub text_strong: Hsla,
    pub text_muted: Hsla,
    pub text_faint: Hsla,
    pub text_placeholder: Hsla,
    pub text_inverse: Hsla,

    pub accent: Hsla,
    pub danger: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub provider_claude: Hsla,
    pub provider_codex: Hsla,

    pub code_text: Hsla,
    pub selection: Hsla,
    pub caret: Hsla,

    /// Project tiles, in the order of `keystone_core::ProjectColor::ALL`.
    pub tiles: [Tint; 7],
}

/// A colored tile and the text on it.
#[derive(Clone, Copy)]
pub struct Tint {
    pub fill: Hsla,
    pub text: Hsla,
}

/// White text on each fill, at least 4.5:1, and each fill at least 3:1 against the
/// sidebar, overlay and background. A highlighted row's wash is not covered.
fn tiles(fills: [u32; 7]) -> [Tint; 7] {
    fills.map(|fill| Tint {
        fill: rgb(fill).into(),
        text: rgb(0xffffff).into(),
    })
}

#[derive(Clone)]
pub struct Fonts {
    pub ui: SharedString,
    /// SemiBold text at [`crate::TextSize::Title`] and top-level Markdown headings. Inter
    /// Display for the bundled Inter, whose tighter spacing suits large sizes, otherwise
    /// the interface font. Only its SemiBold faces are bundled.
    pub display: SharedString,
    pub mono: SharedString,
    /// OpenType features for interface text. Only the bundled Inter has any.
    pub features: FontFeatures,
}

impl Default for Fonts {
    fn default() -> Self {
        Self {
            ui: "Inter".into(),
            display: "Inter Display".into(),
            mono: "GeistMono Nerd Font Mono".into(),
            // The plain-footed 1 and Linear's pair of Inter alternates.
            features: FontFeatures(Arc::new(vec![("cv01".into(), 1), ("ss03".into(), 1)])),
        }
    }
}

impl Fonts {
    /// Uses `font` for interface text and titles. Inter's features are kept only for
    /// Inter, since another font's stylistic sets mean something else.
    pub fn with_ui(self, font: impl Into<SharedString>) -> Self {
        let font = font.into();
        if font == self.ui {
            return self;
        }
        Self {
            display: font.clone(),
            ui: font,
            features: FontFeatures::default(),
            ..self
        }
    }

    /// [`Self::features`] with tabular figures, for numbers that line up in a column.
    pub fn tabular(&self) -> FontFeatures {
        let mut features = self.features.0.as_ref().clone();
        features.push(("tnum".into(), 1));
        FontFeatures(Arc::new(features))
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
            text_scale: 1.,
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
    /// MonoCode's calm dark: near-equal surfaces, #ebebeb ink in opaque tiers, and washes
    /// and strokes of that ink. Primary actions are ink-filled; blue is only focus and links.
    pub fn dark() -> Self {
        Self {
            background: rgb(0x171717).into(),
            sidebar: rgb(0x151515).into(),
            surface: rgb(0x1b1b1b).into(),
            surface_raised: rgb(0x1e1e1e).into(),
            overlay: rgb(0x1e1e1e).into(),
            composer: rgb(0x1b1b1b).into(),
            composer_border: white(0.08),
            composer_shadow: black(0.24),

            element_hover: white(0.05),
            element_active: white(0.15),
            element_selected: white(0.08),
            element_filled: white(0.08),
            element_filled_hover: white(0.12),
            element_disabled: white(0.06),
            element_inverse: rgb(0xebebeb).into(),
            selection_fill: white(0.10),
            selection_ring: white(0.12),
            selection_edge: rgb(0xebebeb).into(),

            border: white(0.07),
            border_strong: white(0.10),
            border_focused: rgb(0x5b87d6).into(),
            shadow: black(0.5),
            scrim: black(0.5),

            text: rgb(0xebebeb).into(),
            text_strong: rgb(0xfafafa).into(),
            text_muted: rgb(0xababab).into(),
            text_faint: rgb(0x969696).into(),
            text_placeholder: rgb(0x969696).into(),
            text_inverse: rgb(0x171717).into(),

            accent: rgb(0x7ea3e3).into(),
            danger: rgb(0xe5847a).into(),
            success: rgb(0x7fbf8e).into(),
            warning: rgb(0xd9b26a).into(),
            provider_claude: rgb(0xd9805f).into(),
            provider_codex: rgb(0x93a6e0).into(),

            code_text: rgb(0xc8c8c8).into(),
            selection: rgba(0x5b87d660).into(),
            caret: rgb(0xebebeb).into(),

            tiles: tiles([
                0xaa5a54, 0x946749, 0x3e7d53, 0x3c7b78, 0x5072a3, 0x7f62b2, 0xaa5386,
            ]),
        }
    }

    /// A neutral light palette with the same structure as the dark one. Interaction
    /// states are translucent black washes.
    pub fn light() -> Self {
        Self {
            background: rgb(0xffffff).into(),
            sidebar: rgb(0xf8f8f8).into(),
            surface: rgb(0xffffff).into(),
            surface_raised: rgb(0xf5f5f5).into(),
            overlay: rgb(0xffffff).into(),
            composer: rgb(0xffffff).into(),
            composer_border: black(0.10),
            composer_shadow: black(0.05),

            element_hover: black(0.04),
            element_active: black(0.10),
            element_selected: black(0.06),
            element_filled: black(0.05),
            element_filled_hover: black(0.08),
            element_disabled: black(0.05),
            element_inverse: rgb(0x171717).into(),
            selection_fill: black(0.07),
            selection_ring: black(0.10),
            selection_edge: rgb(0x171717).into(),

            border: black(0.08),
            border_strong: black(0.12),
            border_focused: rgb(0x3a6ea5).into(),
            shadow: black(0.12),
            scrim: black(0.20),

            text: rgb(0x171717).into(),
            text_strong: rgb(0x0a0a0a).into(),
            text_muted: rgb(0x5c5c5c).into(),
            text_faint: rgb(0x6e6e6e).into(),
            text_placeholder: rgb(0x8f8f8f).into(),
            text_inverse: rgb(0xffffff).into(),

            accent: rgb(0x2f6db3).into(),
            danger: rgb(0xb53b2f).into(),
            success: rgb(0x2e7d43).into(),
            warning: rgb(0x8f6210).into(),
            provider_claude: rgb(0xb4593a).into(),
            provider_codex: rgb(0x4a5fb5).into(),

            code_text: rgb(0x3d3d3d).into(),
            selection: rgba(0x3a6ea540).into(),
            caret: rgb(0x171717).into(),

            tiles: tiles([
                0xae4e47, 0x94603d, 0x32794a, 0x307674, 0x446da7, 0x7d58bb, 0xad4682,
            ]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn density_sizes_are_even_whole_pixels() {
        let controls = Density::ALL.map(|d| [16., 24., 28., 44.].map(|base| d.control(px(base))));
        assert_eq!(
            controls,
            [
                [16., 20., 22., 34.],
                [16., 22., 24., 38.],
                [16., 24., 28., 44.]
            ]
            .map(|row| row.map(px))
        );
        assert_eq!(Density::ALL.map(|d| d.pad(px(7.))), [4., 5., 7.].map(px));
    }

    #[test]
    fn density_space_stays_on_the_grid() {
        let space = Density::ALL.map(|d| [12., 16., 20., 32.].map(|base| d.space(px(base))));
        assert_eq!(
            space,
            [
                [8., 12., 16., 24.],
                [12., 16., 20., 32.],
                [16., 20., 24., 40.]
            ]
            .map(|row| row.map(px))
        );
    }

    fn contrast(a: Hsla, b: Hsla) -> f32 {
        let luminance = |color: Hsla| {
            let rgba = color.to_rgb();
            let channel = |c: f32| {
                if c <= 0.03928 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(rgba.r) + 0.7152 * channel(rgba.g) + 0.0722 * channel(rgba.b)
        };
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn project_tiles_meet_contrast_in_both_themes() {
        for colors in [Colors::light(), Colors::dark()] {
            for tint in colors.tiles {
                assert!(contrast(tint.text, tint.fill) >= 4.5, "{:?}", tint.fill);
                for surface in [colors.sidebar, colors.overlay, colors.background] {
                    assert!(contrast(tint.fill, surface) >= 3., "{:?}", tint.fill);
                }
            }
        }
    }

    #[test]
    fn ink_text_reads_on_every_surface_in_both_themes() {
        for (appearance, colors) in [("light", Colors::light()), ("dark", Colors::dark())] {
            let action = contrast(colors.text_inverse, colors.element_inverse);
            eprintln!("CONTRAST {appearance} text_inverse/element_inverse={action:.3}");
            assert!(action >= 4.5, "{appearance} primary action: {action}");
            for (surface_name, surface) in [
                ("background", colors.background),
                ("sidebar", colors.sidebar),
                ("surface", colors.surface),
                ("surface_raised", colors.surface_raised),
                ("overlay", colors.overlay),
                ("composer", colors.composer),
            ] {
                for (role, foreground) in [
                    ("text", colors.text),
                    ("text_strong", colors.text_strong),
                    ("text_muted", colors.text_muted),
                    ("text_faint", colors.text_faint),
                    ("code_text", colors.code_text),
                    ("element_inverse", colors.element_inverse),
                ] {
                    let ratio = contrast(foreground, surface);
                    eprintln!("CONTRAST {appearance} {role}/{surface_name}={ratio:.3}");
                    assert!(ratio >= 4.5, "{appearance} {role}/{surface_name}: {ratio}");
                }
            }
        }
    }

    #[test]
    fn another_interface_font_drops_inter_display_and_features() {
        let inter = Fonts::default();
        assert_eq!(inter.clone().with_ui("Inter").features, inter.features);

        let other = Fonts::default().with_ui("Noto Sans");
        assert_eq!(other.ui.as_ref(), "Noto Sans");
        assert_eq!(other.display.as_ref(), "Noto Sans");
        assert_eq!(other.mono, inter.mono);
        assert!(other.features.tag_value_list().is_empty());
        assert_eq!(other.tabular().tag_value_list(), [("tnum".to_owned(), 1)]);
    }
}
