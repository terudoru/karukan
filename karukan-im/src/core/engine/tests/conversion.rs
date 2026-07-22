use super::*;
use crate::core::state::ConversionSegment;
use karukan_engine::Dictionary;
use std::io::Write;

fn user_dict_with(reading: &str, surface: &str) -> Dictionary {
    user_dict_with_entries(&[(reading, surface)])
}

fn user_dict_with_entries(entries: &[(&str, &str)]) -> Dictionary {
    dict_with_scored_entries(
        &entries
            .iter()
            .map(|(reading, surface)| (*reading, *surface, 1.0))
            .collect::<Vec<_>>(),
    )
}

fn dict_with_scored_entries(entries: &[(&str, &str, f32)]) -> Dictionary {
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    let entries = entries
        .iter()
        .map(|(reading, surface, score)| {
            format!(
                r#"{{"reading":"{reading}","candidates":[{{"surface":"{surface}","score":{score}}}]}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let json = format!("[{entries}]");
    tmp.write_all(json.as_bytes()).unwrap();
    tmp.flush().unwrap();
    Dictionary::build_from_json(tmp.path()).unwrap()
}

fn visible_preedit_text(engine: &InputMethodEngine) -> String {
    engine.preedit().unwrap().text().replace('\u{200B}', "")
}

fn navigate_active_candidate_to(engine: &mut InputMethodEngine, expected: &str) {
    let (cursor, target, len) = match engine.state() {
        InputState::Conversion { candidates, .. } => (
            candidates.cursor(),
            candidates
                .candidates()
                .iter()
                .position(|candidate| candidate.text == expected)
                .unwrap_or_else(|| panic!("candidate {expected:?} not found")),
            candidates.len(),
        ),
        _ => panic!("expected conversion state"),
    };
    for _ in 0..(target + len - cursor) % len {
        engine.process_key(&press_key(Keysym::SPACE));
    }
}

#[test]
fn test_conversion_char_commits_and_continues() {
    let mut engine = InputMethodEngine::new();

    // Type "あい" and enter conversion
    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    engine.process_key(&press_key(Keysym::SPACE));
    assert!(matches!(engine.state(), InputState::Conversion { .. }));

    // Type 'k' during conversion → should commit candidate and start new input
    let result = engine.process_key(&press('k'));
    assert!(result.consumed);

    // Should have committed the conversion
    let has_commit = result
        .actions
        .iter()
        .any(|a| matches!(a, EngineAction::Commit(_)));
    assert!(has_commit, "Should have a commit action");

    // Should now be in Composing with 'k' in preedit
    assert!(matches!(engine.state(), InputState::Composing { .. }));
    assert_eq!(engine.preedit().unwrap().text(), "k");
}

#[test]
fn test_conversion_char_commits_and_continues_romaji() {
    let mut engine = InputMethodEngine::new();

    // Type "あ" and enter conversion
    engine.process_key(&press('a'));
    engine.process_key(&press_key(Keysym::SPACE));
    assert!(matches!(engine.state(), InputState::Conversion { .. }));

    // Type 'k', 'a' → commits conversion, then starts "か"
    engine.process_key(&press('k'));
    assert!(matches!(engine.state(), InputState::Composing { .. }));
    assert_eq!(engine.preedit().unwrap().text(), "k");

    engine.process_key(&press('a'));
    assert_eq!(engine.preedit().unwrap().text(), "か");
}

#[test]
fn test_alphabet_mode_space_inserts_literal_space() {
    let mut engine = InputMethodEngine::new();

    // Enter alphabet mode via Shift+N
    engine.process_key(&press_shift('N'));
    assert!(engine.mode.current() == InputMode::Alphabet);

    // Type "ew"
    engine.process_key(&press('e'));
    engine.process_key(&press('w'));
    assert_eq!(engine.preedit().unwrap().text(), "New");

    // Space → should insert literal space, NOT start conversion
    engine.process_key(&press_key(Keysym::SPACE));
    assert!(matches!(engine.state(), InputState::Composing { .. }));
    assert_eq!(engine.preedit().unwrap().text(), "New ");

    // Type "york"
    engine.process_key(&press('y'));
    engine.process_key(&press('o'));
    engine.process_key(&press('r'));
    engine.process_key(&press('k'));
    assert_eq!(engine.preedit().unwrap().text(), "New york");
}

#[test]
fn test_conversion_can_select_candidates_per_segment() {
    let mut engine = InputMethodEngine::with_config(EngineConfig {
        composing_chunk_len: 2,
        ..EngineConfig::default()
    });

    for ch in ['a', 'i', 'u', 'e'] {
        engine.process_key(&press(ch));
    }
    engine.process_key(&press_key(Keysym::SPACE));

    let InputState::Conversion {
        segments,
        active_segment,
        ..
    } = engine.state()
    else {
        panic!("expected conversion state");
    };
    assert_eq!(*active_segment, 0);
    assert!(!segments[0].needs_expansion);
    assert!(segments[1..].iter().all(|segment| segment.needs_expansion));
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].reading, "あい");
    assert_eq!(segments[1].reading, "うえ");
    let initial_second_surface = segments[1]
        .candidates
        .selected_text()
        .unwrap_or(&segments[1].reading)
        .to_string();

    // Conversion starts with the first clause focused, matching macOS.
    // Select its katakana fallback by value; candidate order is model-dependent.
    navigate_active_candidate_to(&mut engine, "アイ");
    assert_eq!(
        visible_preedit_text(&engine),
        format!("アイ{initial_second_surface}")
    );
    assert_eq!(
        engine
            .preedit()
            .unwrap()
            .attributes()
            .iter()
            .filter(|a| a.attr_type == AttributeType::Highlight)
            .map(|a| (a.start, a.end))
            .collect::<Vec<_>>(),
        vec![(0, 2)]
    );

    // Move right to the already-present second clause. The first clause must
    // keep its selected surface.
    engine.process_key(&press_key(Keysym::RIGHT));
    let InputState::Conversion {
        segments,
        active_segment,
        ..
    } = engine.state()
    else {
        panic!("expected conversion state");
    };
    assert_eq!(*active_segment, 1);
    assert!(!segments[1].needs_expansion);
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].reading, "あい");
    assert_eq!(segments[1].reading, "うえ");
    assert_eq!(
        visible_preedit_text(&engine),
        format!("アイ{initial_second_surface}")
    );
    assert_eq!(
        engine
            .preedit()
            .unwrap()
            .attributes()
            .iter()
            .filter(|a| a.attr_type == AttributeType::Highlight)
            .map(|a| (a.start, a.end))
            .collect::<Vec<_>>(),
        vec![(3, 3 + initial_second_surface.chars().count())]
    );

    navigate_active_candidate_to(&mut engine, "ウエ");
    assert_eq!(visible_preedit_text(&engine), "アイウエ");

    let result = engine.process_key(&press_key(Keysym::RETURN));
    assert!(
        result
            .actions
            .iter()
            .any(|a| { matches!(a, EngineAction::Commit(text) if text == "アイウエ") })
    );
}

#[test]
fn test_explicit_conversion_splits_short_sentence_at_particles() {
    let mut engine = InputMethodEngine::new();
    engine.dicts.system = Some(user_dict_with_entries(&[
        ("きょう", "今日"),
        ("いい", "良い"),
        ("てんき", "天気"),
    ]));
    engine.input_buf.insert("きょうはいいてんき");

    engine.start_conversion(false);

    let InputState::Conversion {
        segments,
        active_segment,
        ..
    } = engine.state()
    else {
        panic!("expected conversion state");
    };
    assert_eq!(*active_segment, 0);
    assert_eq!(
        segments
            .iter()
            .map(|segment| segment.reading.as_str())
            .collect::<Vec<_>>(),
        vec!["きょう", "は", "いい", "てんき"]
    );
    assert_eq!(
        engine
            .preedit()
            .unwrap()
            .attributes()
            .iter()
            .filter(|attribute| attribute.attr_type == AttributeType::Highlight)
            .map(|attribute| (attribute.start, attribute.end))
            .collect::<Vec<_>>(),
        vec![(0, 2)],
        "conversion must begin with the first clause focused"
    );

    engine.process_key(&press_key(Keysym::RIGHT));

    let InputState::Conversion {
        segments,
        active_segment,
        ..
    } = engine.state()
    else {
        panic!("expected conversion state");
    };
    assert_eq!(*active_segment, 1);
    assert_eq!(
        segments
            .iter()
            .map(|s| s.reading.as_str())
            .collect::<Vec<_>>(),
        vec!["きょう", "は", "いい", "てんき"]
    );
    assert_eq!(
        engine
            .preedit()
            .unwrap()
            .attributes()
            .iter()
            .filter(|a| a.attr_type == AttributeType::Highlight)
            .map(|a| (a.start, a.end))
            .collect::<Vec<_>>(),
        vec![(3, 4)],
        "the particle highlight must start after 今日, not after a proportional surface split"
    );
}

#[test]
fn test_segment_navigation_aligns_word_boundaries_to_converted_surface() {
    let mut engine = InputMethodEngine::new();
    engine.dicts.system = Some(dict_with_scored_entries(&[
        ("きょう", "今日", 10.0),
        // These longer prefixes are valid standalone dictionary entries but
        // do not describe the displayed sentence.
        ("きょうは", "教派", 1.0),
        ("きょうはい", "向背", 1.0),
        ("は", "は", 10.0),
        ("はい", "はい", 20.0),
        ("いい", "いい", 10.0),
        ("てんき", "天気", 10.0),
    ]));
    engine.input_buf.insert("きょうはいいてんき");
    engine.live.text = "今日はいい天気".to_string();

    engine.start_conversion(false);
    engine.process_key(&press_key(Keysym::RIGHT));

    let InputState::Conversion {
        segments,
        active_segment,
        ..
    } = engine.state()
    else {
        panic!("expected conversion state");
    };
    assert_eq!(*active_segment, 1);
    assert_eq!(
        segments
            .iter()
            .map(|segment| segment.reading.as_str())
            .collect::<Vec<_>>(),
        vec!["きょう", "は", "いい", "てんき"]
    );
    assert_eq!(visible_preedit_text(&engine), "今日はいい天気");
    assert_eq!(
        engine
            .preedit()
            .unwrap()
            .attributes()
            .iter()
            .filter(|attribute| attribute.attr_type == AttributeType::Highlight)
            .map(|attribute| (attribute.start, attribute.end))
            .collect::<Vec<_>>(),
        vec![(3, 4)],
        "the active particle must highlight only は"
    );
}

#[test]
fn test_explicit_conversion_keeps_particles_separate_from_nouns() {
    let mut engine = InputMethodEngine::new();
    engine.dicts.system = Some(user_dict_with_entries(&[
        ("だいがく", "大学"),
        ("じゅぎょう", "授業"),
        ("うける", "受ける"),
    ]));
    engine.input_buf.insert("だいがくのじゅぎょうをうける");

    engine.start_conversion(false);
    engine.process_key(&press_key(Keysym::RIGHT));

    let InputState::Conversion { segments, .. } = engine.state() else {
        panic!("expected conversion state");
    };
    assert_eq!(
        segments
            .iter()
            .map(|s| s.reading.as_str())
            .collect::<Vec<_>>(),
        vec!["だいがく", "の", "じゅぎょう", "を", "うける"]
    );
}

#[test]
fn test_explicit_conversion_does_not_split_inside_dictionary_words() {
    let mut engine = InputMethodEngine::new();
    engine.dicts.system = Some(user_dict_with_entries(&[
        ("どうさ", "動作"),
        ("かくにん", "確認"),
        ("かねて", "兼ねて"),
        ("ためし", "試し"),
        ("にゅうりょく", "入力"),
    ]));
    engine
        .input_buf
        .insert("どうさかくにんをかねてためしににゅうりょくしてみましょう");

    engine.start_conversion(false);
    engine.process_key(&press_key(Keysym::RIGHT));

    let InputState::Conversion { segments, .. } = engine.state() else {
        panic!("expected conversion state");
    };
    assert_eq!(
        segments
            .iter()
            .map(|s| s.reading.as_str())
            .collect::<Vec<_>>(),
        vec![
            "どうさ",
            "かくにん",
            "を",
            "かねて",
            "ためし",
            "に",
            "にゅうりょく",
            "してみましょう"
        ]
    );
}

#[test]
fn test_segment_navigation_preserves_user_dictionary_span_without_duplication() {
    let mut engine = InputMethodEngine::new();
    engine.dicts.user = Some(user_dict_with("あい", "愛"));
    engine.input_buf.insert("あいうえ");

    // Supply the surface that was already displayed by live conversion. This
    // isolates span preservation from the real model's whole-reading guess.
    engine.live.text = "愛うえ".to_string();
    engine.start_conversion(false);
    engine.process_key(&press_key(Keysym::RIGHT));

    let InputState::Conversion {
        segments,
        active_segment,
        ..
    } = engine.state()
    else {
        panic!("expected conversion state");
    };
    assert_eq!(*active_segment, 1);
    assert_eq!(
        segments
            .iter()
            .map(|s| s.reading.as_str())
            .collect::<Vec<_>>(),
        vec!["あい", "うえ"]
    );
    assert_eq!(
        segments[1].candidates.selected_text(),
        Some("うえ"),
        "the free span should keep the displayed surface while entering segment navigation; candidates={:?}",
        segments[1]
            .candidates
            .candidates()
            .iter()
            .map(|candidate| candidate.text.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(visible_preedit_text(&engine), "愛うえ");
}

#[test]
fn commit_and_continue_advances_surrounding_context() {
    let mut engine = InputMethodEngine::new();
    engine.config.max_api_context_len = 20;
    engine.set_surrounding_context("前文", "右側");
    engine.input_buf.insert("あい");

    let candidates = CandidateList::from_strings_with_reading(["愛"], "あい");
    let segments = vec![ConversionSegment {
        reading: "あい".to_string(),
        candidates: candidates.clone(),
        needs_expansion: false,
    }];
    engine.state = InputState::Conversion {
        preedit: Preedit::with_text("愛"),
        candidates,
        segments,
        active_segment: 0,
        skip_learning: false,
    };

    let result = engine.process_key(&press('k'));

    assert!(
        result
            .actions
            .iter()
            .any(|action| matches!(action, EngineAction::Commit(text) if text == "愛"))
    );
    let context = engine.surrounding_context.as_ref().unwrap();
    assert_eq!(context.left.as_deref(), Some("前文愛"));
    assert_eq!(context.right.as_deref(), Some("右側"));
}

#[test]
fn test_segment_navigation_preserves_existing_prediction_surface() {
    let mut engine = InputMethodEngine::new();
    engine.input_buf.insert("きょうはいいてんき");
    engine.live.text = "今日はいい天気".to_string();

    engine.start_conversion(false);
    engine.process_key(&press_key(Keysym::RIGHT));

    let InputState::Conversion {
        segments,
        active_segment,
        ..
    } = engine.state()
    else {
        panic!("expected conversion state");
    };
    assert_eq!(*active_segment, 1);
    assert!(segments.len() > 1);
    assert_eq!(visible_preedit_text(&engine), "今日はいい天気");

    engine.process_key(&press_key(Keysym::RIGHT));
    assert_eq!(visible_preedit_text(&engine), "今日はいい天気");
}

#[test]
fn mac_candidate_shortcuts_move_forward_and_backward() {
    let mut engine = InputMethodEngine::new();
    engine.input_buf.insert("あい");
    engine.start_conversion(false);

    let initial = engine.candidates().unwrap().cursor();
    engine.process_key(&press_ctrl(Keysym::KEY_N));
    assert_ne!(engine.candidates().unwrap().cursor(), initial);

    engine.process_key(&press_ctrl(Keysym::KEY_P));
    assert_eq!(engine.candidates().unwrap().cursor(), initial);

    engine.process_key(&press_key(Keysym::SPACE));
    assert_ne!(engine.candidates().unwrap().cursor(), initial);
    engine.process_key(&press_shift_key(Keysym::SPACE));
    assert_eq!(engine.candidates().unwrap().cursor(), initial);
}

#[test]
fn mac_clause_shortcuts_select_and_resize_without_losing_reading() {
    let mut engine = InputMethodEngine::with_config(EngineConfig {
        composing_chunk_len: 2,
        ..EngineConfig::default()
    });
    engine.input_buf.insert("あいうえ");
    engine.start_conversion(false);

    // Control+B selects the next clause and materializes the word-sized
    // navigation segments; Control+F returns to the previous clause.
    engine.process_key(&press_ctrl(Keysym::KEY_B));
    engine.process_key(&press_ctrl(Keysym::KEY_F));
    let InputState::Conversion {
        segments,
        active_segment,
        ..
    } = engine.state()
    else {
        panic!("expected conversion state");
    };
    assert_eq!(*active_segment, 0);
    assert_eq!(
        segments
            .iter()
            .map(|segment| segment.reading.as_str())
            .collect::<Vec<_>>(),
        vec!["あい", "うえ"]
    );

    // Shift+Right grows the active clause by one character. Control+I is
    // the standard shortcut for shrinking it back.
    engine.process_key(&press_shift_key(Keysym::RIGHT));
    let InputState::Conversion { segments, .. } = engine.state() else {
        panic!("expected conversion state");
    };
    assert_eq!(
        segments
            .iter()
            .map(|segment| segment.reading.as_str())
            .collect::<Vec<_>>(),
        vec!["あいう", "え"]
    );
    assert_eq!(
        segments
            .iter()
            .map(|segment| segment.reading.as_str())
            .collect::<String>(),
        "あいうえ"
    );

    engine.process_key(&press_ctrl(Keysym::KEY_I));
    let InputState::Conversion { segments, .. } = engine.state() else {
        panic!("expected conversion state");
    };
    assert_eq!(
        segments
            .iter()
            .map(|segment| segment.reading.as_str())
            .collect::<Vec<_>>(),
        vec!["あい", "うえ"]
    );
}

#[test]
fn mac_control_z_returns_conversion_to_reading() {
    let mut engine = InputMethodEngine::new();
    engine.input_buf.insert("あいう");
    engine.start_conversion(false);
    assert!(matches!(engine.state(), InputState::Conversion { .. }));

    let result = engine.process_key(&press_ctrl(Keysym::KEY_Z));
    assert!(result.consumed);
    assert!(matches!(engine.state(), InputState::Composing { .. }));
    assert_eq!(engine.preedit().unwrap().text(), "あいう");
    assert!(
        result
            .actions
            .iter()
            .any(|action| matches!(action, EngineAction::HideCandidates))
    );
}

#[test]
fn shrinking_whole_conversion_creates_a_following_clause() {
    let mut engine = InputMethodEngine::new();
    engine.input_buf.insert("あいう");
    engine.start_conversion(false);

    engine.process_key(&press_shift_key(Keysym::LEFT));
    let InputState::Conversion { segments, .. } = engine.state() else {
        panic!("expected conversion state");
    };
    assert_eq!(
        segments
            .iter()
            .map(|segment| segment.reading.as_str())
            .collect::<Vec<_>>(),
        vec!["あい", "う"]
    );

    engine.process_key(&press_ctrl(Keysym::KEY_W));
    let InputState::Conversion { segments, .. } = engine.state() else {
        panic!("expected conversion state");
    };
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].reading, "あいう");
}
