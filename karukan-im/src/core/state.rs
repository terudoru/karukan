//! Input state machine
//!
//! Defines the states of the IME and transitions between them.

use super::candidate::CandidateList;
use super::preedit::Preedit;

/// One segment inside conversion mode.
#[derive(Debug, Clone)]
pub struct ConversionSegment {
    /// Reading represented by this segment.
    pub reading: String,
    /// Candidate list for this segment.
    pub candidates: CandidateList,
    /// True when this inactive clause currently has only dictionary/history/
    /// fallback candidates. Model candidates are generated when the user
    /// focuses the clause, avoiding N sequential inferences on Space.
    pub needs_expansion: bool,
}

/// The current state of the IME
#[derive(Debug, Clone, Default)]
pub enum InputState {
    /// No input, waiting for user to type
    #[default]
    Empty,

    /// Composing mode - building preedit text (hiragana, katakana, or alphabet)
    Composing {
        /// The preedit string being composed
        preedit: Preedit,
        /// Unconverted romaji buffer (e.g., "k" waiting for next char)
        romaji_buffer: String,
    },

    /// Conversion mode - selecting from candidates
    Conversion {
        /// The preedit string showing conversion result
        preedit: Preedit,
        /// Candidate list for the active segment.
        candidates: CandidateList,
        /// All conversion segments, each with its own candidate list.
        segments: Vec<ConversionSegment>,
        /// Active segment index in `segments`.
        active_segment: usize,
        /// Whether this conversion was started with the learning-free Tab
        /// path. Clause navigation and resizing must preserve this choice.
        skip_learning: bool,
    },
}

impl InputState {
    /// Check if the engine is in the Empty (idle) state
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    /// Get the current preedit if any
    pub fn preedit(&self) -> Option<&Preedit> {
        match self {
            Self::Empty => None,
            Self::Composing { preedit, .. } => Some(preedit),
            Self::Conversion { preedit, .. } => Some(preedit),
        }
    }

    /// Get mutable reference to preedit
    pub fn preedit_mut(&mut self) -> Option<&mut Preedit> {
        match self {
            Self::Empty => None,
            Self::Composing { preedit, .. } => Some(preedit),
            Self::Conversion { preedit, .. } => Some(preedit),
        }
    }

    /// Get candidates in conversion state
    pub fn candidates(&self) -> Option<&CandidateList> {
        match self {
            Self::Conversion { candidates, .. } => Some(candidates),
            _ => None,
        }
    }

    /// Get mutable reference to candidates
    pub fn candidates_mut(&mut self) -> Option<&mut CandidateList> {
        match self {
            Self::Conversion { candidates, .. } => Some(candidates),
            _ => None,
        }
    }
}
