//! A bounded guard against added, lost or broadened common audio exclusions.
//!
//! This is not a general translation-equivalence checker. It recognizes common
//! Chinese/English ways of excluding music, human voices, drums and percussion;
//! unrecognized language remains the translator's responsibility. In particular,
//! a drum exclusion never licenses excluding all percussion or all music.

use std::collections::BTreeSet;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Category {
    Music,
    Voice,
    Drums,
    Percussion,
}

pub(super) fn preserves_exclusions(original: &str, translated: &str) -> bool {
    exclusions(original) == exclusions(translated)
}

fn exclusions(text: &str) -> BTreeSet<Category> {
    let mut text = text.to_lowercase();
    // Longer phrases precede their substrings. Keep music, voices, drums and
    // percussion distinct instead of guessing exclusions from the chosen model.
    for (source, target) in [
        ("背景音乐", " music "),
        ("背景音樂", " music "),
        ("打击乐", " percussion "),
        ("打擊樂", " percussion "),
        ("音乐", " music "),
        ("音樂", " music "),
        ("配乐", " music "),
        ("配樂", " music "),
        ("伴奏", " music "),
        ("人声", " voice "),
        ("人聲", " voice "),
        ("说话", " voice "),
        ("說話", " voice "),
        ("讲话", " voice "),
        ("講話", " voice "),
        ("歌声", " voice "),
        ("歌聲", " voice "),
        ("鼓点", " drums "),
        ("鼓點", " drums "),
        ("鼓声", " drums "),
        ("鼓聲", " drums "),
        ("不允许出现", " no "),
        ("不允许有", " no "),
        ("不允许", " no "),
        ("不要包含", " no "),
        ("不要加入", " no "),
        ("不要添加", " no "),
        ("不要出现", " no "),
        ("不要有", " no "),
        ("不应该有", " no "),
        ("不能有", " no "),
        ("不需要", " no "),
        ("不包含", " no "),
        ("不要", " no "),
        ("没有", " no "),
        ("沒有", " no "),
        ("不含", " no "),
        ("不带", " no "),
        ("不帶", " no "),
        ("不加", " no "),
        ("无需", " no "),
        ("無需", " no "),
        ("避免", " no "),
        ("禁止", " no "),
        ("排除", " no "),
        ("去掉", " no "),
        ("去除", " no "),
        ("不是", " not "),
        ("任何", " any "),
        ("以及", " and "),
        ("或者", " or "),
        ("和", " and "),
        ("及", " and "),
        ("或", " or "),
        ("free of", "without"),
        ("free from", "without"),
        ("music-free", "no music"),
        ("vocal-free", "no vocals"),
        ("speech-free", "no speech"),
        ("drum-free", "no drums"),
        ("percussion-free", "no percussion"),
    ] {
        text = text.replace(source, target);
    }
    // A lone 无 is only a negation directly before a recognized category.
    // This keeps descriptions such as 无缝循环的音乐 (seamless music) positive.
    for category in ["music", "voice", "drums", "percussion"] {
        for prefix in ["无", "無"] {
            text = text.replace(&format!("{prefix} {category}"), &format!(" no {category}"));
        }
    }
    let tokens: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphabetic() || c.is_whitespace() {
                c.to_string()
            } else {
                " | ".into()
            }
        })
        .collect();
    let mut result = BTreeSet::new();
    let mut negations = 0_u32;
    let mut inherited_negative = false;
    for token in tokens.split_whitespace() {
        let category = match token {
            "music" | "soundtrack" | "accompaniment" | "bgm" => Some(Category::Music),
            "voice" | "voices" | "vocal" | "vocals" | "speech" | "talking" | "speaking"
            | "singing" => Some(Category::Voice),
            "drum" | "drums" | "drumbeats" => Some(Category::Drums),
            "percussion" => Some(Category::Percussion),
            _ => None,
        };
        if let Some(category) = category {
            let negative = if negations == 0 {
                inherited_negative
            } else {
                negations % 2 == 1
            };
            if negative {
                result.insert(category);
            }
            inherited_negative = negative;
            negations = 0;
        } else if matches!(
            token,
            "no" | "not" | "without" | "avoid" | "exclude" | "omit" | "remove" | "lacking"
        ) {
            negations += 1;
        } else if !matches!(
            token,
            "and"
                | "or"
                | "nor"
                | "any"
                | "all"
                | "a"
                | "the"
                | "background"
                | "human"
                | "spoken"
                | "added"
                | "additional"
                | "extra"
                | "audible"
                | "beats"
        ) {
            // Punctuation, a positive clause, or an unrecognized sound ends
            // the exclusion. Never let 'no rain, with music' forbid music.
            negations = 0;
            inherited_negative = false;
        }
    }
    result
}
