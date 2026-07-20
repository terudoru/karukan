//! Tests for the learning cache and the Tab-skips-learning behavior.
//!
//! Space/Down: include learning candidates (default conversion).
//! Tab: skip learning candidates (lets users escape stale learned entries).

use std::io::Write;

use karukan_engine::{Dictionary, LearningCache};

use super::*;

/// Engine seeded with a learning entry `reading → surface`, no kanji model.
/// We bypass `init.rs` (which gates learning on settings + file I/O) and just
/// inject a populated `LearningCache` directly — these tests assert the
/// build_conversion_candidates branching, not the load path.
fn engine_with_learned(reading: &str, surface: &str) -> InputMethodEngine {
    let mut engine = InputMethodEngine::new();
    engine.converters.kanji = None;
    let mut cache = LearningCache::new(100);
    cache.record(reading, surface);
    engine.learning = Some(cache);
    engine
}

fn user_dict_with(reading: &str, surface: &str) -> Dictionary {
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    let json = format!(
        r#"[{{"reading":"{reading}","candidates":[{{"surface":"{surface}","score":1.0}}]}}]"#
    );
    tmp.write_all(json.as_bytes()).unwrap();
    tmp.flush().unwrap();
    Dictionary::build_from_json(tmp.path()).unwrap()
}

fn show_candidate_texts(result: &EngineResult) -> Vec<String> {
    result
        .actions
        .iter()
        .find_map(|a| match a {
            EngineAction::ShowCandidates(list) => Some(
                list.candidates()
                    .iter()
                    .map(|c| c.text.clone())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

#[test]
fn build_candidates_includes_learning_when_not_skipped() {
    let mut engine = engine_with_learned("あい", "藍");

    let texts: Vec<String> = engine
        .build_conversion_candidates("あい", 9, false)
        .into_iter()
        .map(|c| c.text)
        .collect();

    assert!(
        texts.contains(&"藍".to_string()),
        "Space path (skip_learning=false) should surface learned `藍`, got {:?}",
        texts,
    );
}

#[test]
fn build_candidates_omits_learning_when_skipped() {
    let mut engine = engine_with_learned("あい", "藍");

    let texts: Vec<String> = engine
        .build_conversion_candidates("あい", 9, true)
        .into_iter()
        .map(|c| c.text)
        .collect();

    assert!(
        !texts.contains(&"藍".to_string()),
        "Tab path (skip_learning=true) must drop learned `藍`, got {:?}",
        texts,
    );
}

#[test]
fn tab_key_skips_learning_in_composing() {
    // End-to-end: type the reading, press Tab → learned candidate is gone.
    let mut engine = engine_with_learned("あい", "藍");

    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    assert_eq!(engine.input_buf.text, "あい");

    let result = engine.process_key(&press_key(Keysym::TAB));
    assert!(result.consumed);
    assert!(matches!(engine.state(), InputState::Conversion { .. }));

    let texts: Vec<String> = engine
        .state()
        .candidates()
        .unwrap()
        .candidates()
        .iter()
        .map(|c| c.text.clone())
        .collect();
    assert!(
        !texts.contains(&"藍".to_string()),
        "Tab must skip the learned `藍` candidate, got {:?}",
        texts,
    );
}

#[test]
fn space_key_keeps_learning_in_composing() {
    // Counterpart to tab_key_skips_learning_in_composing: Space stays on the
    // learning-included path so the default UX is unchanged.
    let mut engine = engine_with_learned("あい", "藍");

    engine.process_key(&press('a'));
    engine.process_key(&press('i'));

    let result = engine.process_key(&press_key(Keysym::SPACE));
    assert!(result.consumed);
    assert!(matches!(engine.state(), InputState::Conversion { .. }));

    let texts: Vec<String> = engine
        .state()
        .candidates()
        .unwrap()
        .candidates()
        .iter()
        .map(|c| c.text.clone())
        .collect();
    assert!(
        texts.contains(&"藍".to_string()),
        "Space must surface learned `藍`, got {:?}",
        texts,
    );
}

#[test]
fn composing_does_not_show_candidates_before_space() {
    let mut engine = engine_with_learned("あい", "藍");
    engine.dicts.user = Some(user_dict_with("あい", "愛"));

    engine.process_key(&press('a'));
    let result = engine.process_key(&press('i'));

    assert!(
        show_candidate_texts(&result).is_empty(),
        "composing should not show conversion candidates before Space"
    );
}

#[test]
fn space_conversion_prioritizes_user_dictionary_over_learning() {
    let mut engine = engine_with_learned("あい", "藍");
    engine.dicts.user = Some(user_dict_with("あい", "愛"));

    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    let result = engine.process_key(&press_key(Keysym::SPACE));

    let texts = show_candidate_texts(&result);

    assert_eq!(
        texts.first().map(String::as_str),
        Some("愛"),
        "user dictionary candidate should be first after Space, got {:?}",
        texts
    );
    assert!(
        texts.iter().any(|t| t == "藍"),
        "learning candidate should remain after user dictionary, got {:?}",
        texts
    );
}

#[test]
fn space_conversion_keeps_short_learning_prefix_predictions() {
    let mut engine = engine_with_learned("あいしてる", "愛してる");

    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    let result = engine.process_key(&press_key(Keysym::SPACE));
    let texts = show_candidate_texts(&result);

    assert!(
        texts.iter().any(|t| t == "愛してる"),
        "short learned prefix prediction should remain, got {:?}",
        texts
    );
}

#[test]
fn space_conversion_omits_long_learning_prefix_predictions() {
    let mut engine = engine_with_learned(
        "きょうはとてもながいぶんしょうをへんかんしました",
        "今日はとても長い文章を変換しました",
    );

    engine.process_key(&press('k'));
    engine.process_key(&press('y'));
    let result = engine.process_key(&press_key(Keysym::SPACE));
    let texts = show_candidate_texts(&result);

    assert!(
        !texts
            .iter()
            .any(|t| t == "今日はとても長い文章を変換しました"),
        "long learned sentence should not appear in prefix auto-suggest, got {:?}",
        texts
    );
}
