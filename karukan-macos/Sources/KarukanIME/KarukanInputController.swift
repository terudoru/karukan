import Cocoa
import InputMethodKit

/// Internal separators make adjacent conversion segments distinct to IMK,
/// but they are not user text and must not survive a frontend-side fallback
/// commit when the engine process disappears.
func visiblePreeditCommitText(_ text: String) -> String {
    text.replacingOccurrences(of: "\u{200B}", with: "")
}

func shouldScheduleDeferredLiveRefresh(
    after actions: [EngineAction], key: EngineKeyEvent
) -> Bool {
    let modifiers = key.modifiers
    let editsText = key.keysym < 0xff00 || key.keysym == 0xff08 || key.keysym == 0xffff
    guard editsText, !modifiers.control, !modifiers.alt, !modifiers.superKey else {
        return false
    }
    let hasComposingPreedit = actions.contains {
        if case .updatePreedit(let text, _, _) = $0 { return !text.isEmpty }
        return false
    }
    let isComposingUpdate = actions.contains {
        if case .hideCandidates = $0 { return true }
        return false
    }
    return hasComposingPreedit && isComposingUpdate
}

func shouldDeferSurroundingTextRefresh(hasPreedit: Bool, key: EngineKeyEvent) -> Bool {
    guard !hasPreedit else { return false }
    // ASCII/Latin-1 keysyms and XKB Unicode keysyms can begin marked text.
    // Navigation/function keys live in the 0xff00 range and cannot.
    return key.keysym < 0xff00 || (0x0100_0100...0x0110_ffff).contains(key.keysym)
}

struct PreeditSnapshot: Equatable {
    let text: String
    let caret: Int
    let attributes: [PreeditAttr]
}

func shouldApplyPreeditUpdate(previous: PreeditSnapshot?, next: PreeditSnapshot) -> Bool {
    previous != next
}

/// InputMethodKit asks for a character index relative to the inline session
/// when positioning a candidate panel. Conversion candidates belong under the
/// focused clause, while predictive candidates belong at the composing caret.
func candidateAnchorUTF16Index(for snapshot: PreeditSnapshot?) -> Int {
    guard let snapshot else { return 0 }
    let focusedStart = snapshot.attributes.first {
        ["underline_double", "highlight", "reverse"].contains($0.style)
    }?.start
    return utf16Offset(
        ofScalarOffset: focusedStart ?? snapshot.caret,
        in: snapshot.text
    )
}

/// Some clients intermittently return a zero rectangle for IMK document
/// access. Treat it as unavailable so an already-positioned panel keeps its
/// last valid anchor instead of jumping to the fallback screen coordinate.
func usableCandidateCursorRect(_ rect: NSRect) -> NSRect? {
    rect == .zero ? nil : rect
}

let liveRefreshDebounceMilliseconds = 250

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
    private var displayedPreeditSnapshot: PreeditSnapshot?
    private weak var activeClientObject: AnyObject?
    private var liveRefreshGeneration = 0
    private var pendingLiveRefresh: DispatchWorkItem?

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
        invalidateLiveRefresh()

        let refreshSurroundingTextAfterKey = shouldDeferSurroundingTextRefresh(
            hasPreedit: hasPreedit, key: key)

        guard let result = engineClient.processKeySync(key) else {
            // Engine busy or dead: let the key pass through rather than
            // freezing input.
            return false
        }
        apply(actions: result.actions, client: client)
        if result.consumed && refreshSurroundingTextAfterKey {
            // Host document access is synchronous and app-dependent. Run it
            // after returning the rule-based first-key update; the queued
            // JSON-RPC request still reaches the engine before the 250 ms
            // deferred live-conversion refresh.
            DispatchQueue.main.async { [weak self] in
                guard let self,
                    let activeClient = self.activeClientObject as? (any IMKTextInput)
                else { return }
                self.sendSurroundingText(client: activeClient)
            }
        }
        if result.consumed && shouldScheduleDeferredLiveRefresh(after: result.actions, key: key) {
            scheduleLiveRefresh()
        }
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
        invalidateLiveRefresh()
        hasPreedit = false
        displayedPreedit = ""
        displayedPreeditSnapshot = nil
        activeClientObject = nil
        if Self.activeController === self {
            Self.activeController = nil
        }
    }

    private func invalidateLiveRefresh() {
        pendingLiveRefresh?.cancel()
        pendingLiveRefresh = nil
        liveRefreshGeneration &+= 1
    }

    /// Neural live conversion is deliberately delayed until input has been
    /// idle for a moment. Rule-based romaji/kana preedit is already visible,
    /// so inference never sits directly in InputMethodKit's key callback.
    private func scheduleLiveRefresh() {
        let generation = liveRefreshGeneration
        let work = DispatchWorkItem { [weak self] in
            guard let self, self.liveRefreshGeneration == generation else { return }
            self.requestLiveRefresh(generation: generation)
        }
        pendingLiveRefresh = work
        DispatchQueue.main.asyncAfter(
            deadline: .now() + .milliseconds(liveRefreshDebounceMilliseconds), execute: work)
    }

    private func requestLiveRefresh(generation: Int) {
        engineClient.refreshLiveConversionAsync { [weak self] result in
            DispatchQueue.main.async {
                guard let self,
                    self.liveRefreshGeneration == generation,
                    let result,
                    let client = self.activeClientObject as? (any IMKTextInput)
                else { return }
                // Keep the visible marked text identical to the engine state:
                // Return may arrive between any two bounded refreshes.
                self.apply(actions: result.actions, client: client)
                guard result.needsLiveRefresh == true else { return }
                let continuation = DispatchWorkItem { [weak self] in
                    guard let self, self.liveRefreshGeneration == generation else { return }
                    self.requestLiveRefresh(generation: generation)
                }
                self.pendingLiveRefresh = continuation
                // Yield long enough for newly typed keys to enter the server
                // and for each one-way clause update to remain visually calm.
                DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(150), execute: continuation)
            }
        }
    }

    private func selectCandidateFromWindow(pageIndex: Int) {
        guard let client = activeClientObject as? (any IMKTextInput) else { return }
        guard let result = engineClient.selectCandidateSync(pageIndex: pageIndex) else { return }
        apply(actions: result.actions, client: client)
    }

    private func moveCandidateFromWindow(step: Int) {
        guard step != 0,
            let client = activeClientObject as? (any IMKTextInput)
        else { return }
        // XKB Up/Down, matching the keyboard candidate navigation path.
        let keysym: UInt32 = step > 0 ? 0xff54 : 0xff52
        let key = EngineKeyEvent(keysym: keysym, modifiers: KeyModifiers())
        guard let result = engineClient.processKeySync(key) else { return }
        apply(actions: result.actions, client: client)
    }

    private func moveCandidatePageFromWindow(step: Int) {
        guard step != 0,
            let client = activeClientObject as? (any IMKTextInput)
        else { return }
        let keysym: UInt32 = step > 0 ? 0xff56 : 0xff55  // Page Down / Page Up
        for _ in 0..<min(abs(step), 100) {
            let key = EngineKeyEvent(keysym: keysym, modifiers: KeyModifiers())
            guard let result = engineClient.processKeySync(key) else { return }
            apply(actions: result.actions, client: client)
        }
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
                let nextSnapshot = PreeditSnapshot(
                    text: text, caret: caret, attributes: attributes)
                let needsRender = shouldApplyPreeditUpdate(
                    previous: displayedPreeditSnapshot, next: nextSnapshot)
                hasPreedit = !text.isEmpty
                displayedPreedit = text
                displayedPreeditSnapshot = nextSnapshot
                if hasPreedit {
                    activeClientObject = client as AnyObject
                    Self.activeController = self
                } else {
                    clearCompositionTracking()
                }
                // Re-applying an identical marked string can make some host
                // editors tear down and redraw the marked range. Skip that
                // redundant IMK call so a no-op live refresh cannot flicker.
                if needsRender {
                    setMarkedText(text: text, caret: caret, attributes: attributes, client: client)
                }

            case .showCandidates(let candidates, let cursor, let page, let totalPages):
                Self.candidateWindow.onCandidateDoubleClick = { pageIndex in
                    Self.activeController?.selectCandidateFromWindow(pageIndex: pageIndex)
                }
                Self.candidateWindow.onCandidateStep = { step in
                    Self.activeController?.moveCandidateFromWindow(step: step)
                }
                Self.candidateWindow.onCandidatePageStep = { step in
                    Self.activeController?.moveCandidatePageFromWindow(step: step)
                }
                // IMK's index is relative to the inline session. Re-query on
                // every candidate update so the panel follows clause changes,
                // caret motion, window movement, and editor scrolling instead
                // of remaining at the first clause's old screen position.
                let anchor = candidateAnchorUTF16Index(for: displayedPreeditSnapshot)
                var lineHeightRect = NSRect.zero
                client.attributes(
                    forCharacterIndex: anchor,
                    lineHeightRectangle: &lineHeightRect
                )
                Self.candidateWindow.show(
                    candidates: candidates,
                    cursor: cursor,
                    page: page,
                    totalPages: totalPages,
                    cursorRect: usableCandidateCursorRect(lineHeightRect)
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
            engineClient.setSurroundingTextAsync(text: "", cursorPos: 0)
            return
        }

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
