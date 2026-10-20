use std::sync::LazyLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ResolvedFonts {
    pub ui: String,
    pub mono: String,
}

pub(super) fn resolve(ui: &str, mono: &str) -> Result<ResolvedFonts, String> {
    static SYSTEM: LazyLock<fontdb::Database> = LazyLock::new(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        db
    });
    resolve_in(&SYSTEM, ui, mono)
}

fn resolve_in(db: &fontdb::Database, ui: &str, mono: &str) -> Result<ResolvedFonts, String> {
    let choose = |requested: &str, fallbacks: &[&str], monospaced: bool| {
        let mut others: Vec<_> = db
            .faces()
            .filter(|f| f.monospaced == monospaced)
            .flat_map(|f| f.families.iter().map(|(name, _)| name.as_str()))
            .collect();
        others.sort_unstable();
        others.dedup();
        std::iter::once(requested)
            .chain(fallbacks.iter().copied())
            .chain(others)
            .find_map(|name| {
                let lower = name.to_ascii_lowercase();
                if name.len() > 128
                    || !name
                        .chars()
                        .all(|c| c.is_alphanumeric() || " -_,".contains(c))
                    || ["symbol", "dingbat", "emoji", "wingding", "webding"]
                        .iter()
                        .any(|s| lower.contains(s))
                {
                    return None;
                }
                let id = db.query(&fontdb::Query {
                    families: &[fontdb::Family::Name(name)],
                    ..Default::default()
                })?;
                let face = db.face(id)?;
                if monospaced && !face.monospaced {
                    return None;
                }
                let has_latin = db
                    .with_face_data(id, |data, index| {
                        ttf_parser::Face::parse(data, index).is_ok_and(|face| {
                            "AZaz09".chars().all(|c| face.glyph_index(c).is_some())
                        })
                    })
                    .unwrap_or(false);
                has_latin.then(|| face.families[0].0.clone())
            })
    };
    let ui = choose(ui, &[
        "Inter", "Helvetica", "Segoe UI", "Ubuntu", "Adwaita Sans", "Cantarell",
        "Noto Sans", "DejaVu Sans", "Liberation Sans", "Arial",
    ], false)
        .ok_or_else(|| "No installed non-symbol text font is available for diagrams; install a Unicode text font. Source is still available.".to_owned())?;
    let mono = choose(mono, &[
        "GeistMono Nerd Font Mono", "Lilex", "JetBrains Mono", "IBM Plex Mono",
        "Noto Sans Mono", "DejaVu Sans Mono", "Liberation Mono", "Fira Code",
        "Cascadia Mono", "Courier New",
    ], true)
        .ok_or_else(|| "No installed non-symbol monospace font is available for diagrams; install a Unicode monospace font. Source is still available.".to_owned())?;
    Ok(ResolvedFonts { ui, mono })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_symbol_theme_fonts_resolve_to_concrete_installed_text_faces() {
        let mut db = fontdb::Database::new();
        db.load_font_data(include_bytes!("../../tests/fixtures/fonts/symbol.ttf").to_vec());
        db.load_font_data(include_bytes!("../../tests/fixtures/fonts/sans.ttf").to_vec());
        db.load_font_data(include_bytes!("../../tests/fixtures/fonts/mono.ttf").to_vec());
        assert_eq!(
            db.faces().next().unwrap().families[0].0,
            "Keystone Test Symbol"
        );
        let missing = resolve_in(&db, "Inter", "Unavailable Mono").unwrap();
        assert_eq!(
            missing,
            ResolvedFonts {
                ui: "Keystone Test Sans".into(),
                mono: "Keystone Test Mono".into()
            }
        );
        let explicit = resolve_in(&db, "Keystone Test Sans", "Keystone Test Mono").unwrap();
        assert_eq!(explicit.ui, "Keystone Test Sans");
        let symbol = resolve_in(&db, "Keystone Test Symbol", "Keystone Test Symbol").unwrap();
        assert!(!symbol.ui.contains("Symbol") && !symbol.mono.contains("Symbol"));
        let mut symbol_only = fontdb::Database::new();
        symbol_only
            .load_font_data(include_bytes!("../../tests/fixtures/fonts/symbol.ttf").to_vec());
        assert!(resolve_in(&symbol_only, "Inter", "Unavailable").is_err());
    }

    #[test]
    fn svg_text_never_selects_a_symbol_first_face_when_the_theme_family_is_missing() {
        let mut db = fontdb::Database::new();
        db.load_font_data(include_bytes!("../../tests/fixtures/fonts/symbol.ttf").to_vec());
        db.load_font_data(include_bytes!("../../tests/fixtures/fonts/sans.ttf").to_vec());
        db.load_font_data(include_bytes!("../../tests/fixtures/fonts/mono.ttf").to_vec());
        let symbol = db.faces().next().unwrap().id;
        let fonts = resolve_in(&db, "Inter", "Unavailable Mono").unwrap();
        let selected = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = selected.clone();
        let default = resvg::usvg::FontResolver::default_font_selector();
        let options = resvg::usvg::Options {
            fontdb: std::sync::Arc::new(db),
            font_resolver: resvg::usvg::FontResolver {
                select_font: Box::new(move |font, db| {
                    let id = default(font, db)
                        .or_else(|| {
                            db.query(&fontdb::Query {
                                families: &[fontdb::Family::SansSerif],
                                ..Default::default()
                            })
                        })
                        .or_else(|| db.faces().next().map(|f| f.id));
                    if let Some(id) = id {
                        seen.lock().unwrap().push(id);
                    }
                    id
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        for (source, expected) in [
            ("flowchart TD\nA[Read source]-->B[User]", fonts.ui.as_str()),
            (
                "flowchart TD\nA[Read source]-->B[User]\nstyle A font-family:Inter",
                fonts.ui.as_str(),
            ),
            (
                "flowchart TD\nA[Read source]-->B[User]\nclassDef custom font-family:Inter\nclass A custom",
                fonts.ui.as_str(),
            ),
            (
                "flowchart TD\nA[Read source]-->B[User]\nstyle A font:bold 14px Inter",
                fonts.ui.as_str(),
            ),
            (
                "railroad-beta\nexpr = terminal(\"User\");",
                fonts.mono.as_str(),
            ),
        ] {
            let request = super::super::runtime::RenderRequest {
                source: source.into(),
                width: 640,
                palette: Default::default(),
                font_family: fonts.ui.clone(),
                font_family_mono: fonts.mono.clone(),
            };
            let image = super::super::runtime::render(
                request,
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
            .unwrap();
            assert!(
                std::str::from_utf8(&image.svg)
                    .unwrap()
                    .contains(&format!("font-family=\"{expected}\""))
            );
            let tree = resvg::usvg::Tree::from_data(&image.svg, &options).unwrap();
            assert!(!tree.root().children().is_empty());
        }
        let selected = selected.lock().unwrap();
        assert!(!selected.is_empty());
        assert!(selected.iter().all(|&id| id != symbol));
    }
}
