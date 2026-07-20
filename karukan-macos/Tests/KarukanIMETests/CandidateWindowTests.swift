import Cocoa
import XCTest

@testable import KarukanIME

final class CandidateWindowTests: XCTestCase {
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
