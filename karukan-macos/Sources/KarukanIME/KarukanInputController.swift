import Cocoa
import InputMethodKit

/// Internal separators make adjacent conversion segments distinct to IMK,
/// but they are not user text and must not survive a frontend-side fallback
/// commit when the engine process disappears.
func visiblePreeditCommitText(_ text: String) -> String {
    text.replacingOccurrences(of: "\u{200B}", with: "")
}

/// Thin InputMethodKit adapter for the karukan engine.
///
/// All IME state (Empty → Composing → Conversion, romaji conversion,
/// candidates, learning) lives in karukan-imserver; this controller only
/// translates key events and applies the resulting UI actions, mirroring
/// the fcitx5 addon (karukan.cpp).
@objc(KarukanInputController)
class KarukanInputController: IMKInputController {
    static let candidateWindow = CandidateWindowController()
    private static weak var activeController: KarukanInputController?

    /// Mirrors whether the engine currently shows a preedit (updated from
    /// engine actions). Used to decide when to refresh surrounding text.
    private var hasPreedit = false
    private var displayedPreedit = ""
    private weak var activeClientObject: AnyObject?

    /// Detects the lone right-⌘ tap that returns to hiragana mode on
    /// keyboards without a JIS かな key (issue #33).
    private var rightCommandTap = RightCommandTapDetector()

    // MARK: - Event handling

    override func recognizedEvents(_ sender: Any!) -> Int {
        // flagsChanged is needed for the right-⌘ tap detection; plain
        // keyDown events never carry lone-modifier transitions.
        Int(NSEvent.EventTypeMask([.keyDown, .flagsChanged]).rawValue)
    }

    override func handle(_ event: NSEvent!, client sender: Any!) -> Bool {
        guard let event else { return false }

        // Lone right-⌘ tap → hiragana, the US-layout stand-in for the JIS
        // かな key. Never consume flagsChanged: the system must keep an
        // accurate view of modifier state.
        if event.type == .flagsChanged {
            let fired = rightCommandTap.handleFlagsChanged(
                keyCode: event.keyCode,
                flags: event.modifierFlags.intersection(.deviceIndependentFlagsMask)
            )
            if fired, let client = sender as? (any IMKTextInput) {
                sendKanaToggle(client: client)
            }
            return false
        }

        guard event.type == .keyDown else { return false }
        guard let client = sender as? (any IMKTextInput) else { return false }
        activeClientObject = client as AnyObject

        // A real key press means any held right ⌘ is a shortcut modifier,
        // not a pending tap.
        rightCommandTap.cancel()

        let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        // Never swallow Command shortcuts.
        if flags.contains(.command) { return false }

        // JIS かな key (and Karabiner right-Command tap → かな): always
        // consume so the system doesn't process keyCode 104 after the engine
        // returns not_consumed (already in hiragana mode).
        if event.keyCode == KeyCodeMap.kanaKeyCode {
            sendKanaToggle(client: client)
            return true
        }

        // JIS 英数 key: flush pending composition so preedit doesn't linger
        // after macOS switches to the English input source.
        if event.keyCode == KeyCodeMap.eisuKeyCode {
            flushComposition(client: client)
            return false
        }

        guard let key = KeyCodeMap.translate(event: event) else { return false }

        // Refresh the conversion context while no composition is active
        // (mirrors the fcitx5 addon, which captures surrounding text in the
        // Empty state). Queued before process_key on the same pipe, so the
        // engine sees it first. Skipped for function/navigation keysyms
        // (0xff00 range): they can't start a composition, and the three
        // synchronous client IPCs in sendSurroundingText would otherwise
        // fire on every arrow-key repeat.
        if !hasPreedit && key.keysym < 0xff00 {
            sendSurroundingText(client: client)
        }

        guard let result = engineClient.processKeySync(key) else {
            // Engine busy or dead: let the key pass through rather than
            // freezing input.
            return false
        }
        apply(actions: result.actions, client: client)
        return result.consumed
    }

    /// Send the return-to-hiragana toggle (Super_R) to the engine and apply
    /// the resulting actions.
    private func sendKanaToggle(client: any IMKTextInput) {
        let key = EngineKeyEvent(keysym: KeyCodeMap.superRKeysym, modifiers: KeyModifiers())
        if let result = engineClient.processKeySync(key) {
            apply(actions: result.actions, client: client)
        }
    }

    // MARK: - Lifecycle

    /// Preserve the currently displayed composition before the shared engine
    /// is replaced. The replacement starts empty, so waiting for its first key
    /// would otherwise overwrite the old marked text.
    static func prepareForEngineRestart() {
        guard let controller = activeController else {
            candidateWindow.hide()
            return
        }
        controller.commitDisplayedPreeditLocally()
    }

    override func deactivateServer(_ sender: Any!) {
        // A right-⌘ press armed before a focus switch must not fire after
        // it (e.g. right-⌘-clicking another window).
        rightCommandTap.cancel()
        // Mozc-style: commit the pending preedit on focus loss, then
        // persist what the user taught us.
        if let client = sender as? (any IMKTextInput) {
            flushComposition(client: client)
        } else {
            Self.candidateWindow.hide()
            clearCompositionTracking()
        }
        engineClient.saveLearningAsync()
        super.deactivateServer(sender)
    }

    override func commitComposition(_ sender: Any!) {
        if let client = sender as? (any IMKTextInput) {
            flushComposition(client: client)
        } else {
            Self.candidateWindow.hide()
            clearCompositionTracking()
        }
    }

    /// Commit any pending composition via the engine and apply the cleanup
    /// actions it emits (clear preedit, hide candidates/aux).
    private func flushComposition(client: any IMKTextInput) {
        if let result = engineClient.commitSync() {
            apply(actions: result.actions, client: client)
        } else {
            // The engine no longer knows this composition, but IMK still
            // displays it. Commit the exact visible text rather than leaving
            // a stuck marked range or silently losing it.
            commitDisplayedPreeditLocally(client: client)
        }
    }

    private func commitDisplayedPreeditLocally() {
        guard let client = activeClientObject as? (any IMKTextInput) else {
            Self.candidateWindow.hide()
            clearCompositionTracking()
            return
        }
        commitDisplayedPreeditLocally(client: client)
    }

    private func commitDisplayedPreeditLocally(client: any IMKTextInput) {
        let text = visiblePreeditCommitText(displayedPreedit)
        if hasPreedit, !text.isEmpty {
            client.insertText(text, replacementRange: NSRange(location: NSNotFound, length: 0))
        }
        // If the old process is merely slow rather than dead, discard its
        // stale composition before it can reappear on the next key event.
        engineClient.resetAsync()
        Self.candidateWindow.hide()
        clearCompositionTracking()
    }

    private func clearCompositionTracking() {
        hasPreedit = false
        displayedPreedit = ""
        activeClientObject = nil
        if Self.activeController === self {
            Self.activeController = nil
        }
    }

    private func selectCandidateFromWindow(pageIndex: Int) {
        guard let client = activeClientObject as? (any IMKTextInput) else { return }
        guard let result = engineClient.selectCandidateSync(pageIndex: pageIndex) else { return }
        apply(actions: result.actions, client: client)
    }

    // MARK: - Applying engine actions

    private func apply(actions: [EngineAction], client: any IMKTextInput) {
        // The engine emits ShowCandidates before UpdateAux. Fold aux changes
        // in first (deferring their render when a candidate update follows)
        // so the panel is rendered once per batch, not once for the
        // candidates and again for the aux footer.
        let updatesCandidates = actions.contains {
            switch $0 {
            case .showCandidates, .hideCandidates: return true
            default: return false
            }
        }
        for action in actions {
            switch action {
            case .updateAux(let text):
                Self.candidateWindow.setAux(text, deferRender: updatesCandidates)
            case .hideAux:
                Self.candidateWindow.setAux(nil, deferRender: updatesCandidates)
            default:
                break
            }
        }

        for action in actions {
            switch action {
            case .commit(let text):
                // insertText replaces the marked text and ends the
                // composition; since #46 the engine no longer pairs Commit
                // with an empty UpdatePreedit, so clear the flag here or the
                // next keystroke would skip the surrounding-text refresh.
                clearCompositionTracking()
                client.insertText(text, replacementRange: NSRange(location: NSNotFound, length: 0))

            case .updatePreedit(let text, let caret, let attributes):
                hasPreedit = !text.isEmpty
                displayedPreedit = text
                if hasPreedit {
                    activeClientObject = client as AnyObject
                    Self.activeController = self
                } else {
                    clearCompositionTracking()
                }
                setMarkedText(text: text, caret: caret, attributes: attributes, client: client)

            case .showCandidates(let candidates, let cursor, let page, let totalPages):
                Self.candidateWindow.onCandidateDoubleClick = { pageIndex in
                    Self.activeController?.selectCandidateFromWindow(pageIndex: pageIndex)
                }
                // Query the composition anchor (a synchronous IPC into the
                // focused app) only when the panel comes on screen; it
                // doesn't move while the panel stays visible.
                var cursorRect: NSRect?
                if !Self.candidateWindow.isVisible {
                    var lineHeightRect = NSRect.zero
                    client.attributes(forCharacterIndex: 0, lineHeightRectangle: &lineHeightRect)
                    cursorRect = lineHeightRect
                }
                Self.candidateWindow.show(
                    candidates: candidates,
                    cursor: cursor,
                    page: page,
                    totalPages: totalPages,
                    cursorRect: cursorRect
                )

            case .hideCandidates:
                Self.candidateWindow.hide()

            case .updateAux, .hideAux:
                break  // applied above
            }
        }
    }

    /// Send the text left of the cursor to the engine as conversion
    /// context. Gated on `selectedRange` only: `client.length()` is the
    /// least-implemented part of IMKTextInput (it returns 0 even in apps
    /// whose `attributedSubstring` works fine), and the request below is
    /// capped to 40 UTF-16 units anyway, so document size doesn't matter.
    /// Whether a client supports this at all is app-dependent (Cocoa text
    /// views do; Electron/Chromium/terminals mostly don't), so the skip
    /// reasons are logged for dogfooding visibility.
    private func sendSurroundingText(client: any IMKTextInput) {
        // When capture isn't possible, CLEAR the engine's context rather
        // than skipping: leaving the context from a previous cursor
        // position in place makes the engine condition on (and display)
        // text that is no longer left of the cursor. No context beats a
        // wrong one. selectedRange flakiness is per-keystroke in some
        // apps, so this also self-heals on the next successful capture.
        let selected = client.selectedRange()
        guard selected.location != NSNotFound, selected.location > 0 else {
            NSLog("KarukanIME: surrounding text cleared (no usable selection)")
            engineClient.setSurroundingTextAsync(text: "", cursorPos: 0)
            return
        }

        let maxContextUTF16 = 40  // engine truncates further per its config
        let start = max(0, selected.location - maxContextUTF16)
        let range = NSRange(location: start, length: selected.location - start)
        // string(from:actualRange:) rather than attributedSubstring(from:):
        // it's the IMKTextInput document-access method clients actually
        // implement (azooKey-Desktop settled on the same call).
        var actualRange = NSRange()
        guard let leftContext = client.string(from: range, actualRange: &actualRange),
            !leftContext.isEmpty
        else {
            NSLog("KarukanIME: surrounding text cleared (string(from:) unavailable)")
            engineClient.setSurroundingTextAsync(text: "", cursorPos: 0)
            return
        }

        NSLog("KarukanIME: surrounding text captured (\(leftContext.count) chars)")
        engineClient.setSurroundingTextAsync(
            text: leftContext,
            cursorPos: leftContext.unicodeScalars.count
        )
    }

    private func setMarkedText(
        text: String, caret: Int, attributes: [PreeditAttr], client: any IMKTextInput
    ) {
        guard !text.isEmpty else {
            client.setMarkedText(
                NSAttributedString(string: ""),
                selectionRange: NSRange(location: 0, length: 0),
                replacementRange: NSRange(location: NSNotFound, length: 0)
            )
            return
        }

        let attributed = NSMutableAttributedString(string: text)
        for attr in attributes {
            guard let range = utf16Range(of: attr.start..<attr.end, in: text) else { continue }
            let style: NSUnderlineStyle
            switch attr.style {
            // The focused/highlighted segment is drawn with a thick
            // underline (the convention azooKey/mac-akaza use for marked
            // text, since background colors are unreliable across apps).
            case "underline_double", "highlight", "reverse":
                style = .thick
                attributed.addAttribute(.underlineColor, value: NSColor.labelColor, range: range)
            default:
                style = .single
                attributed.addAttribute(
                    .underlineColor, value: NSColor.tertiaryLabelColor, range: range)
            }
            attributed.addAttribute(.underlineStyle, value: style.rawValue, range: range)
        }

        let caretUTF16 = utf16Offset(ofScalarOffset: caret, in: text)
        // Keep IMK's actual selection collapsed. A non-empty selectionRange is
        // treated by some clients as text selected inside the marked text; the
        // next setMarkedText can then replace only that range and duplicate a
        // clause. Segment focus is represented by underline attributes instead.
        client.setMarkedText(
            attributed,
            selectionRange: NSRange(location: caretUTF16, length: 0),
            replacementRange: NSRange(location: NSNotFound, length: 0)
        )
    }
}

// MARK: - Unicode scalar → UTF-16 offset conversion

/// The engine reports positions in Unicode scalar values; IMK APIs take
/// UTF-16 offsets.
func utf16Offset(ofScalarOffset offset: Int, in text: String) -> Int {
    let scalars = text.unicodeScalars
    let clamped = min(max(offset, 0), scalars.count)
    let index = scalars.index(scalars.startIndex, offsetBy: clamped)
    return text.utf16.distance(from: text.utf16.startIndex, to: index)
}

func utf16Range(of scalarRange: Range<Int>, in text: String) -> NSRange? {
    guard scalarRange.lowerBound >= 0, scalarRange.lowerBound <= scalarRange.upperBound else {
        return nil
    }
    let start = utf16Offset(ofScalarOffset: scalarRange.lowerBound, in: text)
    let end = utf16Offset(ofScalarOffset: scalarRange.upperBound, in: text)
    return NSRange(location: start, length: end - start)
}
