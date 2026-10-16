//! Font fallback for icon-font (Private Use Area) glyphs.
//!
//! When no shaping font covers a character, cosmic-text falls back to trying
//! every installed face in turn, loading and keeping each one. For a Private
//! Use Area icon that only one icon font provides, that is every face on the
//! system: on a desktop with a few thousand font files a single unmatched
//! format icon cost a ~300 ms frame and left ~780 MB resident.
//!
//! Instead, tablero finds the face for such a glyph itself, once per
//! character, by reading only each face's character map, and shapes the icon
//! with that face named explicitly. Ordinary text keeps cosmic-text's
//! script-aware fallback.

use std::collections::HashMap;

use cosmic_text::fontdb::{Database, Family, ID, Query};
use cosmic_text::{Style, Weight};

/// True for the Unicode Private Use Areas, where icon fonts place their glyphs.
pub(crate) fn is_private_use(c: char) -> bool {
    matches!(c as u32, 0xE000..=0xF8FF | 0xF_0000..=0xF_FFFD | 0x10_0000..=0x10_FFFD)
}

/// The face to shape one fallback glyph with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FallbackFace {
    pub(crate) family: String,
    pub(crate) weight: Weight,
    pub(crate) style: Style,
}

/// Per-process cache of resolved icon glyphs, valid for one primary family.
#[derive(Debug, Default)]
pub(crate) struct IconFallback {
    primary: Option<String>,
    faces: HashMap<char, Option<FallbackFace>>,
}

impl IconFallback {
    /// The face for Private Use Area glyph `c` when the `primary` family (or
    /// the default sans-serif) lacks it, scanning `db` on the first request.
    pub(crate) fn face_for(
        &mut self,
        db: &Database,
        primary: Option<&str>,
        c: char,
    ) -> Option<FallbackFace> {
        self.face_for_with(primary, c, || resolve(db, primary, c))
    }

    fn face_for_with(
        &mut self,
        primary: Option<&str>,
        c: char,
        resolve: impl FnOnce() -> Option<FallbackFace>,
    ) -> Option<FallbackFace> {
        if self.primary.as_deref() != primary {
            self.faces.clear();
            self.primary = primary.map(str::to_owned);
        }
        self.faces.entry(c).or_insert_with(resolve).clone()
    }
}

/// `None` when the primary face covers `c` or nothing does; otherwise the
/// first face in `db` that maps `c` to a glyph.
fn resolve(db: &Database, primary: Option<&str>, c: char) -> Option<FallbackFace> {
    let families = [primary.map_or(Family::SansSerif, Family::Name)];
    let query = Query {
        families: &families,
        ..Query::default()
    };
    if db.query(&query).is_some_and(|id| has_glyph(db, id, c)) {
        return None;
    }
    db.faces()
        .find(|face| has_glyph(db, face.id, c))
        .and_then(|face| {
            Some(FallbackFace {
                family: face.families.first()?.0.clone(),
                weight: face.weight,
                style: face.style,
            })
        })
}

/// Whether face `id` maps `c`, reading only its character map from a
/// temporary mapping (cosmic-text would keep the whole face loaded).
fn has_glyph(db: &Database, id: ID, c: char) -> bool {
    db.with_face_data(id, |data, index| {
        ttf_parser::Face::parse(data, index).is_ok_and(|face| face.glyph_index(c).is_some())
    })
    .unwrap_or(false)
}

/// Split `text` into runs that shape with the default attributes (`None`) and
/// runs of icon glyphs that need a fallback face. Only Private Use Area
/// characters are passed to `face_for`.
pub(crate) fn fallback_spans(
    text: &str,
    mut face_for: impl FnMut(char) -> Option<FallbackFace>,
) -> Vec<(&str, Option<FallbackFace>)> {
    let mut spans: Vec<(&str, Option<FallbackFace>)> = Vec::new();
    let mut start = 0;
    let mut current = None;
    for (i, c) in text.char_indices() {
        let face = if is_private_use(c) { face_for(c) } else { None };
        if face != current && i > start {
            spans.push((&text[start..i], current));
            start = i;
        }
        current = face;
    }
    if start < text.len() {
        spans.push((&text[start..], current));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(family: &str) -> FallbackFace {
        FallbackFace {
            family: family.to_owned(),
            weight: Weight::BLACK,
            style: Style::Normal,
        }
    }

    #[test]
    fn private_use_covers_the_bmp_area_and_both_supplementary_planes() {
        for c in [
            '\u{e000}',
            '\u{f863}',
            '\u{f8ff}',
            '\u{f0001}',
            '\u{10fffd}',
        ] {
            assert!(is_private_use(c), "{:x}", c as u32);
        }
        for c in ['a', '\u{2603}', '\u{f900}', '\u{1f600}'] {
            assert!(!is_private_use(c), "{:x}", c as u32);
        }
    }

    #[test]
    fn spans_isolate_icons_that_need_a_fallback_face() {
        let spans = fallback_spans("ab\u{f863}\u{f864}c\u{f0e7}", |c| {
            (c != '\u{f0e7}').then(|| face("Font Awesome 7 Free"))
        });
        assert_eq!(
            spans,
            vec![
                ("ab", None),
                ("\u{f863}\u{f864}", Some(face("Font Awesome 7 Free"))),
                ("c\u{f0e7}", None),
            ]
        );
    }

    #[test]
    fn spans_never_ask_about_ordinary_text() {
        let spans = fallback_spans("CPU 42%", |c| panic!("asked about {c:?}"));
        assert_eq!(spans, vec![("CPU 42%", None)]);
    }

    #[test]
    fn each_glyph_is_resolved_once_per_primary_family() {
        let mut cache = IconFallback::default();
        let mut scans = 0;
        for _ in 0..3 {
            let found = cache.face_for_with(None, '\u{f863}', || {
                scans += 1;
                Some(face("Font Awesome 7 Free"))
            });
            assert_eq!(found, Some(face("Font Awesome 7 Free")));
        }
        assert_eq!(scans, 1);

        // A different configured family may cover the glyph itself.
        let found = cache.face_for_with(Some("Symbols Nerd Font"), '\u{f863}', || {
            scans += 1;
            None
        });
        assert_eq!(found, None);
        assert_eq!(scans, 2);
    }

    #[test]
    fn an_empty_database_resolves_nothing() {
        assert_eq!(resolve(&Database::new(), None, '\u{f863}'), None);
    }
}
