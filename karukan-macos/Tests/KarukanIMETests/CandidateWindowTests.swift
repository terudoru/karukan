import Cocoa
import XCTest

@testable import KarukanIME

final class CandidateWindowTests: XCTestCase {
    func testCandidateScrollAccumulatesTrackpadDeltasAndMapsDirection() {
        let partial = candidateScrollResult(accumulated: 0, delta: -3, precise: true)
        XCTAssertEqual(partial, CandidateScrollResult(step: 0, remainder: -3))

        let next = candidateScrollResult(
            accumulated: partial.remainder, delta: -5, precise: true)
        XCTAssertEqual(next, CandidateScrollResult(step: 1, remainder: 0))

        let previous = candidateScrollResult(accumulated: 0, delta: 1, precise: false)
        XCTAssertEqual(previous, CandidateScrollResult(step: -1, remainder: 0))
    }

    func testCandidateRowTracksSelectionWithoutAStoredColor() {
        let row = CandidateRowView(pageIndex: 0)
        row.isSelected = true

        XCTAssertTrue(row.isSelected)
        XCTAssertNil(row.layer?.backgroundColor)
    }

    func testCandidateAuxHidesDiagnosticsButKeepsActionableHint() {
        XCTAssertNil(userFacingCandidateAux("[変換] きょう | 12ms/13ms 4tok | model"))
        XCTAssertEqual(
            userFacingCandidateAux(
                "[変換] きょう | 12ms/13ms 4tok | model | 📝 学習 "
                    + "(Ctrl+Shift+Deleteで履歴から削除)"),
            "Ctrl+Shift+Deleteで履歴から削除"
        )
    }

    func testOnlyDoubleClickSelectsCandidate() {
        XCTAssertNil(candidateIndexForDoubleClick(clickCount: 1, pageIndex: 3))
        XCTAssertEqual(candidateIndexForDoubleClick(clickCount: 2, pageIndex: 3), 3)
        XCTAssertEqual(candidateIndexForDoubleClick(clickCount: 3, pageIndex: 3), 3)

        let row = CandidateRowView(pageIndex: 3)
        XCTAssertTrue(row.acceptsFirstMouse(for: nil))
        var selectedIndex: Int?
        row.onDoubleClick = { selectedIndex = $0 }
        row.handleClick(count: 1)
        XCTAssertNil(selectedIndex)
        row.handleClick(count: 2)
        XCTAssertEqual(selectedIndex, 3)

        selectedIndex = nil
        XCTAssertTrue(row.accessibilityPerformPress())
        XCTAssertEqual(selectedIndex, 3)
    }

    func testPanelClampsToRightEdge() {
        let frame = candidatePanelFrame(
            cursorRect: NSRect(x: 950, y: 500, width: 2, height: 20),
            panelSize: NSSize(width: 200, height: 100),
            visibleFrames: [NSRect(x: 0, y: 0, width: 1000, height: 800)]
        )

        XCTAssertEqual(frame.origin.x, 800)
        XCTAssertEqual(frame.origin.y, 400)
    }

    func testPanelUsesDisplayContainingCursor() {
        let frame = candidatePanelFrame(
            cursorRect: NSRect(x: 1950, y: 500, width: 2, height: 20),
            panelSize: NSSize(width: 200, height: 100),
            visibleFrames: [
                NSRect(x: 0, y: 0, width: 1000, height: 800),
                NSRect(x: 1000, y: 0, width: 1000, height: 800),
            ]
        )

        XCTAssertEqual(frame.origin.x, 1800)
        XCTAssertEqual(frame.origin.y, 400)
    }

    func testPanelFlipsAboveCursorNearBottom() {
        let frame = candidatePanelFrame(
            cursorRect: NSRect(x: 100, y: 10, width: 2, height: 20),
            panelSize: NSSize(width: 200, height: 100),
            visibleFrames: [NSRect(x: 0, y: 0, width: 1000, height: 800)]
        )

        XCTAssertEqual(frame.origin.y, 30)
    }
}
