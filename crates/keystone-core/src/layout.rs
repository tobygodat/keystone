//! Where the session sidebar sits and how wide it is.

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarSide {
    #[default]
    Left,
    Right,
}

impl SidebarSide {
    pub fn label(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

/// A preferred sidebar width in logical pixels, always within
/// [`SidebarWidth::MIN`]..=[`SidebarWidth::MAX`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SidebarWidth(u16);

impl SidebarWidth {
    pub const MIN: u16 = 220;
    pub const MAX: u16 = 520;
    pub const DEFAULT: Self = Self(302);

    /// `None` outside the allowed range.
    pub fn new(px: u16) -> Option<Self> {
        (Self::MIN..=Self::MAX).contains(&px).then_some(Self(px))
    }

    /// The nearest allowed width, for drags and steps that may overshoot.
    pub fn clamped(px: f32) -> Self {
        let px = if px.is_finite() { px.round() } else { 0. };
        Self(px.clamp(Self::MIN.into(), Self::MAX.into()) as u16)
    }

    pub fn px(self) -> u16 {
        self.0
    }
}

impl Default for SidebarWidth {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl<'de> Deserialize<'de> for SidebarWidth {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let px = u16::deserialize(deserializer)?;
        Self::new(px).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "sidebar width {px} is outside {}..={}",
                Self::MIN,
                Self::MAX
            ))
        })
    }
}

/// The main column keeps at least this much of the window before the sidebar
/// gives up width.
pub const MAIN_MIN_WIDTH: f32 = 360.;
/// The hairline between the sidebar and the main column.
pub const DIVIDER_WIDTH: f32 = 1.;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SidebarLayout {
    pub side: SidebarSide,
    #[serde(rename = "width_px")]
    pub width: SidebarWidth,
}

impl SidebarLayout {
    /// The width to draw in a window `window_width` wide. A narrow window takes width
    /// from the sidebar, even below [`SidebarWidth::MIN`], and never changes the
    /// preference.
    pub fn fit(self, window_width: f32) -> f32 {
        let room = (window_width - MAIN_MIN_WIDTH - DIVIDER_WIDTH).max(0.);
        f32::from(self.width.px()).min(room)
    }
}

/// Reads a saved layout. Anything missing or invalid, such as an unknown side, falls
/// back to the whole default layout rather than failing to open the workspace.
pub(crate) fn lenient<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<SidebarLayout, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_accepts_only_the_allowed_range() {
        assert_eq!(SidebarWidth::new(219), None);
        assert_eq!(SidebarWidth::new(220).map(SidebarWidth::px), Some(220));
        assert_eq!(SidebarWidth::new(520).map(SidebarWidth::px), Some(520));
        assert_eq!(SidebarWidth::new(521), None);
        assert_eq!(SidebarWidth::clamped(10.).px(), 220);
        assert_eq!(SidebarWidth::clamped(9_999.).px(), 520);
        assert_eq!(SidebarWidth::clamped(f32::NAN).px(), 220);
        assert_eq!(SidebarWidth::clamped(333.4).px(), 333);
    }

    #[test]
    fn fit_gives_way_to_a_narrow_window_down_to_zero() {
        let layout = SidebarLayout {
            side: SidebarSide::Right,
            width: SidebarWidth::new(400).unwrap(),
        };
        assert_eq!(layout.fit(1_100.), 400.);
        assert_eq!(layout.fit(700.), 339.);
        assert_eq!(layout.fit(500.), 139.);
        assert_eq!(layout.fit(300.), 0.);
    }

    #[test]
    fn saved_layouts_round_trip_and_reject_unknown_values() {
        let layout = SidebarLayout {
            side: SidebarSide::Right,
            width: SidebarWidth::new(360).unwrap(),
        };
        let json = serde_json::to_string(&layout).unwrap();
        assert_eq!(json, r#"{"side":"right","width_px":360}"#);
        assert_eq!(
            serde_json::from_str::<SidebarLayout>(&json).unwrap(),
            layout
        );
        for bad in [
            r#"{"side":"top","width_px":360}"#,
            r#"{"side":"left","width_px":100}"#,
            r#"{"side":"left","width_px":360,"extra":1}"#,
            r#"{"side":"left","width_px":-5}"#,
        ] {
            assert!(serde_json::from_str::<SidebarLayout>(bad).is_err(), "{bad}");
        }
    }
}
