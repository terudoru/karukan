import Cocoa

func candidateIndexForDoubleClick(clickCount: Int, pageIndex: Int) -> Int? {
    clickCount >= 2 ? pageIndex : nil
}

/// A candidate row receives mouse events without activating the floating
/// panel, preserving keyboard focus in the client application.
final class CandidateRowView: NSView {
    let pageIndex: Int
    var onDoubleClick: ((Int) -> Void)?

    init(pageIndex: Int) {
        self.pageIndex = pageIndex
        super.init(frame: .zero)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override func hitTest(_ point: NSPoint) -> NSView? {
        guard !isHidden, alphaValue > 0, bounds.contains(point) else { return nil }
        // Labels are presentation only; route the entire row to one click
        // target so double-clicking directly on the candidate text works.
        return self
    }

    override func mouseDown(with event: NSEvent) {
        handleClick(count: event.clickCount)
    }

    func handleClick(count: Int) {
        guard
            let index = candidateIndexForDoubleClick(
                clickCount: count,
                pageIndex: pageIndex
            )
        else { return }
        onDoubleClick?(index)
    }
}

/// Place a candidate panel on the display containing the composition cursor,
/// flipping above when necessary and clamping both axes to the visible frame.
/// Kept as a pure function so multi-display edge cases are unit-testable
/// without showing an NSPanel.
func candidatePanelFrame(
    cursorRect: NSRect, panelSize: NSSize, visibleFrames: [NSRect]
) -> NSRect {
    guard
        let visibleFrame = visibleFrames.first(where: { frame in
            frame.contains(NSPoint(x: cursorRect.midX, y: cursorRect.midY))
                || frame.intersects(cursorRect)
        }) ?? visibleFrames.first
    else {
        return NSRect(origin: cursorRect.origin, size: panelSize)
    }

    let width = min(panelSize.width, visibleFrame.width)
    let height = min(panelSize.height, visibleFrame.height)
    let proposedBelow = cursorRect.minY - height
    let proposedY = proposedBelow >= visibleFrame.minY ? proposedBelow : cursorRect.maxY
    let maxX = max(visibleFrame.minX, visibleFrame.maxX - width)
    let maxY = max(visibleFrame.minY, visibleFrame.maxY - height)
    let originX = min(max(cursorRect.minX, visibleFrame.minX), maxX)
    let originY = min(max(proposedY, visibleFrame.minY), maxY)

    return NSRect(x: originX, y: originY, width: width, height: height)
}

/// Custom candidate window (borderless non-activating NSPanel).
///
/// The engine pre-paginates: `show` receives only the visible page plus
/// page metadata, so this controller just renders rows. An optional aux
/// line (reading hint / model info from the engine) is shown as a footer.
class CandidateWindowController {
    // Keep the candidate panel close to macOS' built-in Japanese IME scale.
    private static let candidateFontSize: CGFloat = 14
    private static let footerFontSize: CGFloat = 11
    private static let minPanelWidth: CGFloat = 140
    private static let minPanelHeight: CGFloat = 24
    private static let panelWidthPadding: CGFloat = 8
    private static let panelHeightPadding: CGFloat = 4
    private static let panelCornerRadius: CGFloat = 8
    private static let stackHorizontalInset: CGFloat = 3
    private static let stackVerticalInset: CGFloat = 2

    private let panel: NSPanel
    private let stackView: NSStackView
    private let panelBackgroundView: NSView
    private var rowViews: [NSView] = []
    private var auxText: String?
    var onCandidateDoubleClick: ((Int) -> Void)?

    private struct PageState {
        let candidates: [CandidateItem]
        let cursor: Int
        let page: Int
        let totalPages: Int
    }
    private var pageState: PageState?

    init() {
        panel = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 100),
            styleMask: [.nonactivatingPanel, .borderless],
            backing: .buffered,
            defer: true
        )
        panel.level = .popUpMenu
        panel.hidesOnDeactivate = false
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.ignoresMouseEvents = false
        panel.hasShadow = true

        panelBackgroundView = NSView()
        panelBackgroundView.translatesAutoresizingMaskIntoConstraints = false
        panelBackgroundView.wantsLayer = true
        panelBackgroundView.layer?.cornerRadius = Self.panelCornerRadius
        panelBackgroundView.layer?.borderWidth = 0.5
        panelBackgroundView.layer?.borderColor = NSColor.separatorColor.cgColor
        panelBackgroundView.layer?.backgroundColor = NSColor.windowBackgroundColor.cgColor
        panelBackgroundView.layer?.masksToBounds = true

        stackView = NSStackView()
        stackView.orientation = .vertical
        stackView.alignment = .leading
        stackView.spacing = 1
        stackView.edgeInsets = NSEdgeInsets(
            top: Self.stackVerticalInset,
            left: Self.stackHorizontalInset,
            bottom: Self.stackVerticalInset,
            right: Self.stackHorizontalInset
        )
        stackView.translatesAutoresizingMaskIntoConstraints = false

        panelBackgroundView.addSubview(stackView)
        panel.contentView?.addSubview(panelBackgroundView)
        if let contentView = panel.contentView {
            NSLayoutConstraint.activate([
                panelBackgroundView.topAnchor.constraint(equalTo: contentView.topAnchor),
                panelBackgroundView.leadingAnchor.constraint(equalTo: contentView.leadingAnchor),
                panelBackgroundView.trailingAnchor.constraint(equalTo: contentView.trailingAnchor),
                panelBackgroundView.bottomAnchor.constraint(equalTo: contentView.bottomAnchor),
                stackView.topAnchor.constraint(equalTo: panelBackgroundView.topAnchor),
                stackView.leadingAnchor.constraint(equalTo: panelBackgroundView.leadingAnchor),
                stackView.trailingAnchor.constraint(equalTo: panelBackgroundView.trailingAnchor),
                stackView.bottomAnchor.constraint(equalTo: panelBackgroundView.bottomAnchor),
            ])
        }
    }

    var isVisible: Bool { panel.isVisible }

    /// `cursorRect: nil` reuses the rect from the previous `show` — the
    /// caller can skip its (synchronous, per-keystroke) client IPC while
    /// the panel is already on screen, since the composition anchor
    /// doesn't move mid-composition.
    func show(
        candidates: [CandidateItem], cursor: Int, page: Int, totalPages: Int, cursorRect: NSRect?
    ) {
        pageState = PageState(
            candidates: candidates, cursor: cursor, page: page, totalPages: totalPages)
        render(cursorRect: cursorRect)
    }

    /// Update the aux footer; re-renders in place if the window is visible.
    /// Pass `deferRender: true` when a `show`/`hide` follows in the same
    /// action batch, so the panel is rendered once per batch instead of
    /// once for the aux change and again for the candidates.
    func setAux(_ text: String?, deferRender: Bool = false) {
        auxText = text
        if !deferRender, panel.isVisible, pageState != nil {
            render(cursorRect: nil)
        }
    }

    func hide() {
        pageState = nil
        panel.orderOut(nil)
    }

    private func render(cursorRect: NSRect?) {
        clearRows()
        guard let state = pageState, !state.candidates.isEmpty else {
            hide()
            return
        }

        for (index, candidate) in state.candidates.enumerated() {
            addCandidateRow(candidate, number: index + 1, selected: index == state.cursor)
        }
        if state.totalPages > 1 {
            addFooterLabel("[\(state.page + 1)/\(state.totalPages)]")
        }
        if let aux = auxText, !aux.isEmpty {
            addFooterLabel(aux)
        }

        positionPanel(cursorRect: cursorRect)
    }

    private func clearRows() {
        for view in rowViews {
            stackView.removeArrangedSubview(view)
            view.removeFromSuperview()
        }
        rowViews.removeAll()
    }

    private func addCandidateRow(_ candidate: CandidateItem, number: Int, selected: Bool) {
        let rowContainer = CandidateRowView(pageIndex: number - 1)
        rowContainer.onDoubleClick = { [weak self] pageIndex in
            self?.onCandidateDoubleClick?(pageIndex)
        }
        rowContainer.translatesAutoresizingMaskIntoConstraints = false
        rowContainer.wantsLayer = true
        rowContainer.layer?.cornerRadius = 4
        rowContainer.layer?.masksToBounds = true
        rowContainer.layer?.backgroundColor = selected
            ? NSColor.selectedControlColor.cgColor
            : NSColor.clear.cgColor

        let text = NSMutableAttributedString(
            string: "\(number). \(candidate.text)",
            attributes: [
                .font: NSFont.systemFont(ofSize: Self.candidateFontSize),
                .foregroundColor:
                    selected ? NSColor.alternateSelectedControlTextColor : NSColor.labelColor,
            ]
        )
        if let description = candidate.description {
            text.append(
                NSAttributedString(
                    string: "  \(description)",
                    attributes: [
                        .font: NSFont.systemFont(ofSize: Self.footerFontSize),
                        .foregroundColor: selected
                            ? NSColor.alternateSelectedControlTextColor.withAlphaComponent(0.8)
                            : NSColor.secondaryLabelColor,
                    ]
                ))
        }

        let label = NSTextField(labelWithAttributedString: text)
        label.translatesAutoresizingMaskIntoConstraints = false
        label.lineBreakMode = .byTruncatingTail
        label.setContentHuggingPriority(.defaultLow, for: .horizontal)
        rowContainer.addSubview(label)

        NSLayoutConstraint.activate([
            rowContainer.heightAnchor.constraint(greaterThanOrEqualToConstant: Self.minPanelHeight),
            label.topAnchor.constraint(
                equalTo: rowContainer.topAnchor, constant: 2),
            label.leadingAnchor.constraint(
                equalTo: rowContainer.leadingAnchor, constant: 6),
            label.trailingAnchor.constraint(
                equalTo: rowContainer.trailingAnchor, constant: -6),
            label.bottomAnchor.constraint(
                equalTo: rowContainer.bottomAnchor, constant: -2),
        ])

        stackView.addArrangedSubview(rowContainer)
        rowViews.append(rowContainer)
    }

    private func addFooterLabel(_ text: String) {
        let label = NSTextField(labelWithString: text)
        label.font = NSFont.systemFont(ofSize: Self.footerFontSize)
        label.textColor = NSColor.secondaryLabelColor
        label.translatesAutoresizingMaskIntoConstraints = false
        stackView.addArrangedSubview(label)
        rowViews.append(label)
    }

    private var lastCursorRect: NSRect = .zero

    private func positionPanel(cursorRect: NSRect?) {
        if let rect = cursorRect {
            lastCursorRect = rect
        }
        let cursorRect = lastCursorRect

        stackView.layoutSubtreeIfNeeded()
        let contentSize = stackView.fittingSize
        let panelWidth = max(contentSize.width + Self.panelWidthPadding, Self.minPanelWidth)
        let panelHeight = contentSize.height + Self.panelHeightPadding

        guard cursorRect != .zero else {
            panel.setFrame(
                NSRect(x: 100, y: 100, width: panelWidth, height: panelHeight), display: true)
            panel.orderFront(nil)
            return
        }

        let frame = candidatePanelFrame(
            cursorRect: cursorRect,
            panelSize: NSSize(width: panelWidth, height: panelHeight),
            visibleFrames: NSScreen.screens.map(\.visibleFrame)
        )
        panel.setFrame(frame, display: true)
        panel.orderFront(nil)
    }
}
