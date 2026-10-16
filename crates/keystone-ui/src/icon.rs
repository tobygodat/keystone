use crate::color::Color;
use gpui::{
    App, AssetSource, IntoElement, Pixels, RenderOnce, SharedString, Styled, Window, px, svg,
};
use std::borrow::Cow;

macro_rules! icons {
    ($($name:ident => $path:literal, $shape:literal;)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum IconName { $($name),* }

        impl IconName {
            pub const ALL: &[IconName] = &[$(IconName::$name),*];

            /// The `AssetSource` key for this icon.
            pub fn path(self) -> &'static str {
                match self { $(IconName::$name => $path),* }
            }

            fn shape(self) -> &'static str {
                match self { $(IconName::$name => $shape),* }
            }
        }
    };
}

icons! {
    Panel => "icons/panel.svg", "<rect x='4' y='4' width='16' height='16' rx='3'/><path d='M9 4v16'/>";
    Chat => "icons/chat.svg", "<path d='M20 11a8 8 0 0 1-8 8H5l-3 3v-10a8 8 0 0 1 16-2'/><path d='M15 13h5v8l-3-2h-2a3 3 0 0 1 0-6Z'/>";
    Search => "icons/search.svg", "<circle cx='10' cy='10' r='6'/><path d='m15 15 5 5'/>";
    Inbox => "icons/inbox.svg", "<path d='m4 5-2 9v6h20v-6l-2-9Z'/><path d='M2 14h6l2 3h4l2-3h6'/>";
    File => "icons/file.svg", "<path d='M14 2H5v20h14V7Z M14 2v6h5'/>";
    Branch => "icons/branch.svg", "<circle cx='6' cy='4' r='2'/><circle cx='6' cy='20' r='2'/><circle cx='18' cy='4' r='2'/><path d='M6 6v12 M18 6v3a5 5 0 0 1-5 5H6'/>";
    Note => "icons/note.svg", "<rect x='4' y='3' width='16' height='18' rx='3'/><path d='M8 8h8 M8 12h6 M8 16h4'/>";
    Settings => "icons/settings.svg", "<circle cx='12' cy='12' r='3'/><path d='m10 2 4 0 1 3 3 1 3 0 1 4-2 2 0 3 1 3-4 3-3-1-3 0-2 1-4-3 1-3 0-3-2-2 1-4 3 0 3-1Z'/>";
    Plus => "icons/plus.svg", "<path d='M12 4v16 M4 12h16'/>";
    Filter => "icons/filter.svg", "<path d='M3 4h18l-7 8v7l-4 2v-9Z'/>";
    Chevron => "icons/chevron.svg", "<path d='m8 10 4 4 4-4'/>";
    Left => "icons/left.svg", "<path d='m14 6-6 6 6 6'/>";
    Right => "icons/right.svg", "<path d='m10 6 6 6-6 6'/>";
    Up => "icons/up.svg", "<path d='m5 12 7-7 7 7 M12 5v15'/>";
    Folder => "icons/folder.svg", "<path d='M3 6h6l2 3h10v11H3Z M3 6V4h6l2 2h7v3'/>";
    Copy => "icons/copy.svg", "<rect x='8' y='8' width='12' height='12' rx='2'/><path d='M16 8V4H4v12h4'/>";
    Close => "icons/close.svg", "<path d='m6 6 12 12 M18 6 6 18'/>";
    Minus => "icons/minus.svg", "<path d='M5 12h14'/>";
    Square => "icons/square.svg", "<rect x='5' y='5' width='14' height='14' rx='2'/>";
    Check => "icons/check.svg", "<path d='m5 12 5 5 9-10'/>";
    Shield => "icons/shield.svg", "<path d='M12 3 4 6v6c0 5 3.5 8 8 9 4.5-1 8-4 8-9V6Z'/>";
}

/// Serves every `IconName` as a white-stroked SVG. GPUI tints it with `text_color`.
pub struct Icons;

impl AssetSource for Icons {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let Some(icon) = IconName::ALL.iter().find(|icon| icon.path() == path) else {
            return Ok(None);
        };
        let svg = format!(
            "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='none' stroke='white' stroke-width='1.6' stroke-linecap='round' stroke-linejoin='round'>{}</svg>",
            icon.shape()
        );
        Ok(Some(Cow::Owned(svg.into_bytes())))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(IconName::ALL
            .iter()
            .map(|icon| icon.path())
            .filter(|key| key.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}

#[derive(IntoElement)]
pub struct Icon {
    name: IconName,
    color: Color,
    size: Pixels,
}

impl Icon {
    pub fn new(name: IconName) -> Self {
        Self {
            name,
            color: Color::Muted,
            size: px(16.),
        }
    }

    pub fn color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    pub fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }
}

impl RenderOnce for Icon {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        svg()
            .path(self.name.path())
            .flex_shrink_0()
            .size(self.size)
            .text_color(self.color.to_hsla(cx))
    }
}
