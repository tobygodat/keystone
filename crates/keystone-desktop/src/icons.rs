use gpui::{AssetSource, SharedString};
use std::borrow::Cow;

pub struct Icons;

impl AssetSource for Icons {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let shape = match path {
            "panel" => "<rect x='4' y='4' width='16' height='16' rx='3'/><path d='M9 4v16'/>",
            "chat" => {
                "<path d='M20 11a8 8 0 0 1-8 8H5l-3 3v-10a8 8 0 0 1 16-2'/><path d='M15 13h5v8l-3-2h-2a3 3 0 0 1 0-6Z'/>"
            }
            "search" => "<circle cx='10' cy='10' r='6'/><path d='m15 15 5 5'/>",
            "inbox" => "<path d='m4 5-2 9v6h20v-6l-2-9Z'/><path d='M2 14h6l2 3h4l2-3h6'/>",
            "file" => "<path d='M14 2H5v20h14V7Z M14 2v6h5'/>",
            "branch" => {
                "<circle cx='6' cy='4' r='2'/><circle cx='6' cy='20' r='2'/><circle cx='18' cy='4' r='2'/><path d='M6 6v12 M18 6v3a5 5 0 0 1-5 5H6'/>"
            }
            "note" => {
                "<rect x='4' y='3' width='16' height='18' rx='3'/><path d='M8 8h8 M8 12h6 M8 16h4'/>"
            }
            "settings" => {
                "<circle cx='12' cy='12' r='3'/><path d='m10 2 4 0 1 3 3 1 3 0 1 4-2 2 0 3 1 3-4 3-3-1-3 0-2 1-4-3 1-3 0-3-2-2 1-4 3 0 3-1Z'/>"
            }
            "plus" => "<path d='M12 4v16 M4 12h16'/>",
            "filter" => "<path d='M3 4h18l-7 8v7l-4 2v-9Z'/>",
            "chevron" => "<path d='m8 10 4 4 4-4'/>",
            "left" => "<path d='m14 6-6 6 6 6'/>",
            "right" => "<path d='m10 6 6 6-6 6'/>",
            "up" => "<path d='m5 12 7-7 7 7 M12 5v15'/>",
            "folder" => "<path d='M3 6h6l2 3h10v11H3Z M3 6V4h6l2 2h7v3'/>",
            "copy" => "<rect x='8' y='8' width='12' height='12' rx='2'/><path d='M16 8V4H4v12h4'/>",
            "close" => "<path d='m6 6 12 12 M18 6 6 18'/>",
            "minus" => "<path d='M5 12h14'/>",
            "square" => "<rect x='5' y='5' width='14' height='14' rx='2'/>",
            _ => return Ok(None),
        };
        Ok(Some(Cow::Owned(format!("<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='none' stroke='white' stroke-width='1.6' stroke-linecap='round' stroke-linejoin='round'>{shape}</svg>").into_bytes())))
    }

    fn list(&self, _: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}
