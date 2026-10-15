use gpui::{App, Global, Hsla, SharedString, hsla, rgb, rgba};

/// The only file allowed to contain color literals. `scripts/check.sh` enforces it.
pub struct Theme {
    pub colors: Colors,
    pub fonts: Fonts,
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

pub struct Fonts {
    pub ui: SharedString,
    pub mono: SharedString,
}

fn white(alpha: f32) -> Hsla {
    hsla(0., 0., 1., alpha)
}

impl Theme {
    /// The charcoal MonoCode palette. Interaction states are translucent white
    /// washes so they read the same over the rail, the sidebar, and popovers.
    pub fn dark() -> Self {
        Self {
            colors: Colors {
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
            },
            fonts: Fonts {
                ui: "Inter".into(),
                mono: "GeistMono Nerd Font Mono".into(),
            },
        }
    }
}
