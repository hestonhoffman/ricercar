//! Answers of `lyrics.get` and `item.details`: plain text and item lists,
//! checked and cut to size before the UI sees them.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Item, items_of};

/// Most synced lines (and plain lines) kept.
pub const MAX_LYRIC_LINES: usize = 5000;
const MAX_LYRIC_LINE: usize = 500;
/// Most timed words kept per synced line.
const MAX_LYRIC_WORDS: usize = 500;
const MAX_PLAIN: usize = 200_000;
/// Longest biography kept, in characters.
pub const MAX_BIOGRAPHY: usize = 20_000;
const MAX_SOURCE: usize = 200;
/// Most `related` shelves kept.
pub const MAX_SHELVES: usize = 10;
/// Most items kept per shelf.
pub const MAX_SHELF_ITEMS: usize = 50;
const MAX_SHELF_TITLE: usize = 120;
/// Most `facts` kept.
pub const MAX_FACTS: usize = 30;
const MAX_FACT_LABEL: usize = 80;
const MAX_FACT_VALUE: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricLine {
    pub time_ms: u64,
    pub text: String,
    /// Word timings, empty when the plugin gave none. Their texts joined
    /// are `text`: the spaces between words are added to the word before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<LyricWord>,
}

/// One word (or syllable) of a [`LyricLine`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricWord {
    pub time_ms: u64,
    pub text: String,
}

/// The `words` of a synced line, as pieces of `text`. Words are looked up
/// in order in the text, each taking the spaces after it (the first one,
/// the text before it); `None` when one is malformed or not found. A time
/// before the previous word's is moved up to it.
fn words_of(v: Option<&Value>, text: &str) -> Option<Vec<LyricWord>> {
    let raw = v?.as_array()?;
    let mut found: Vec<(u64, usize)> = Vec::new();
    let mut at = 0;
    for w in raw.iter().take(MAX_LYRIC_WORDS) {
        let time_ms = w.get("time_ms")?.as_u64()?;
        let time_ms = found.last().map_or(time_ms, |&(t, _)| time_ms.max(t));
        let word = clean(w.get("text")?.as_str()?, MAX_LYRIC_LINE, &[]);
        let word = word.trim();
        if word.is_empty() {
            continue;
        }
        let start = at + text[at..].find(word)?;
        found.push((time_ms, if found.is_empty() { 0 } else { start }));
        at = start + word.len();
    }
    // Each word runs to the next one's start, the last to the end.
    let ends = found.iter().skip(1).map(|&(_, s)| s).chain([text.len()]);
    let words: Vec<LyricWord> = found
        .iter()
        .zip(ends)
        .map(|(&(time_ms, start), end)| LyricWord {
            time_ms,
            text: text[start..end].to_string(),
        })
        .collect();
    (!words.is_empty()).then_some(words)
}

/// Answer to `lyrics.get`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginLyrics {
    /// Sorted by time; never empty when present.
    pub synced: Option<Vec<LyricLine>>,
    pub plain: Option<String>,
    pub instrumental: bool,
}

/// Up to `max` characters, without control characters (`keep` excepted).
fn clean(s: &str, max: usize, keep: &[char]) -> String {
    s.chars()
        .filter(|c| !c.is_control() || keep.contains(c))
        .take(max)
        .collect()
}

fn text_field(v: &Value, key: &str, max: usize) -> Option<String> {
    let s = clean(v.get(key)?.as_str()?.trim(), max, &[]);
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

impl PluginLyrics {
    /// `None` when the answer holds nothing usable (treated as not found).
    pub(crate) fn parse(v: &Value) -> Option<PluginLyrics> {
        let mut synced: Vec<LyricLine> = v
            .get("synced")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|l| {
                        let text = clean(l.get("text")?.as_str()?, MAX_LYRIC_LINE, &[])
                            .trim()
                            .to_string();
                        Some(LyricLine {
                            time_ms: l.get("time_ms")?.as_u64()?,
                            words: words_of(l.get("words"), &text).unwrap_or_default(),
                            text,
                        })
                    })
                    .take(MAX_LYRIC_LINES)
                    .collect()
            })
            .unwrap_or_default();
        // Stable: equal times keep the plugin's order.
        synced.sort_by_key(|l| l.time_ms);
        let plain = v
            .get("plain")
            .and_then(Value::as_str)
            .map(|p| {
                let p = p.replace("\r\n", "\n");
                p.lines()
                    .take(MAX_LYRIC_LINES)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .map(|p| clean(&p, MAX_PLAIN, &['\n', '\t']))
            .filter(|p| !p.trim().is_empty());
        let instrumental = v
            .get("instrumental")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let out = PluginLyrics {
            synced: (!synced.is_empty()).then_some(synced),
            plain,
            instrumental,
        };
        (out.synced.is_some() || out.plain.is_some() || out.instrumental).then_some(out)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Biography {
    /// Plain text.
    pub text: String,
    /// Where it comes from, shown under it.
    pub source: Option<String>,
}

/// A titled list of items ("Similar artists"…).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Shelf {
    pub title: String,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    pub label: String,
    pub value: String,
}

/// Answer to `item.details`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ItemDetails {
    pub biography: Option<Biography>,
    /// Shelves with at least one item.
    pub related: Vec<Shelf>,
    pub facts: Vec<Fact>,
}

impl ItemDetails {
    pub(crate) fn parse(v: &Value) -> ItemDetails {
        let biography = v.get("biography").and_then(|b| {
            let text = b.get("text")?.as_str()?;
            let text = clean(text.trim(), MAX_BIOGRAPHY, &['\n', '\t']);
            let text = text.trim_end().to_string();
            (!text.is_empty()).then(|| Biography {
                text,
                source: text_field(b, "source", MAX_SOURCE),
            })
        });
        let related = v
            .get("related")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|s| {
                        let title = text_field(s, "title", MAX_SHELF_TITLE)?;
                        let mut items = items_of(s.get("items"));
                        items.truncate(MAX_SHELF_ITEMS);
                        (!items.is_empty()).then_some(Shelf { title, items })
                    })
                    .take(MAX_SHELVES)
                    .collect()
            })
            .unwrap_or_default();
        let facts = v
            .get("facts")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|f| {
                        Some(Fact {
                            label: text_field(f, "label", MAX_FACT_LABEL)?,
                            value: text_field(f, "value", MAX_FACT_VALUE)?,
                        })
                    })
                    .take(MAX_FACTS)
                    .collect()
            })
            .unwrap_or_default();
        ItemDetails {
            biography,
            related,
            facts,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.biography.is_none() && self.related.is_empty() && self.facts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lyrics_are_sorted_and_capped() {
        let l = PluginLyrics::parse(&json!({
            "synced": [
                {"time_ms": 2000, "text": "b"},
                {"time_ms": 1000, "text": " a\u{7} "},
                {"time_ms": 2000, "text": "c"},
                {"time_ms": -5, "text": "bad"},
                {"text": "no time"}
            ],
            "plain": "a\r\nb\r\n"
        }))
        .unwrap();
        let s = l.synced.unwrap();
        assert_eq!(
            s.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(l.plain.as_deref(), Some("a\nb"));
        let many: Vec<Value> = (0..6000)
            .map(|i| json!({"time_ms": i, "text": "x".repeat(900)}))
            .collect();
        let l = PluginLyrics::parse(&json!({ "synced": many })).unwrap();
        let s = l.synced.unwrap();
        assert_eq!(s.len(), MAX_LYRIC_LINES);
        assert_eq!(s[0].text.len(), 500);
        assert!(PluginLyrics::parse(&json!({"synced": [], "plain": "  "})).is_none());
        assert!(
            PluginLyrics::parse(&json!({"instrumental": true}))
                .unwrap()
                .instrumental
        );
    }

    fn words(l: &LyricLine) -> Vec<(u64, &str)> {
        l.words
            .iter()
            .map(|w| (w.time_ms, w.text.as_str()))
            .collect()
    }

    #[test]
    fn lyric_words() {
        let l = PluginLyrics::parse(&json!({
            "synced": [
                // Without spaces: they go to the word before.
                {"time_ms": 1000, "text": "Some words here", "words": [
                    {"time_ms": 1000, "text": "Some"},
                    {"time_ms": 1400, "text": "words"},
                    {"time_ms": 1900, "text": "here"}
                ]},
                // With them, and text before the first word.
                {"time_ms": 3000, "text": "(oh) beautiful!", "words": [
                    {"time_ms": 3100, "text": "beau"},
                    {"time_ms": 3200, "text": "ti"},
                    {"time_ms": 3150, "text": "ful "}
                ]},
                {"time_ms": 2000, "text": "no words"},
                {"time_ms": 4000, "text": "not found", "words": [{"time_ms": 4000, "text": "lost"}]},
                {"time_ms": 5000, "text": "bad", "words": [{"time_ms": "x", "text": "bad"}]},
                {"time_ms": 6000, "text": "odd", "words": "odd"}
            ]
        }))
        .unwrap();
        let s = l.synced.unwrap();
        assert_eq!(
            words(&s[0]),
            [(1000, "Some "), (1400, "words "), (1900, "here")]
        );
        assert_eq!(s[1].text, "no words");
        assert!(s[1].words.is_empty());
        assert_eq!(
            words(&s[2]),
            [(3100, "(oh) beau"), (3200, "ti"), (3200, "ful!")]
        );
        for l in &s[3..] {
            assert!(l.words.is_empty(), "{}", l.text);
        }
        for l in &s {
            if !l.words.is_empty() {
                let joined: String = l.words.iter().map(|w| w.text.as_str()).collect();
                assert_eq!(joined, l.text);
            }
        }
    }

    #[test]
    fn lyric_words_serde() {
        // Lines without words serialise as before.
        let l = LyricLine {
            time_ms: 5,
            text: "a".into(),
            words: Vec::new(),
        };
        let json = serde_json::to_string(&l).unwrap();
        assert_eq!(json, r#"{"time_ms":5,"text":"a"}"#);
        assert_eq!(serde_json::from_str::<LyricLine>(&json).unwrap(), l);
        let l = LyricLine {
            words: vec![LyricWord {
                time_ms: 5,
                text: "a".into(),
            }],
            ..l
        };
        let json = serde_json::to_string(&l).unwrap();
        assert_eq!(serde_json::from_str::<LyricLine>(&json).unwrap(), l);
    }

    #[test]
    fn details_are_checked() {
        let items: Vec<Value> = (0..80)
            .map(|i| json!({"ref": format!("album/{i}"), "kind": "album", "title": "A"}))
            .collect();
        let shelves: Vec<Value> = (0..12)
            .map(|i| json!({"title": format!("S{i}"), "items": items}))
            .collect();
        let facts: Vec<Value> = (0..40)
            .map(|i| json!({"label": "L", "value": format!("{i}")}))
            .collect();
        let mut related = shelves;
        related.insert(0, json!({"title": "empty", "items": []}));
        related.insert(0, json!({"items": items}));
        let d = ItemDetails::parse(&json!({
            "biography": {"text": "x".repeat(30_000), "source": "somewhere"},
            "related": related,
            "facts": facts,
        }));
        let b = d.biography.unwrap();
        assert_eq!(b.text.chars().count(), MAX_BIOGRAPHY);
        assert_eq!(b.source.as_deref(), Some("somewhere"));
        assert_eq!(d.related.len(), MAX_SHELVES);
        assert_eq!(d.related[0].title, "S0");
        assert_eq!(d.related[0].items.len(), MAX_SHELF_ITEMS);
        assert_eq!(d.facts.len(), MAX_FACTS);
        assert!(ItemDetails::parse(&json!({"biography": {"text": " "}})).is_empty());
    }
}
