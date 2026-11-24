//! vue-native fork: CSS `font-family` lists and composed families, for a platform text system
//! whose own fallback knows neither. Each character is drawn with the first family of the list
//! whose face covers it (its `unicode-range`) and has a glyph for it; one no family draws goes to
//! the first family's face, whose platform fallback takes it, and a character no font at all has
//! is that face's glyph 0.

use std::{borrow::Cow, sync::Arc};

use collections::FxHashMap;

use crate::{ComposedFace, FamilyComposer, Font, FontFallbacks, FontId, FontRun, SharedString};

use super::split_by_face_with;

/// The members of a listed or composed font, as resolved for its weight and style.
#[derive(Debug)]
pub struct FontList {
    /// In the order they are tried for a character.
    pub faces: Vec<ComposedFace>,
    /// `faces[i]`'s font.
    pub ids: Vec<FontId>,
    /// What draws a character no member draws: the next family of the list, else `primary`.
    pub rest: FontId,
    /// The face that stands for the family (CSS's first available font), which draws a character
    /// no family of the list has a glyph for, by the platform's own fallback.
    pub primary: FontId,
}

/// The platform side of [`resolve_listed_font`].
pub trait FontListHost {
    /// `font`'s family alone, as the platform resolves a family name.
    fn plain_font(&mut self, font: &Font) -> Option<FontId>;
    /// A new font id drawn and measured as `of`, which [`FontLists`] keeps as a list's.
    fn alias_font(&mut self, of: FontId) -> FontId;
    /// Where the platform keeps its [`FontLists`].
    fn font_lists(&mut self) -> &mut FontLists;
}

/// The composed families and the listed fonts resolved so far.
#[derive(Default)]
pub struct FontLists {
    composed: FxHashMap<String, FamilyComposer>,
    resolved: FxHashMap<Font, Option<FontId>>,
    lists: FxHashMap<FontId, Arc<FontList>>,
}

impl FontLists {
    /// Replaces the composed families; fonts already resolved resolve again.
    pub fn set_composed_families(&mut self, families: Vec<(SharedString, FamilyComposer)>) {
        self.composed = families
            .into_iter()
            .map(|(family, composer)| (family.to_lowercase(), composer))
            .collect();
        self.resolved.clear();
    }

    /// Forgets the fonts resolved so far, for a font set that changed.
    pub fn clear_resolved(&mut self) {
        self.resolved.clear();
    }

    /// Whether `font` is resolved by [`resolve_listed_font`] rather than by family name alone.
    pub fn takes(&self, font: &Font) -> bool {
        font.fallbacks
            .as_ref()
            .is_some_and(|fallbacks| !fallbacks.fallback_list().is_empty())
            || (!self.composed.is_empty()
                && self.composed.contains_key(&font.family.to_lowercase()))
    }

    /// The list `font_id` stands for, if any.
    pub fn list(&self, font_id: FontId) -> Option<&FontList> {
        self.lists.get(&font_id).map(Arc::as_ref)
    }

    /// `runs` with every run of a listed font split into runs of its members
    /// ([`split_font_runs`]); `runs` itself when none is.
    pub fn split_runs<'a>(
        &self,
        text: &str,
        runs: &'a [FontRun],
        has_glyph: impl Fn(FontId, char) -> bool,
    ) -> Cow<'a, [FontRun]> {
        if self.lists.is_empty() || !runs.iter().any(|run| self.lists.contains_key(&run.font_id)) {
            return Cow::Borrowed(runs);
        }
        Cow::Owned(split_font_runs(text, runs, |id| self.list(id), has_glyph))
    }
}

/// `font`'s id when [`FontLists::takes`] it. A composed family's members are the faces its
/// composer names, each resolved as the family it is registered under; the face that stands for
/// it is the first member that covers U+0020, else the next family, else the first member. An
/// installed family followed by others is a list of that family alone. When the family does not
/// resolve, the rest of the list stands for it.
pub fn resolve_listed_font(host: &mut impl FontListHost, font: &Font) -> Option<FontId> {
    resolve(host, font, 0)
}

fn resolve(host: &mut impl FontListHost, font: &Font, depth: usize) -> Option<FontId> {
    if !host.font_lists().takes(font) {
        return host.plain_font(font);
    }
    if let Some(&font_id) = host.font_lists().resolved.get(font) {
        return font_id;
    }
    // Only a list that names itself goes this deep.
    if depth > 16 {
        return None;
    }
    let composer = host
        .font_lists()
        .composed
        .get(&font.family.to_lowercase())
        .cloned();
    let mut faces = Vec::new();
    let mut ids = Vec::new();
    if let Some(composer) = composer {
        for face in composer(font) {
            if face.family.eq_ignore_ascii_case(&font.family) {
                continue;
            }
            let member = face.member(font);
            if let Some(font_id) = host.plain_font(&member) {
                faces.push(face);
                ids.push(font_id);
            }
        }
    } else {
        let own = Font {
            fallbacks: None,
            ..font.clone()
        };
        if let Some(font_id) = host.plain_font(&own) {
            faces.push(ComposedFace {
                family: font.family.clone(),
                ranges: Arc::from([]),
                descriptors: Default::default(),
            });
            ids.push(font_id);
        }
    }
    let next = next_family(font).and_then(|next| resolve(host, &next, depth + 1));
    let spacer = faces
        .iter()
        .position(|face| face.covers(' '))
        .map(|i| ids[i]);
    let font_id = match spacer.or(next).or_else(|| ids.first().copied()) {
        Some(primary) if !ids.is_empty() => {
            let rest = next.unwrap_or(primary);
            let font_id = host.alias_font(primary);
            host.font_lists().lists.insert(
                font_id,
                Arc::new(FontList {
                    faces,
                    ids,
                    rest,
                    primary,
                }),
            );
            Some(font_id)
        }
        other => other,
    };
    host.font_lists().resolved.insert(font.clone(), font_id);
    font_id
}

/// The first family of `font.fallbacks`, with the others as its own.
fn next_family(font: &Font) -> Option<Font> {
    let (first, rest) = font.fallbacks.as_ref()?.fallback_list().split_first()?;
    Some(Font {
        family: first.clone().into(),
        features: font.features.clone(),
        fallbacks: (!rest.is_empty()).then(|| FontFallbacks::from_fonts(rest.to_vec())),
        ..font.clone()
    })
}

/// `runs` with every run of a listed font replaced by runs of its members, split by character
/// ([`split_by_face_with`]): a member draws a character its range covers and its face has a glyph
/// for. A character no member draws goes to the list's `rest`, split again if that is a list too,
/// and one no font of the list draws to the first list's `primary`. Pieces of one run that land in
/// the same font are one run; runs are never merged across the runs given, which may differ in
/// spacing.
pub fn split_font_runs<'a>(
    text: &str,
    runs: &[FontRun],
    list_of: impl Fn(FontId) -> Option<&'a FontList>,
    has_glyph: impl Fn(FontId, char) -> bool,
) -> Vec<FontRun> {
    struct Splitter<L, G> {
        list_of: L,
        has_glyph: G,
    }

    impl<'a, L: Fn(FontId) -> Option<&'a FontList>, G: Fn(FontId, char) -> bool> Splitter<L, G> {
        fn split(
            &self,
            text: &str,
            run: &FontRun,
            font_id: FontId,
            primary: Option<FontId>,
            depth: usize,
            out: &mut Vec<FontRun>,
        ) {
            match (self.list_of)(font_id) {
                Some(list) if depth < 16 => {
                    let primary = primary.unwrap_or(list.primary);
                    let mut at = 0;
                    let pieces = split_by_face_with(text, list.faces.len(), |i, c| {
                        list.faces[i].covers(c) && (self.has_glyph)(list.ids[i], c)
                    });
                    for (len, face) in pieces {
                        let piece = &text[at..at + len];
                        at += len;
                        match face {
                            Some(i) => out.push(FontRun {
                                len,
                                font_id: list.ids[i],
                                ..*run
                            }),
                            None => {
                                self.split(piece, run, list.rest, Some(primary), depth + 1, out)
                            }
                        }
                    }
                }
                // The last family of a list draws what it has glyphs for.
                _ if primary.is_some_and(|primary| primary != font_id) => {
                    let primary = primary.unwrap_or(font_id);
                    for (len, face) in
                        split_by_face_with(text, 1, |_, c| (self.has_glyph)(font_id, c))
                    {
                        out.push(FontRun {
                            len,
                            font_id: if face.is_some() { font_id } else { primary },
                            ..*run
                        });
                    }
                }
                _ => out.push(FontRun {
                    len: text.len(),
                    font_id,
                    ..*run
                }),
            }
        }
    }

    let splitter = Splitter { list_of, has_glyph };
    let mut out: Vec<FontRun> = Vec::with_capacity(runs.len() + 2);
    let mut pieces = Vec::new();
    let mut offset = 0;
    for run in runs {
        let piece = &text[offset..offset + run.len];
        offset += run.len;
        pieces.clear();
        splitter.split(piece, run, run.font_id, None, 0, &mut pieces);
        let start = out.len();
        for piece in pieces.drain(..) {
            if out.len() > start
                && let Some(last) = out.last_mut()
                && last.font_id == piece.font_id
            {
                last.len += piece.len;
            } else {
                out.push(piece);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FaceDescriptors, FontWeight, px};

    /// Families by name, each with the characters it has glyphs for.
    struct Host {
        families: Vec<(&'static str, fn(char) -> bool)>,
        aliases: Vec<FontId>,
        lists: FontLists,
        asked: Vec<(SharedString, FontWeight)>,
    }

    impl Host {
        fn new() -> Self {
            Self {
                families: vec![
                    ("Latin", |c| c.is_ascii()),
                    ("Han", |c| {
                        ('\u{4E00}'..='\u{9FFF}').contains(&c) || c == ' '
                    }),
                    ("Kana", |c| ('\u{3040}'..='\u{30FF}').contains(&c)),
                    ("Digits", |c| c.is_ascii_digit()),
                ],
                aliases: Vec::new(),
                lists: FontLists::default(),
                asked: Vec::new(),
            }
        }

        fn id(&self, family: &str) -> FontId {
            FontId(self.families.iter().position(|f| f.0 == family).unwrap())
        }

        fn has_glyph(&self, font_id: FontId, c: char) -> bool {
            let font_id = self.real(font_id);
            (self.families[font_id.0].1)(c)
        }

        fn real(&self, font_id: FontId) -> FontId {
            match font_id.0.checked_sub(self.families.len()) {
                Some(i) => self.aliases[i],
                None => font_id,
            }
        }

        fn split(&self, text: &str, font_id: FontId) -> Vec<(String, FontId)> {
            let runs = [FontRun {
                len: text.len(),
                font_id,
                tracking: px(0.),
                word_spacing: px(0.),
            }];
            let mut at = 0;
            self.lists
                .split_runs(text, &runs, |id, c| self.has_glyph(id, c))
                .iter()
                .map(|run| {
                    let piece = text[at..at + run.len].to_string();
                    at += run.len;
                    (piece, run.font_id)
                })
                .collect()
        }
    }

    impl FontListHost for Host {
        fn plain_font(&mut self, font: &Font) -> Option<FontId> {
            self.asked.push((font.family.clone(), font.weight));
            let i = self.families.iter().position(|f| font.family == f.0)?;
            Some(FontId(i))
        }

        fn alias_font(&mut self, of: FontId) -> FontId {
            self.aliases.push(self.real(of));
            FontId(self.families.len() + self.aliases.len() - 1)
        }

        fn font_lists(&mut self) -> &mut FontLists {
            &mut self.lists
        }
    }

    fn listed(family: &str, fallbacks: &[&str]) -> Font {
        Font {
            family: family.to_string().into(),
            fallbacks: Some(FontFallbacks::from_fonts(
                fallbacks.iter().map(|f| f.to_string()).collect(),
            )),
            ..Default::default()
        }
    }

    #[test]
    fn each_character_takes_the_first_listed_family_with_its_glyph() {
        let mut host = Host::new();
        let font = resolve_listed_font(&mut host, &listed("Latin", &["Kana", "Han"])).unwrap();
        assert_eq!(host.real(font), host.id("Latin"));
        assert_eq!(
            host.split("ab 设置かな\u{10FFFD}", font),
            vec![
                ("ab ".into(), host.id("Latin")),
                ("设置".into(), host.id("Han")),
                ("かな".into(), host.id("Kana")),
                ("\u{10FFFD}".into(), host.id("Latin")),
            ]
        );
    }

    #[test]
    fn a_family_that_does_not_resolve_leaves_the_list_to_the_rest() {
        let mut host = Host::new();
        let font = resolve_listed_font(&mut host, &listed("Missing", &["Han", "Latin"])).unwrap();
        assert_eq!(host.real(font), host.id("Han"));
        assert_eq!(
            host.split("a设", font),
            vec![
                ("a".into(), host.id("Latin")),
                ("设".into(), host.id("Han"))
            ]
        );
    }

    #[test]
    fn a_composed_face_draws_only_what_its_range_covers_and_it_has_a_glyph_for() {
        let mut host = Host::new();
        let digits: FamilyComposer = Arc::new(|_: &Font| {
            vec![
                ComposedFace {
                    family: "Digits".into(),
                    ranges: Arc::from([(0x30, 0x35)]),
                    descriptors: Default::default(),
                },
                ComposedFace {
                    family: "Kana".into(),
                    ranges: Arc::from([(0x3040, 0x30FF)]),
                    descriptors: Default::default(),
                },
            ]
        });
        host.lists
            .set_composed_families(vec![("Web".into(), digits)]);
        let font = resolve_listed_font(&mut host, &listed("Web", &["Latin"])).unwrap();
        // No member covers U+0020, so the next family stands for the composed one.
        assert_eq!(host.real(font), host.id("Latin"));
        assert_eq!(
            host.split("19か", font),
            vec![
                ("1".into(), host.id("Digits")),
                ("9".into(), host.id("Latin")),
                ("か".into(), host.id("Kana")),
            ]
        );
    }

    #[test]
    fn each_composed_face_is_asked_for_at_its_own_weight() {
        let mut host = Host::new();
        let subsets: FamilyComposer = Arc::new(|_: &Font| {
            let clamped = FaceDescriptors {
                weight: Some((FontWeight(300.0), FontWeight(900.0))),
                ..Default::default()
            };
            vec![
                ComposedFace {
                    family: "Latin".into(),
                    ranges: Arc::from([(0x0, 0xFF)]),
                    descriptors: clamped.clone(),
                },
                ComposedFace {
                    family: "Kana".into(),
                    ranges: Arc::from([(0x3040, 0x30FF)]),
                    descriptors: clamped.clone(),
                },
                ComposedFace {
                    family: "Digits".into(),
                    ranges: Arc::from([]),
                    descriptors: Default::default(),
                },
            ]
        });
        host.lists
            .set_composed_families(vec![("Web".into(), subsets)]);
        for (asked, drawn) in [(600.0, 600.0), (200.0, 300.0)] {
            host.asked.clear();
            let font = Font {
                family: "Web".into(),
                weight: FontWeight(asked),
                ..Default::default()
            };
            resolve_listed_font(&mut host, &font).unwrap();
            assert_eq!(
                host.asked,
                vec![
                    ("Latin".into(), FontWeight(drawn)),
                    ("Kana".into(), FontWeight(drawn)),
                    ("Digits".into(), FontWeight(asked)),
                ]
            );
        }
    }
}
