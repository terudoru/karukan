//! Conversion state handling (candidates, commit). The live-conversion
//! chunking lives in the sibling `chunk` module.

use std::collections::HashSet;
use std::time::Instant;

use tracing::debug;

use super::*;
use crate::core::state::ConversionSegment;

/// Maximum number of learning candidates to show
const MAX_LEARNING_CANDIDATES: usize = 3;
/// Do not surface long learned sentences through prefix prediction. Exact
/// matches are still allowed; this only prevents a past long conversion from
/// dominating suggestions after typing its first few kana.
const MAX_LEARNING_PREFIX_READING_CHARS: usize = 12;
const MAX_LEARNING_PREFIX_SURFACE_CHARS: usize = 24;
/// Explicit conversion segments should be short enough that Left/Right can
/// choose a useful clause even when live-conversion chunking uses a larger
/// latency-oriented chunk size.
const MAX_EXPLICIT_SEGMENT_CHARS: usize = 8;
/// Bound the dynamic-programming matrix for unusually long preedits. Normal
/// live-conversion chunks are at most 40 characters (about 1,600 cells).
const MAX_SURFACE_ALIGNMENT_CELLS: usize = 16_384;
const SEGMENT_DISPLAY_SEPARATOR: &str = "\u{200B}";
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReadingSpan {
    pub reading: String,
    pub fixed_surface: Option<String>,
}

#[derive(Clone, Default)]
struct SurfaceAlignment {
    unmatched_reading_chars: usize,
    user_reading_chars: usize,
    dictionary_score: f32,
    spans: Vec<ReadingSpan>,
}

impl SurfaceAlignment {
    fn is_better_than(&self, other: &Self) -> bool {
        // A displayed user-dictionary surface remains a hard anchor. Among
        // paths with equal user coverage, prefer dictionary-backed coverage,
        // then the lower dictionary cost and finally fewer segments.
        self.user_reading_chars > other.user_reading_chars
            || (self.user_reading_chars == other.user_reading_chars
                && (self.unmatched_reading_chars < other.unmatched_reading_chars
                    || (self.unmatched_reading_chars == other.unmatched_reading_chars
                        && (self
                            .dictionary_score
                            .total_cmp(&other.dictionary_score)
                            .is_lt()
                            || (self
                                .dictionary_score
                                .total_cmp(&other.dictionary_score)
                                .is_eq()
                                && self.spans.len() < other.spans.len())))))
    }

    fn push_literal(&mut self, ch: char) {
        self.unmatched_reading_chars += 1;
        if let Some(last) = self.spans.last_mut()
            && last.fixed_surface.is_none()
        {
            last.reading.push(ch);
            return;
        }
        self.spans.push(ReadingSpan {
            reading: ch.to_string(),
            fixed_surface: None,
        });
    }

    fn push_dictionary_match(&mut self, reading: &str, surface: &str, score: f32, is_user: bool) {
        let reading_len = reading.chars().count();
        if is_user {
            self.user_reading_chars += reading_len;
        }
        self.dictionary_score += score;
        self.spans.push(ReadingSpan {
            reading: reading.to_string(),
            fixed_surface: Some(surface.to_string()),
        });
    }
}

fn keep_better_alignment(slot: &mut Option<SurfaceAlignment>, candidate: SurfaceAlignment) {
    if slot
        .as_ref()
        .is_none_or(|existing| candidate.is_better_than(existing))
    {
        *slot = Some(candidate);
    }
}

struct AlignmentPosition<'a> {
    reading_suffix: &'a str,
    reading_index: usize,
    surface_index: usize,
    surface_chars: &'a [char],
}

fn extend_dictionary_alignments(
    paths: &mut [Vec<Option<SurfaceAlignment>>],
    path: &SurfaceAlignment,
    dictionary: &karukan_engine::Dictionary,
    position: &AlignmentPosition<'_>,
    max_reading_len: usize,
    is_user: bool,
) {
    for entry in dictionary.common_prefix_search(position.reading_suffix) {
        let reading_len = entry.reading.chars().count();
        if reading_len == 0 || reading_len > max_reading_len {
            continue;
        }
        for candidate in entry.candidates {
            let candidate_chars: Vec<char> = candidate.surface.chars().collect();
            if candidate_chars.is_empty()
                || !position.surface_chars[position.surface_index..].starts_with(&candidate_chars)
            {
                continue;
            }

            let mut next = path.clone();
            next.push_dictionary_match(entry.reading, &candidate.surface, candidate.score, is_user);
            keep_better_alignment(
                &mut paths[position.reading_index + reading_len]
                    [position.surface_index + candidate_chars.len()],
                next,
            );
        }
    }
}

/// Mozc-style width/script annotation for a pure-kana candidate, or `None`
/// if the text mixes scripts or contains kanji/punctuation. Used to label
/// `あ` / `ア` / `ｱ` candidates in the conversion list.
fn width_annotation(text: &str) -> Option<&'static str> {
    if karukan_engine::is_pure_hiragana(text) {
        Some("[全]ひらがな")
    } else if karukan_engine::is_pure_full_katakana(text) {
        Some("[全]カタカナ")
    } else {
        None
    }
}

fn is_hiragana_char(c: char) -> bool {
    matches!(c, '\u{3040}'..='\u{309F}')
}

fn is_full_katakana_char(c: char) -> bool {
    matches!(c, '\u{30A0}'..='\u{30FF}') && c != '\u{30FB}'
}

fn is_kanji_char(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{9FFF}')
}

fn has_multiple_katakana_runs(text: &str) -> bool {
    let mut runs = 0usize;
    let mut in_run = false;
    for ch in text.chars() {
        if is_full_katakana_char(ch) {
            if !in_run {
                runs += 1;
                if runs >= 2 {
                    return true;
                }
            }
            in_run = true;
        } else {
            in_run = false;
        }
    }
    false
}

fn suspicious_auto_conversion(reading: &str, converted: &str) -> bool {
    if converted.is_empty() {
        return true;
    }

    let reading_is_kana = reading
        .chars()
        .all(|ch| is_hiragana_char(ch) || is_full_katakana_char(ch));
    if !reading_is_kana {
        return false;
    }

    // A neural guess must never silently turn an all-hiragana reading into
    // Latin text during live conversion. Explicit conversion still exposes
    // the model candidate, and user-dictionary replacements bypass this
    // heuristic, so legitimate registrations such as `えーあい -> AI` keep
    // working without allowing hallucinations such as `あい -> I` to become
    // the marked text and then be committed by Enter.
    if converted.chars().any(|ch| ch.is_ascii_alphabetic()) {
        return true;
    }

    let mut total = 0usize;
    let mut katakana = 0usize;
    let mut kanji = 0usize;
    for ch in converted.chars() {
        total += 1;
        if is_full_katakana_char(ch) {
            katakana += 1;
        } else if is_kanji_char(ch) {
            kanji += 1;
        }
    }

    // A normal loanword such as `プログラム所属` has one katakana run. The
    // failure mode we want to suppress is a long sentence broken into several
    // katakana fragments around kanji, e.g. `ニイガタ第ガクコウ学部...`.
    kanji > 0 && has_multiple_katakana_runs(converted) && katakana * 3 >= total
}

fn split_surface_by_reading_lengths(surface: &str, spans: &[ReadingSpan]) -> Vec<Option<String>> {
    let mut surfaces = Vec::with_capacity(spans.len());
    let chars: Vec<char> = surface.chars().collect();
    if chars.is_empty() || spans.is_empty() {
        return vec![None; spans.len()];
    }

    let total_reading_len: usize = spans.iter().map(|span| span.reading.chars().count()).sum();
    if total_reading_len == 0 {
        return vec![None; spans.len()];
    }

    let mut start = 0usize;
    let mut consumed_reading_len = 0usize;
    for (idx, span) in spans.iter().enumerate() {
        consumed_reading_len += span.reading.chars().count();
        let remaining_segments = spans.len() - idx - 1;
        let proportional_end = if idx + 1 == spans.len() {
            chars.len()
        } else {
            (consumed_reading_len * chars.len()).div_ceil(total_reading_len)
        };
        let min_end = start + usize::from(chars.len() - start > remaining_segments);
        let max_end = chars.len().saturating_sub(remaining_segments);
        let end = proportional_end.clamp(min_end, max_end);
        surfaces.push((start < end).then(|| chars[start..end].iter().collect()));
        start = end;
    }
    surfaces
}

/// Split an already displayed surface while keeping user-dictionary spans as
/// exact anchors. A proportional split alone can consume part of the following
/// free span (`愛うえ` + readings `あい`/`うえ` became `愛う`/`え`).
fn split_surface_preserving_fixed_spans(
    surface: &str,
    spans: &[ReadingSpan],
) -> Vec<Option<String>> {
    let surface_chars: Vec<char> = surface.chars().collect();
    let mut result = vec![None; spans.len()];
    let mut surface_start = 0usize;
    let mut span_start = 0usize;

    for (idx, span) in spans.iter().enumerate() {
        let Some(fixed) = span.fixed_surface.as_deref() else {
            continue;
        };
        let fixed_chars: Vec<char> = fixed.chars().collect();
        if fixed_chars.is_empty() {
            return split_surface_by_reading_lengths(surface, spans);
        }
        let Some(relative_start) = surface_chars[surface_start..]
            .windows(fixed_chars.len())
            .position(|window| window == fixed_chars.as_slice())
        else {
            // The displayed model surface does not contain the registered
            // value, so there is no trustworthy anchor. Preserve the old
            // whole-surface behavior instead of dropping characters.
            return split_surface_by_reading_lengths(surface, spans);
        };
        let fixed_start = surface_start + relative_start;

        if span_start < idx {
            let free_surface: String = surface_chars[surface_start..fixed_start].iter().collect();
            for (offset, value) in
                split_surface_by_reading_lengths(&free_surface, &spans[span_start..idx])
                    .into_iter()
                    .enumerate()
            {
                result[span_start + offset] = value;
            }
        }

        result[idx] = Some(fixed.to_string());
        surface_start = fixed_start + fixed_chars.len();
        span_start = idx + 1;
    }

    if span_start < spans.len() {
        let tail: String = surface_chars[surface_start..].iter().collect();
        for (offset, value) in split_surface_by_reading_lengths(&tail, &spans[span_start..])
            .into_iter()
            .enumerate()
        {
            result[span_start + offset] = value;
        }
    }

    result
}

fn merge_reading_spans(left: ReadingSpan, right: ReadingSpan) -> ReadingSpan {
    let fixed_surface = match (left.fixed_surface, right.fixed_surface) {
        (Some(left), Some(right)) => Some(format!("{left}{right}")),
        _ => None,
    };
    ReadingSpan {
        reading: format!("{}{}", left.reading, right.reading),
        fixed_surface,
    }
}

fn coalesce_spans_to_surface_len(mut spans: Vec<ReadingSpan>, surface: &str) -> Vec<ReadingSpan> {
    let limit = surface.chars().count();
    if limit == 0 || spans.len() <= limit {
        return spans;
    }

    while spans.len() > limit {
        let merge_at = spans
            .windows(2)
            .enumerate()
            .filter(|(_, pair)| pair[0].fixed_surface.is_none() && pair[1].fixed_surface.is_none())
            .min_by_key(|(_, pair)| {
                pair[0].reading.chars().count() + pair[1].reading.chars().count()
            })
            .map(|(idx, _)| idx)
            .unwrap_or_else(|| spans.len().saturating_sub(2));
        let right = spans.remove(merge_at + 1);
        let left = spans.remove(merge_at);
        spans.insert(merge_at, merge_reading_spans(left, right));
    }
    spans
}

/// Helper for building a deduplicated list of conversion candidates.
///
/// Candidate text is used as the deduplication key; earlier sources win.
struct CandidateBuilder {
    candidates: Vec<AnnotatedCandidate>,
    seen: HashSet<String>,
}

impl CandidateBuilder {
    fn new() -> Self {
        Self {
            candidates: Vec::new(),
            seen: HashSet::new(),
        }
    }

    fn push(&mut self, ac: AnnotatedCandidate) {
        if self.seen.insert(ac.text.clone()) {
            self.candidates.push(ac);
        }
    }

    fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    fn into_candidates(self) -> Vec<AnnotatedCandidate> {
        self.candidates
    }
}

impl InputMethodEngine {
    /// Run kana-kanji conversion for a reading via llama.cpp model.
    ///
    /// Determines the conversion strategy (main model, light model, or parallel beam),
    /// dispatches to the appropriate model(s), measures latency, and records which model was used.
    ///
    /// Skips the model entirely when the reading has no hiragana/katakana — the
    /// model is trained on kana → kanji and hallucinates garbage (e.g. `「` → `w`)
    /// for symbol- or alphabet-only inputs. Rule-based variants from
    /// `SymbolRewriter` cover those cases instead.
    ///
    /// `api_context` is the left context (lctx) fed to the model. Callers pass
    /// `truncate_context_for_api()` for a whole-buffer conversion, or — for
    /// chunked live conversion — the converted text of the preceding chunks.
    pub(super) fn run_kana_kanji_conversion(
        &mut self,
        reading: &str,
        api_context: &str,
        num_candidates: usize,
    ) -> Vec<String> {
        if !karukan_engine::contains_kana(reading) {
            return vec![];
        }
        let Some(converter) = self.converters.kanji.as_ref() else {
            return vec![];
        };
        let katakana = karukan_engine::hiragana_to_katakana(reading);
        let main_model_name = converter.model_display_name().to_string();

        let strategy = self.determine_strategy(reading, num_candidates);
        debug!(
            "convert: reading=\"{}\" api_context=\"{}\" candidates={} strategy={:?}",
            reading, api_context, num_candidates, strategy
        );

        let start = Instant::now();

        let candidates = match &strategy {
            ConversionStrategy::ParallelBeam { beam_width } => {
                let Some(light_converter) = self.converters.light_kanji.as_ref() else {
                    return vec![];
                };
                let bw = *beam_width;
                let (default_top1, light_candidates) = std::thread::scope(|s| {
                    let h_default = s.spawn(|| {
                        converter
                            .convert(&katakana, api_context, 1)
                            .unwrap_or_default()
                    });
                    let h_beam = s.spawn(|| {
                        light_converter
                            .convert(&katakana, api_context, bw)
                            .unwrap_or_default()
                    });
                    (
                        h_default.join().unwrap_or_default(),
                        h_beam.join().unwrap_or_default(),
                    )
                });
                Self::merge_candidates_dedup(default_top1, light_candidates, bw)
            }
            ConversionStrategy::LightModelOnly => {
                let Some(light_converter) = self.converters.light_kanji.as_ref() else {
                    return vec![];
                };
                light_converter
                    .convert(&katakana, api_context, 1)
                    .unwrap_or_default()
            }
            ConversionStrategy::MainModelOnly => converter
                .convert(&katakana, api_context, 1)
                .unwrap_or_default(),
            ConversionStrategy::MainModelBeam { beam_width } => converter
                .convert(&katakana, api_context, *beam_width)
                .unwrap_or_default(),
        };

        self.metrics.conversion_ms = start.elapsed().as_millis() as u64;
        self.update_adaptive_model_flag(&strategy);

        self.metrics.model_name = match &strategy {
            ConversionStrategy::ParallelBeam { .. } => {
                let light_name = self
                    .converters
                    .light_kanji
                    .as_ref()
                    .map(|c| c.model_display_name().to_string())
                    .unwrap_or_default();
                format!("{}+{}", main_model_name, light_name)
            }
            ConversionStrategy::LightModelOnly => self
                .converters
                .light_kanji
                .as_ref()
                .map(|c| c.model_display_name().to_string())
                .unwrap_or(main_model_name),
            ConversionStrategy::MainModelOnly | ConversionStrategy::MainModelBeam { .. } => {
                main_model_name
            }
        };

        candidates
    }

    /// Start kanji conversion for the current input buffer.
    ///
    /// Called when DOWN/TAB/SPACE is pressed: flushes any pending romaji,
    /// resolves the reading, runs `build_conversion_candidates`, and
    /// transitions into the Conversion state. The previous live-conversion
    /// result is preserved as the first model candidate so the user sees
    /// the same text they had been looking at during input.
    ///
    /// `skip_learning` is set by the Tab path to omit learning-cache
    /// candidates (Space/Down keep the default learning-included behavior).
    pub(super) fn start_conversion(&mut self, skip_learning: bool) -> EngineResult {
        // Flush any remaining romaji into composed_hiragana
        self.flush_romaji_to_composed();

        let reading = self.input_buf.text.clone();

        // Save auto-suggest/live conversion result before clearing state.
        // This ensures the candidate that was displayed during input is preserved
        // in the conversion candidate list even if the re-inference uses a different strategy.
        let prev_suggest_text = std::mem::take(&mut self.live.text);

        self.converters.romaji.reset();
        self.input_buf.cursor_pos = 0;

        if reading.is_empty() {
            return EngineResult::consumed();
        }

        let initial_segments =
            self.build_initial_conversion_segment(&reading, &prev_suggest_text, skip_learning);

        if initial_segments.is_empty() {
            // No candidates, stay in hiragana mode
            let preedit = Preedit::with_text_underlined(&reading);
            self.state = InputState::Composing {
                preedit: preedit.clone(),
                romaji_buffer: String::new(),
            };
            return EngineResult::consumed().with_action(EngineAction::UpdatePreedit(preedit));
        }

        // macOS enters conversion with clause boundaries already present and
        // the first clause focused. Previously Karukan kept the entire
        // reading as one segment until the first Left/Right key, so that key
        // both changed the segmentation and moved focus. Besides looking
        // different, Shift+Left/Right then resized the wrong clause.
        let current_surface = initial_segments[0]
            .candidates
            .selected_text()
            .unwrap_or(&reading)
            .to_string();
        let navigation_segments = self.build_navigation_segments(&reading, &current_surface);
        let segments = if navigation_segments.len() > 1 {
            navigation_segments
        } else {
            initial_segments
        };

        self.enter_conversion_state(segments)
    }

    /// Transition to Conversion state with the given segments.
    ///
    /// Sets up the preedit (highlighted selected text), updates the state, and
    /// returns an EngineResult with preedit, candidates, and aux text actions.
    fn enter_conversion_state(&mut self, segments: Vec<ConversionSegment>) -> EngineResult {
        let active_segment = 0;
        let candidates = segments[active_segment].candidates.clone();
        let preedit = Self::build_conversion_preedit_from_segments(&segments, active_segment);
        let reading = segments[active_segment].reading.clone();

        self.state = InputState::Conversion {
            preedit: preedit.clone(),
            candidates: candidates.clone(),
            segments,
            active_segment,
        };

        EngineResult::consumed()
            .with_action(EngineAction::UpdatePreedit(preedit))
            .with_action(EngineAction::ShowCandidates(candidates.clone()))
            .with_action(EngineAction::UpdateAuxText(
                self.format_aux_conversion_with_page(&reading, Some(&candidates)),
            ))
    }

    fn build_conversion_preedit_from_segments(
        segments: &[ConversionSegment],
        active_segment: usize,
    ) -> Preedit {
        let mut caret = 0;
        let mut text = String::new();
        let mut attributes = Vec::new();
        for (idx, segment) in segments.iter().enumerate() {
            if idx > 0 {
                text.push_str(SEGMENT_DISPLAY_SEPARATOR);
            }
            let start = text.chars().count();
            let segment_text = segment
                .candidates
                .selected_text()
                .unwrap_or(&segment.reading)
                .to_string();
            text.push_str(&segment_text);
            let end = text.chars().count();
            if idx <= active_segment {
                caret = end;
            }
            let attr = if idx == active_segment {
                AttributeType::Highlight
            } else {
                AttributeType::Underline
            };
            attributes.push(PreeditAttribute::new(start, end, attr));
        }
        let mut preedit = Preedit::with_text(text);
        preedit.set_caret(caret);
        preedit.set_attributes(attributes);
        preedit
    }

    fn annotated_to_candidate_list(
        candidates: Vec<AnnotatedCandidate>,
        reading: &str,
    ) -> CandidateList {
        CandidateList::new(
            candidates
                .into_iter()
                .map(|ac| Candidate {
                    reading: Some(ac.reading.unwrap_or_else(|| reading.to_string())),
                    text: ac.text,
                    source: Some(ac.source),
                    description: ac.description,
                })
                .collect(),
        )
    }

    fn candidate_list_for_conversion_segment(
        &mut self,
        reading: &str,
        preferred_text: Option<&str>,
        skip_learning: bool,
    ) -> CandidateList {
        let mut candidates =
            self.build_conversion_candidates(reading, self.config.num_candidates, skip_learning);

        if let Some(preferred_text) = preferred_text
            && !preferred_text.is_empty()
        {
            if let Some(index) = candidates.iter().position(|c| c.text == preferred_text) {
                let preferred = candidates.remove(index);
                candidates.insert(0, preferred);
            } else if preferred_text != reading {
                candidates.insert(
                    0,
                    AnnotatedCandidate::new(preferred_text, CandidateSource::Model),
                );
            }
        }

        Self::annotated_to_candidate_list(candidates, reading)
    }

    fn longest_user_dict_prefix(&self, input: &str) -> Option<(String, String)> {
        let dict = self.dicts.user.as_ref()?;
        dict.common_prefix_search(input)
            .into_iter()
            .filter_map(|entry| {
                let surface = entry.candidates.first()?.surface.clone();
                Some((entry.reading.to_string(), surface))
            })
            .max_by_key(|(reading, _)| reading.chars().count())
    }

    fn longest_system_reading_prefix(&self, input: &str) -> Option<String> {
        const MIN_SYSTEM_WORD_READING_CHARS: usize = 2;
        let dict = self.dicts.system.as_ref()?;
        dict.common_prefix_search(input)
            .into_iter()
            .map(|entry| entry.reading.to_string())
            .filter(|reading| reading.chars().count() >= MIN_SYSTEM_WORD_READING_CHARS)
            .max_by_key(|reading| reading.chars().count())
    }

    fn split_free_reading_segments(&self, reading: &str) -> Vec<ReadingSpan> {
        let chars: Vec<char> = reading.chars().collect();
        let mut spans = Vec::new();
        let mut pending = String::new();
        let mut index = 0usize;
        let max_len = self.chunk_len().clamp(1, MAX_EXPLICIT_SEGMENT_CHARS);

        while index < chars.len() {
            let suffix: String = chars[index..].iter().collect();
            if let Some(dict_reading) = self.longest_system_reading_prefix(&suffix) {
                if !pending.is_empty() {
                    spans.push(ReadingSpan {
                        reading: std::mem::take(&mut pending),
                        fixed_surface: None,
                    });
                }
                let len = dict_reading.chars().count();
                spans.push(ReadingSpan {
                    reading: dict_reading,
                    fixed_surface: None,
                });
                index += len;
                continue;
            }

            pending.push(chars[index]);
            index += 1;
            if pending.chars().count() >= max_len {
                spans.push(ReadingSpan {
                    reading: std::mem::take(&mut pending),
                    fixed_surface: None,
                });
            }
        }

        if !pending.is_empty() {
            spans.push(ReadingSpan {
                reading: pending,
                fixed_surface: None,
            });
        }

        spans
    }

    /// Align a converted surface back to dictionary readings before entering
    /// per-segment navigation. This avoids treating a merely longer prefix as
    /// a word (`きょうはい...` → `きょうはい`) and avoids proportional
    /// surface splits that attach `は` to `今日`.
    fn split_navigation_spans_aligned_to_surface(
        &self,
        reading: &str,
        surface: &str,
    ) -> Option<Vec<ReadingSpan>> {
        let reading_chars: Vec<char> = reading.chars().collect();
        let surface_chars: Vec<char> = surface.chars().collect();
        if reading_chars.is_empty() || surface_chars.is_empty() {
            return None;
        }
        if (reading_chars.len() + 1)
            .checked_mul(surface_chars.len() + 1)
            .is_none_or(|cells| cells > MAX_SURFACE_ALIGNMENT_CELLS)
        {
            return None;
        }

        let mut paths = vec![vec![None; surface_chars.len() + 1]; reading_chars.len() + 1];
        paths[0][0] = Some(SurfaceAlignment::default());
        let max_system_reading_len = self.chunk_len().clamp(1, MAX_EXPLICIT_SEGMENT_CHARS);

        for reading_index in 0..=reading_chars.len() {
            for surface_index in 0..=surface_chars.len() {
                let Some(path) = paths[reading_index][surface_index].clone() else {
                    continue;
                };
                if reading_index == reading_chars.len() || surface_index == surface_chars.len() {
                    continue;
                }

                let reading_suffix: String = reading_chars[reading_index..].iter().collect();
                let position = AlignmentPosition {
                    reading_suffix: &reading_suffix,
                    reading_index,
                    surface_index,
                    surface_chars: &surface_chars,
                };
                if let Some(dictionary) = self.dicts.user.as_ref() {
                    extend_dictionary_alignments(
                        &mut paths,
                        &path,
                        dictionary,
                        &position,
                        reading_chars.len() - reading_index,
                        true,
                    );
                }
                if let Some(dictionary) = self.dicts.system.as_ref() {
                    extend_dictionary_alignments(
                        &mut paths,
                        &path,
                        dictionary,
                        &position,
                        max_system_reading_len,
                        false,
                    );
                }

                // Kana, punctuation, and other unchanged characters bridge
                // gaps between dictionary-backed spans. Consecutive literal
                // characters are kept as one segment.
                if reading_chars[reading_index] == surface_chars[surface_index] {
                    let mut next = path;
                    next.push_literal(reading_chars[reading_index]);
                    keep_better_alignment(&mut paths[reading_index + 1][surface_index + 1], next);
                }
            }
        }

        paths[reading_chars.len()][surface_chars.len()]
            .take()
            .map(|alignment| {
                alignment
                    .spans
                    .into_iter()
                    .flat_map(|span| {
                        if span.fixed_surface.is_some() {
                            vec![span]
                        } else {
                            self.split_free_reading_segments(&span.reading)
                        }
                    })
                    .collect()
            })
    }

    fn split_reading_spans_preserving_user_dict(
        &self,
        reading: &str,
        split_free: bool,
    ) -> Vec<ReadingSpan> {
        let chars: Vec<char> = reading.chars().collect();
        let mut spans = Vec::new();
        let mut pending = String::new();
        let mut index = 0usize;

        while index < chars.len() {
            let suffix: String = chars[index..].iter().collect();
            if let Some((dict_reading, surface)) = self.longest_user_dict_prefix(&suffix) {
                if !pending.is_empty() {
                    if split_free {
                        spans.extend(self.split_free_reading_segments(&pending));
                    } else {
                        spans.push(ReadingSpan {
                            reading: std::mem::take(&mut pending),
                            fixed_surface: None,
                        });
                    }
                    pending.clear();
                }
                let len = dict_reading.chars().count();
                spans.push(ReadingSpan {
                    reading: dict_reading,
                    fixed_surface: Some(surface),
                });
                index += len;
            } else {
                pending.push(chars[index]);
                index += 1;
            }
        }

        if !pending.is_empty() {
            if split_free {
                spans.extend(self.split_free_reading_segments(&pending));
            } else {
                spans.push(ReadingSpan {
                    reading: pending,
                    fixed_surface: None,
                });
            }
        }
        spans
    }

    pub(super) fn user_dictionary_auto_text(&self, reading: &str) -> Option<String> {
        let spans = self.split_reading_spans_preserving_user_dict(reading, false);
        let mut text = String::new();
        let mut has_fixed_span = false;
        for span in spans {
            if let Some(surface) = span.fixed_surface {
                has_fixed_span = true;
                text.push_str(&surface);
            } else {
                text.push_str(&span.reading);
            }
        }
        has_fixed_span.then_some(text)
    }

    pub(super) fn conservative_auto_convert_reading(
        &mut self,
        reading: &str,
        lctx: &str,
    ) -> String {
        let spans = self.split_reading_spans_preserving_user_dict(reading, false);
        if spans.is_empty() {
            return reading.to_string();
        }

        let mut converted = String::new();
        for span in spans {
            if let Some(surface) = span.fixed_surface {
                converted.push_str(&surface);
                continue;
            }

            let ctx = self.truncate_context(&format!("{lctx}{converted}"));
            let text = self
                .run_kana_kanji_conversion(&span.reading, &ctx, 1)
                .into_iter()
                .next()
                .unwrap_or_else(|| span.reading.clone());
            if suspicious_auto_conversion(&span.reading, &text) {
                converted.push_str(&span.reading);
            } else {
                converted.push_str(&text);
            }
        }
        converted
    }

    fn build_initial_conversion_segment(
        &mut self,
        reading: &str,
        prev_suggest_text: &str,
        skip_learning: bool,
    ) -> Vec<ConversionSegment> {
        let preferred = (!prev_suggest_text.is_empty()).then_some(prev_suggest_text);
        let candidates =
            self.candidate_list_for_conversion_segment(reading, preferred, skip_learning);
        (!candidates.is_empty())
            .then_some(ConversionSegment {
                reading: reading.to_string(),
                candidates,
            })
            .into_iter()
            .collect()
    }

    fn build_navigation_segments(
        &mut self,
        reading: &str,
        current_surface: &str,
    ) -> Vec<ConversionSegment> {
        let aligned_spans = (!current_surface.is_empty() && current_surface != reading)
            .then(|| self.split_navigation_spans_aligned_to_surface(reading, current_surface))
            .flatten()
            .filter(|spans| spans.len() > 1);
        let mut spans = aligned_spans
            .unwrap_or_else(|| self.split_reading_spans_preserving_user_dict(reading, true));
        if !current_surface.is_empty() && current_surface != reading {
            spans = coalesce_spans_to_surface_len(spans, current_surface);
        }
        if spans.len() <= 1 {
            return vec![];
        }
        let current_surfaces = (!current_surface.is_empty() && current_surface != reading)
            .then(|| split_surface_preserving_fixed_spans(current_surface, &spans));

        spans
            .into_iter()
            .enumerate()
            .filter_map(|(idx, span)| {
                let preferred = span.fixed_surface.as_deref().or_else(|| {
                    current_surfaces
                        .as_ref()
                        .and_then(|surfaces| surfaces.get(idx))
                        .and_then(|surface| surface.as_deref())
                });
                let candidates =
                    self.candidate_list_for_conversion_segment(&span.reading, preferred, false);
                (!candidates.is_empty()).then_some(ConversionSegment {
                    reading: span.reading,
                    candidates,
                })
            })
            .collect()
    }

    /// Search user and system dictionaries for candidates matching a reading.
    ///
    /// User dictionary results come first (higher priority), then system dictionary
    /// results sorted by score. Duplicates are removed via HashSet.
    fn search_dictionaries(&self, reading: &str, limit: usize) -> Vec<AnnotatedCandidate> {
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();

        // User dictionary (higher priority)
        if let Some(dict) = &self.dicts.user
            && let Some(result) = dict.exact_match_search(reading)
        {
            for cand in result.candidates {
                if candidates.len() >= limit {
                    break;
                }
                if seen.insert(cand.surface.clone()) {
                    candidates.push(AnnotatedCandidate::new(
                        cand.surface.clone(),
                        CandidateSource::UserDictionary,
                    ));
                }
            }
        }

        // System dictionary (sorted by score)
        if let Some(dict) = &self.dicts.system
            && let Some(result) = dict.exact_match_search(reading)
        {
            let mut dict_candidates: Vec<_> = result.candidates.to_vec();
            dict_candidates.sort_by(|a, b| a.score.total_cmp(&b.score));
            for cand in dict_candidates {
                if candidates.len() >= limit {
                    break;
                }
                if seen.insert(cand.surface.clone()) {
                    candidates.push(AnnotatedCandidate::new(
                        cand.surface,
                        CandidateSource::Dictionary,
                    ));
                }
            }
        }

        candidates
    }

    /// Build conversion candidates for a reading from multiple sources.
    ///
    /// Combines learning cache, dictionaries, and model inference results
    /// with deduplication. Uses dynamic candidate count based on input token
    /// count for performance.
    ///
    /// Priority: User Dictionary → Learning → Model → System Dictionary → Fallback
    ///
    /// `skip_learning` suppresses the learning-cache step (1). Used by the Tab
    /// key path so users can escape a noisy learning history without losing
    /// access to dictionary/model candidates.
    pub(super) fn build_conversion_candidates(
        &mut self,
        reading: &str,
        num_candidates: usize,
        skip_learning: bool,
    ) -> Vec<AnnotatedCandidate> {
        // Try to initialize the kanji converter, but don't bail out if it
        // fails — symbol-only inputs (e.g. `。。。`) don't need the model and
        // we still want to produce dictionary, rewriter, and fallback candidates.
        // run_kana_kanji_conversion handles the converter-missing case.
        if self.converters.kanji.is_none()
            && let Err(e) = self.init_kanji_converter()
        {
            debug!("Failed to initialize kanji converter: {}", e);
        }

        let api_context = self.truncate_context_for_api();
        let candidates = self.run_kana_kanji_conversion(reading, &api_context, num_candidates);

        let hiragana = reading.to_string();
        let katakana = karukan_engine::hiragana_to_katakana(reading);

        // Priority: User Dictionary → Learning → Model → System Dictionary → Fallback
        let mut builder = CandidateBuilder::new();
        let dict_results = self.search_dictionaries(reading, usize::MAX);

        // 1. User dictionary candidates. Explicit user registrations should
        //    win over learned history and duplicate text from later sources.
        for ac in &dict_results {
            if ac.source == CandidateSource::UserDictionary {
                builder.push(ac.clone());
            }
        }

        // 2. Learning cache candidates. Skipped when the caller asks for a
        //    learning-free conversion (Tab key).
        if !skip_learning {
            for c in self.lookup_learning_candidates(reading) {
                // Exact matches have reading == input reading; use None to avoid redundancy
                let cand_reading = c.reading.filter(|r| r != reading);
                builder.push(
                    AnnotatedCandidate::new(c.text, CandidateSource::Learning)
                        .with_reading(cand_reading),
                );
            }
        }

        // 3. Model inference results
        if candidates.is_empty() {
            // In emoji mode, defer the literal-fallback decision until
            // after rewriters have run — otherwise `:smile` would be
            // pinned to the top of the candidate list as a Fallback
            // and outrank the 😄 we surface in step 5/6.
            if builder.is_empty() && self.mode.current() != InputMode::Emoji {
                builder.push(AnnotatedCandidate::new(
                    hiragana.clone(),
                    CandidateSource::Fallback,
                ));
            }
        } else {
            for text in candidates {
                builder.push(AnnotatedCandidate::new(text, CandidateSource::Model));
            }
        }

        // 4. System dictionary candidates (from search_dictionaries result)
        for ac in dict_results {
            if ac.source == CandidateSource::Dictionary {
                builder.push(ac);
            }
        }

        // 5/6. Hiragana/katakana fallback + rewriter variants.
        //
        // In emoji mode we surface ONLY the rewriter (i.e. EmojiRewriter)
        // candidates — Slack's emoji picker shows emojis and nothing
        // else, and that's the mental model the user wants here.
        // No literal `:smile` / `:xyz` fallback in the candidate list:
        // if nothing matches, the picker is just empty. (Enter on a
        // no-match query in Composing still commits the buffer
        // literal via `commit_composing`; that's the escape hatch.)
        // Non-emoji modes keep the original order so existing IME
        // behavior is untouched.
        let rewriter_variants = self
            .converters
            .rewriters
            .rewrite_all(&[reading.to_string()]);
        if self.mode.current() == InputMode::Emoji {
            for (variant, description) in rewriter_variants {
                builder.push(
                    AnnotatedCandidate::new(variant, CandidateSource::Rewriter)
                        .with_description(description),
                );
            }
        } else {
            builder.push(AnnotatedCandidate::new(hiragana, CandidateSource::Fallback));
            builder.push(AnnotatedCandidate::new(katakana, CandidateSource::Fallback));
            // Rewriters operate on the user's typed input (the reading
            // itself). Running them on dictionary/model/fallback
            // candidates produces unrelated noise (e.g. a dictionary
            // entry of `,` for some reading would generate `、`/`，`
            // variants the user never asked for; a learning entry `アト`
            // pulled by prefix lookup on `あ` would emit `ｱﾄ`).
            for (variant, description) in rewriter_variants {
                builder.push(
                    AnnotatedCandidate::new(variant, CandidateSource::Rewriter)
                        .with_description(description),
                );
            }
        }

        // 7. Enrich Fallback candidates whose text is a known symbol with
        //    its description (mirrors the relevant slice of mozc's
        //    `AddDescForCurrentCandidates`). Restricted to Fallback so the
        //    AI/Dict/Learning paths don't pick up unwanted labels — e.g.
        //    the model returning `金` for `きん` should NOT inherit mozc's
        //    "部首" annotation. Typed-symbol input still gets annotated:
        //    pressing `「` produces a Fallback candidate `「`, which here
        //    picks up "始めかぎ括弧".
        for c in &mut builder.candidates {
            if c.source == CandidateSource::Fallback
                && c.description.is_none()
                && let Some(desc) = karukan_engine::symbol_description(&c.text)
            {
                c.description = Some(desc.to_string());
            }
        }

        // 8. Attach mozc-style width annotations (`[全]ひらがな`,
        //    `[全]カタカナ`, `[半]カタカナ`) to any pure-kana candidate that
        //    still has no description. This catches `あ`/`ア` candidates that
        //    arrived via the Model or Fallback paths and were deduped against
        //    the rewriter's already-labelled variants.
        for c in &mut builder.candidates {
            if c.description.is_none()
                && let Some(desc) = width_annotation(&c.text)
            {
                c.description = Some(desc.to_string());
            }
        }

        builder.into_candidates()
    }

    /// Look up learning cache candidates for a reading (exact + prefix match, max 3).
    ///
    /// Returns candidates from the learning cache suitable for auto-suggest display.
    pub(super) fn lookup_learning_candidates(&self, reading: &str) -> Vec<Candidate> {
        let Some(cache) = &self.learning else {
            return vec![];
        };
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut seen = HashSet::new();

        // Exact match
        for (surface, _score) in cache.lookup(reading) {
            if candidates.len() >= MAX_LEARNING_CANDIDATES {
                break;
            }
            if seen.insert(surface.clone()) {
                candidates.push(Candidate {
                    text: surface,
                    reading: Some(reading.to_string()),
                    source: Some(CandidateSource::Learning),
                    description: None,
                });
            }
        }

        // Prefix match (predictive)
        for (full_reading, surface, _score) in cache.prefix_lookup(reading) {
            if candidates.len() >= MAX_LEARNING_CANDIDATES {
                break;
            }
            if full_reading == reading {
                continue;
            }
            if full_reading.chars().count() > MAX_LEARNING_PREFIX_READING_CHARS
                || surface.chars().count() > MAX_LEARNING_PREFIX_SURFACE_CHARS
            {
                continue;
            }
            if seen.insert(surface.clone()) {
                candidates.push(Candidate {
                    text: surface,
                    reading: Some(full_reading),
                    source: Some(CandidateSource::Learning),
                    description: None,
                });
            }
        }

        candidates
    }

    /// Build rule-based rewriter variants for the reading itself (e.g. for
    /// symbol input `「` → `『`, `【`, `（`, ...). Used in the auto-suggest path
    /// so users see mozc-style symbol variants without pressing Space first.
    pub(super) fn lookup_rewriter_variants(&self, reading: &str) -> Vec<Candidate> {
        self.converters
            .rewriters
            .rewrite_all(&[reading.to_string()])
            .into_iter()
            .map(|(text, description)| Candidate {
                text,
                reading: Some(reading.to_string()),
                source: Some(CandidateSource::Rewriter),
                description,
            })
            .collect()
    }

    /// Merge two candidate lists with deduplication
    /// Primary candidates come first, then secondary candidates that aren't duplicates
    pub(super) fn merge_candidates_dedup(
        primary: Vec<String>,
        secondary: Vec<String>,
        max_candidates: usize,
    ) -> Vec<String> {
        let mut seen = HashSet::new();
        primary
            .into_iter()
            .chain(secondary)
            .filter(|c| seen.insert(c.clone()))
            .take(max_candidates)
            .collect()
    }

    /// Process key in conversion state
    pub(super) fn process_key_conversion(&mut self, key: &KeyEvent) -> EngineResult {
        match key.keysym {
            Keysym::RETURN => self.commit_conversion(),
            Keysym::ESCAPE => self.cancel_conversion(),
            // macOS Japanese IME candidate navigation.
            Keysym::SPACE if key.modifiers.shift_key => self.prev_candidate(),
            Keysym::DOWN if key.modifiers.shift_key => self.next_candidate_page(),
            Keysym::UP if key.modifiers.shift_key => self.prev_candidate_page(),
            Keysym::SPACE | Keysym::DOWN | Keysym::TAB => self.next_candidate(),
            Keysym::UP => self.prev_candidate(),
            // A selected clause grows/shrinks one reading character at a
            // time with Shift+Right/Left. Bare arrows remain accepted for
            // clause navigation for existing Karukan/fcitx5 users.
            Keysym::LEFT if key.modifiers.shift_key => self.shrink_conversion_segment(),
            Keysym::RIGHT if key.modifiers.shift_key => self.expand_conversion_segment(),
            Keysym::LEFT => self.prev_conversion_segment(),
            Keysym::RIGHT => self.next_conversion_segment(),
            Keysym::PAGE_DOWN => self.next_candidate_page(),
            Keysym::PAGE_UP => self.prev_candidate_page(),
            // Ctrl+Backspace / Ctrl+Delete: delete the selected learning
            // candidate from the history. Backspace doubles as Delete because
            // the Mac "delete" key is Backspace. On a non-learning selection
            // the chord is consumed but does nothing, so it can't leak into
            // the application mid-conversion.
            Keysym::DELETE | Keysym::BACKSPACE
                if key.modifiers.control_key && !key.modifiers.alt_key =>
            {
                if self.selected_is_deletable() {
                    self.delete_selected_candidate_from_history()
                } else {
                    EngineResult::consumed()
                }
            }
            Keysym::BACKSPACE => self.backspace_conversion(),
            _ => {
                // Ctrl+N / Ctrl+P: emacs-style candidate navigation
                if key.modifiers.control_key && !key.modifiers.alt_key {
                    match key.keysym {
                        Keysym::KEY_N | Keysym::KEY_N_UPPER => return self.next_candidate(),
                        Keysym::KEY_P | Keysym::KEY_P_UPPER => return self.prev_candidate(),
                        Keysym::KEY_B | Keysym::KEY_B_UPPER => {
                            return self.next_conversion_segment();
                        }
                        Keysym::KEY_F | Keysym::KEY_F_UPPER => {
                            return self.prev_conversion_segment();
                        }
                        Keysym::KEY_W
                        | Keysym::KEY_W_UPPER
                        | Keysym::KEY_O
                        | Keysym::KEY_O_UPPER => return self.expand_conversion_segment(),
                        Keysym::KEY_I | Keysym::KEY_I_UPPER => {
                            return self.shrink_conversion_segment();
                        }
                        Keysym::KEY_V | Keysym::KEY_V_UPPER => {
                            return self.next_candidate_page();
                        }
                        Keysym::KEY_R | Keysym::KEY_R_UPPER => {
                            return self.prev_candidate_page();
                        }
                        Keysym::KEY_H | Keysym::KEY_H_UPPER => {
                            return self.backspace_conversion();
                        }
                        _ => {}
                    }
                }

                // Check for digit selection (1-9)
                if let Some(digit) = key.keysym.digit_value() {
                    return self.select_candidate_by_digit(digit);
                }

                // Any printable character: commit current conversion and start new input
                if let Some(ch) = key.to_char()
                    && !key.modifiers.control_key
                    && !key.modifiers.alt_key
                {
                    return self.commit_conversion_and_continue(ch);
                }

                EngineResult::not_consumed()
            }
        }
    }

    /// Get selected text/readings from conversion state, or None if not in conversion.
    fn selected_conversion_info(&self) -> Option<(String, Vec<(String, String)>)> {
        match &self.state {
            InputState::Conversion { segments, .. } => {
                let mut text = String::new();
                let mut selections = Vec::with_capacity(segments.len());
                for segment in segments {
                    let selected = segment.candidates.selected().cloned().unwrap_or_else(|| {
                        Candidate::with_reading(&segment.reading, &segment.reading)
                    });
                    text.push_str(&selected.text);
                    let reading = selected.reading.unwrap_or_else(|| segment.reading.clone());
                    selections.push((reading, selected.text));
                }
                Some((text, selections))
            }
            _ => None,
        }
    }

    /// Record a conversion selection in the learning cache.
    pub(super) fn record_learning(&mut self, reading: &str, surface: &str) {
        if let Some(cache) = &mut self.learning {
            cache.record(reading, surface);
        }
    }

    fn record_conversion_learning(&mut self, selections: &[(String, String)]) {
        if self.mode.current() == InputMode::Emoji {
            return;
        }
        for (reading, text) in selections {
            self.record_learning(reading, text);
        }
    }

    /// Advance the cached editor context after committing text and starting a
    /// new composition in the same key event. Frontends cannot refresh their
    /// surrounding-text snapshot between those two operations, so without
    /// this the new word would be converted against context from before the
    /// just-committed text.
    fn advance_surrounding_context_after_commit(&mut self, committed: &str) {
        let (mut left, right) = self
            .surrounding_context
            .as_ref()
            .map(|ctx| {
                (
                    ctx.left.clone().unwrap_or_default(),
                    ctx.right.clone().unwrap_or_default(),
                )
            })
            .unwrap_or_default();
        left.push_str(committed);
        self.set_surrounding_context(&left, &right);
    }

    /// Commit the current conversion
    fn commit_conversion(&mut self) -> EngineResult {
        let Some((text, selections)) = self.selected_conversion_info() else {
            return EngineResult::not_consumed();
        };

        if text.is_empty() {
            return EngineResult::consumed();
        }

        self.record_conversion_learning(&selections);

        self.state = InputState::Empty;
        self.input_buf.text.clear();
        self.live.text.clear();
        self.chunks.clear();
        self.mode.exit_temporary();

        EngineResult::consumed()
            .with_action(EngineAction::HideCandidates)
            .with_action(EngineAction::HideAuxText)
            .with_action(EngineAction::Commit(text))
    }

    /// Commit current conversion and then process a new character as fresh input
    fn commit_conversion_and_continue(&mut self, ch: char) -> EngineResult {
        let Some((text, selections)) = self.selected_conversion_info() else {
            return EngineResult::not_consumed();
        };

        self.record_conversion_learning(&selections);

        self.state = InputState::Empty;
        self.input_buf.text.clear();
        self.live.text.clear();
        self.chunks.clear();
        self.mode.exit_temporary();

        self.advance_surrounding_context_after_commit(&text);

        // Start new input with the character
        let new_input_result = self.start_input(ch);

        // Combine: commit first, then new input actions
        let mut result = EngineResult::consumed()
            .with_action(EngineAction::Commit(text))
            .with_action(EngineAction::HideCandidates);
        result.actions.extend(new_input_result.actions);
        result
    }

    /// Whether the selected candidate can be removed from the learning
    /// history. False when nothing is selected, so the delete chord stays
    /// inert outside the case it is meant for.
    fn selected_is_deletable(&self) -> bool {
        self.state
            .candidates()
            .and_then(|c| c.selected())
            .is_some_and(Candidate::is_deletable)
    }

    /// Delete the selected learning candidate from the history
    /// (Ctrl+Backspace / Ctrl+Delete); the caller guards deletability
    /// ([`Self::selected_is_deletable`]).
    ///
    /// Removes the entry and its prefix twins
    /// ([`LearningCache::remove_suggestion`]), then rebuilds the conversion
    /// rather than dropping the row in place: dedup hid any
    /// model/dictionary/fallback copy of the same surface behind the learning
    /// entry, and only a rebuild brings it back.
    fn delete_selected_candidate_from_history(&mut self) -> EngineResult {
        let (surface, reading) = match &self.state {
            InputState::Conversion {
                segments,
                active_segment,
                ..
            } => {
                let Some(segment) = segments.get(*active_segment) else {
                    return EngineResult::consumed();
                };
                let Some(surface) = segment
                    .candidates
                    .selected()
                    .map(|candidate| candidate.text.clone())
                else {
                    return EngineResult::consumed();
                };
                (surface, segment.reading.clone())
            }
            _ => return EngineResult::consumed(),
        };
        let removed = self
            .learning
            .as_mut()
            .is_some_and(|cache| cache.remove_suggestion(&reading, &surface));
        if !removed {
            return EngineResult::consumed();
        }
        debug!("deleted learning entry: {} -> {}", reading, surface);

        let candidate_list = self.candidate_list_for_conversion_segment(&reading, None, false);
        let (preedit, candidates) = {
            let InputState::Conversion {
                preedit,
                candidates,
                segments,
                active_segment,
            } = &mut self.state
            else {
                return EngineResult::consumed();
            };
            let Some(segment) = segments.get_mut(*active_segment) else {
                return EngineResult::consumed();
            };
            segment.candidates = candidate_list.clone();
            *candidates = candidate_list;
            let updated_preedit =
                Self::build_conversion_preedit_from_segments(segments, *active_segment);
            *preedit = updated_preedit.clone();
            (updated_preedit, candidates.clone())
        };
        self.conversion_update_result(preedit, candidates, &reading)
    }

    /// Cancel conversion and return to hiragana
    pub(super) fn cancel_conversion(&mut self) -> EngineResult {
        if !matches!(self.state, InputState::Conversion { .. }) {
            return EngineResult::not_consumed();
        }
        let reading = self.input_buf.text.clone();

        if reading.is_empty() {
            self.state = InputState::Empty;
            self.input_buf.clear();
            return EngineResult::consumed()
                .with_action(EngineAction::UpdatePreedit(Preedit::new()))
                .with_action(EngineAction::HideCandidates)
                .with_action(EngineAction::HideAuxText);
        }

        // Set up composed_hiragana with the reading
        self.input_buf.text = reading.clone();
        self.input_buf.cursor_pos = self.input_buf.text.chars().count();

        // Reset romaji converter and set output to reading
        self.converters.romaji.reset();
        // We need to push each character to rebuild the state
        for ch in reading.chars() {
            self.converters.romaji.push(ch);
        }

        let preedit = self.set_composing_state();

        EngineResult::consumed()
            .with_action(EngineAction::UpdatePreedit(preedit))
            .with_action(EngineAction::HideCandidates)
            .with_action(EngineAction::UpdateAuxText(self.format_aux_composing()))
    }

    /// Navigate candidates with the given operation, then update preedit
    fn navigate_candidate(&mut self, op: impl FnOnce(&mut CandidateList) -> bool) -> EngineResult {
        let (preedit, candidates, reading) = {
            let InputState::Conversion {
                preedit,
                candidates,
                segments,
                active_segment,
            } = &mut self.state
            else {
                return EngineResult::not_consumed();
            };
            op(candidates);
            if let Some(segment) = segments.get_mut(*active_segment) {
                segment.candidates = candidates.clone();
            }
            let new_preedit =
                Self::build_conversion_preedit_from_segments(segments, *active_segment);
            *preedit = new_preedit.clone();
            let reading = segments
                .get(*active_segment)
                .map(|s| s.reading.clone())
                .unwrap_or_default();
            (new_preedit, candidates.clone(), reading)
        };
        self.conversion_update_result(preedit, candidates, &reading)
    }

    /// Select next candidate
    fn next_candidate(&mut self) -> EngineResult {
        self.navigate_candidate(CandidateList::move_next)
    }

    /// Select previous candidate
    fn prev_candidate(&mut self) -> EngineResult {
        self.navigate_candidate(CandidateList::move_prev)
    }

    /// Go to next candidate page
    fn next_candidate_page(&mut self) -> EngineResult {
        self.navigate_candidate(CandidateList::next_page)
    }

    /// Go to previous candidate page
    fn prev_candidate_page(&mut self) -> EngineResult {
        self.navigate_candidate(CandidateList::prev_page)
    }

    fn move_conversion_segment(&mut self, delta: isize) -> EngineResult {
        if matches!(
            &self.state,
            InputState::Conversion { segments, .. } if segments.len() <= 1
        ) {
            let current_surface = self
                .selected_conversion_info()
                .map(|(text, _)| text)
                .unwrap_or_default();
            let reading = self.input_buf.text.clone();
            let new_segments = self.build_navigation_segments(&reading, &current_surface);
            if new_segments.len() > 1 {
                let len = new_segments.len() as isize;
                let active_segment = if delta >= 0 {
                    1.min(new_segments.len() - 1)
                } else {
                    (len - 1) as usize
                };
                let candidates = new_segments[active_segment].candidates.clone();
                let preedit =
                    Self::build_conversion_preedit_from_segments(&new_segments, active_segment);
                let reading = new_segments[active_segment].reading.clone();
                self.state = InputState::Conversion {
                    preedit: preedit.clone(),
                    candidates: candidates.clone(),
                    segments: new_segments,
                    active_segment,
                };
                return self.conversion_update_result(preedit, candidates, &reading);
            }
        }

        let (preedit, candidates, reading) = {
            let InputState::Conversion {
                preedit,
                candidates,
                segments,
                active_segment,
            } = &mut self.state
            else {
                return EngineResult::not_consumed();
            };

            if segments.len() <= 1 {
                return EngineResult::consumed();
            }

            let len = segments.len() as isize;
            let next = (*active_segment as isize + delta).rem_euclid(len) as usize;
            *active_segment = next;
            *candidates = segments[next].candidates.clone();
            let new_preedit = Self::build_conversion_preedit_from_segments(segments, next);
            *preedit = new_preedit.clone();
            (
                new_preedit,
                candidates.clone(),
                segments[next].reading.clone(),
            )
        };
        self.conversion_update_result(preedit, candidates, &reading)
    }

    fn next_conversion_segment(&mut self) -> EngineResult {
        self.move_conversion_segment(1)
    }

    fn prev_conversion_segment(&mut self) -> EngineResult {
        self.move_conversion_segment(-1)
    }

    /// Move one reading character across the boundary to the right of the
    /// active clause, then reconvert only the two clauses whose readings
    /// changed. This mirrors macOS Shift+Right / Control+W / Control+O.
    fn expand_conversion_segment(&mut self) -> EngineResult {
        self.resize_conversion_segment(true)
    }

    /// Move the last reading character of the active clause into the next
    /// clause, creating that clause when necessary. The active clause is
    /// never allowed to become empty.
    fn shrink_conversion_segment(&mut self) -> EngineResult {
        self.resize_conversion_segment(false)
    }

    fn resize_conversion_segment(&mut self, expand: bool) -> EngineResult {
        let (mut segments, active_segment) = match &self.state {
            InputState::Conversion {
                segments,
                active_segment,
                ..
            } => (segments.clone(), *active_segment),
            _ => return EngineResult::not_consumed(),
        };

        if expand {
            let Some(next_segment) = segments.get(active_segment + 1) else {
                return EngineResult::consumed();
            };
            let mut next_chars = next_segment.reading.chars();
            let Some(moved) = next_chars.next() else {
                return EngineResult::consumed();
            };
            let next_reading: String = next_chars.collect();
            segments[active_segment].reading.push(moved);
            let active_reading = segments[active_segment].reading.clone();
            segments[active_segment].candidates =
                self.candidate_list_for_conversion_segment(&active_reading, None, false);

            if next_reading.is_empty() {
                segments.remove(active_segment + 1);
            } else {
                segments[active_segment + 1].reading = next_reading.clone();
                segments[active_segment + 1].candidates =
                    self.candidate_list_for_conversion_segment(&next_reading, None, false);
            }
        } else {
            let mut active_chars: Vec<char> = segments[active_segment].reading.chars().collect();
            if active_chars.len() <= 1 {
                return EngineResult::consumed();
            }
            let moved = active_chars.pop().expect("length checked above");
            let active_reading: String = active_chars.into_iter().collect();
            segments[active_segment].reading = active_reading.clone();
            segments[active_segment].candidates =
                self.candidate_list_for_conversion_segment(&active_reading, None, false);

            if let Some(next_segment) = segments.get_mut(active_segment + 1) {
                let next_reading = format!("{moved}{}", next_segment.reading);
                next_segment.reading = next_reading.clone();
                next_segment.candidates =
                    self.candidate_list_for_conversion_segment(&next_reading, None, false);
            } else {
                let next_reading = moved.to_string();
                let candidates =
                    self.candidate_list_for_conversion_segment(&next_reading, None, false);
                segments.push(ConversionSegment {
                    reading: next_reading,
                    candidates,
                });
            }
        }

        debug_assert_eq!(
            segments
                .iter()
                .map(|segment| segment.reading.as_str())
                .collect::<String>(),
            self.input_buf.text,
            "clause resizing must preserve the complete reading"
        );

        let candidates = segments[active_segment].candidates.clone();
        let reading = segments[active_segment].reading.clone();
        let preedit = Self::build_conversion_preedit_from_segments(&segments, active_segment);
        self.state = InputState::Conversion {
            preedit: preedit.clone(),
            candidates: candidates.clone(),
            segments,
            active_segment,
        };
        self.conversion_update_result(preedit, candidates, &reading)
    }

    /// Select the candidate at `page_index` (0-based) within the current
    /// page, like pressing the digit key `page_index + 1`. macOS keeps the
    /// composition active until Return confirms the selection.
    pub fn select_candidate_on_page(&mut self, page_index: usize) -> EngineResult {
        let start = std::time::Instant::now();
        self.metrics.conversion_ms = 0;
        let result = self.select_candidate_by_digit(page_index + 1);
        self.metrics.process_key_ms = start.elapsed().as_millis() as u64;
        result
    }

    /// Select candidate by digit (1-9)
    fn select_candidate_by_digit(&mut self, digit: usize) -> EngineResult {
        self.navigate_candidate(|candidates| candidates.select_on_page(digit).is_some())
    }

    /// Build UI update actions after conversion state changed.
    fn conversion_update_result(
        &self,
        preedit: Preedit,
        candidates: CandidateList,
        reading: &str,
    ) -> EngineResult {
        EngineResult::consumed()
            .with_action(EngineAction::UpdatePreedit(preedit))
            .with_action(EngineAction::ShowCandidates(candidates.clone()))
            .with_action(EngineAction::UpdateAuxText(
                self.format_aux_conversion_with_page(reading, Some(&candidates)),
            ))
    }

    /// Handle backspace in conversion mode
    fn backspace_conversion(&mut self) -> EngineResult {
        // Return to hiragana mode with the reading
        self.cancel_conversion()
    }
}

#[cfg(test)]
mod conservative_auto_conversion_tests {
    use super::suspicious_auto_conversion;

    #[test]
    fn rejects_latin_hallucinations_for_hiragana_readings() {
        assert!(suspicious_auto_conversion("あい", "I"));
        assert!(suspicious_auto_conversion("えーあい", "AI"));
    }

    #[test]
    fn accepts_japanese_and_numeric_surfaces() {
        assert!(!suspicious_auto_conversion("あい", "愛"));
        assert!(!suspicious_auto_conversion(
            "ぷろぐらむしょぞく",
            "プログラム所属"
        ));
        assert!(!suspicious_auto_conversion(
            "にせんにじゅうろくねん",
            "2026年"
        ));
    }
}
