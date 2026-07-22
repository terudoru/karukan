import XCTest

@testable import KarukanIME

/// Integration tests driving a real karukan-imserver binary through
/// EngineProcess + EngineClient. Skipped when the Rust binary hasn't been
/// built (run `cargo build -p karukan-im --bin karukan-imserver` first;
/// `make test` does this automatically).
///
/// Only config-independent requests are exercised: the server loads the
/// user's config.toml, so anything involving conversion behavior is
/// covered by the Rust-side tests instead.
final class TransportTests: XCTestCase {
    static func serverBinaryPath() -> String? {
        // <repo>/karukan-macos/Tests/KarukanIMETests/TransportTests.swift
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // KarukanIMETests
            .deletingLastPathComponent()  // Tests
            .deletingLastPathComponent()  // karukan-macos
            .deletingLastPathComponent()  // repo root
        for profile in ["release", "debug"] {
            let candidate =
                repoRoot
                .appendingPathComponent("target/\(profile)/karukan-imserver").path
            if FileManager.default.fileExists(atPath: candidate) {
                return candidate
            }
        }
        return nil
    }

    private var process: EngineProcess!
    private var client: EngineClient!

    override func setUpWithError() throws {
        guard let path = Self.serverBinaryPath() else {
            throw XCTSkip("karukan-imserver not built")
        }
        process = EngineProcess(serverPath: path)
        client = EngineClient(serverProcess: process, autoInit: false)
        process.start()
        client.startReaderLoop()
    }

    override func tearDown() {
        process?.stop()
    }

    func testStatusRoundTrip() throws {
        let data = client.sendRequestSync(method: "status", params: [:], timeout: 5.0)
        let json = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: XCTUnwrap(data)) as? [String: Any])
        XCTAssertEqual(json["initialized"] as? Bool, false)
        XCTAssertEqual(json["state"] as? String, "empty")
    }

    func testEscapeInEmptyStateNotConsumed() throws {
        let key = EngineKeyEvent(keysym: 0xff1b, modifiers: KeyModifiers())
        let result = try XCTUnwrap(client.processKeySync(key))
        XCTAssertFalse(result.consumed)
    }

    func testUnknownMethodReturnsNil() {
        let data = client.sendRequestSync(method: "no_such_method", params: [:], timeout: 5.0)
        XCTAssertNil(data)
    }

    func testManySequentialRequests() throws {
        // The reader loop must keep request/response pairing intact.
        for _ in 0..<50 {
            let data = client.sendRequestSync(method: "status", params: [:], timeout: 5.0)
            XCTAssertNotNil(data)
        }
    }

    func testWakeProbeKeepsHealthyProcess() throws {
        let unexpectedRestart = expectation(description: "healthy child must not restart")
        unexpectedRestart.isInverted = true
        process.onWillRestart = {
            unexpectedRestart.fulfill()
        }

        client.verifyConnectionAfterWake()
        wait(for: [unexpectedRestart], timeout: 0.4)
        XCTAssertNotNil(client.sendRequestSync(method: "status", params: [:], timeout: 1.0))
    }

    func testSynchronousTimeoutTriggersRecovery() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(
            "karukan-silent-server-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(
            at: directory, withIntermediateDirectories: true)
        let script = directory.appendingPathComponent("silent-server")
        try "#!/bin/sh\nsleep 10\n".write(to: script, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes(
            [.posixPermissions: 0o700], ofItemAtPath: script.path)

        let silentProcess = EngineProcess(serverPath: script.path)
        defer {
            silentProcess.stop()
            try? FileManager.default.removeItem(at: directory)
        }
        let recovered = expectation(description: "timeout recovery invoked")
        let silentClient = EngineClient(
            serverProcess: silentProcess,
            autoInit: false,
            timeoutRecovery: { recovered.fulfill() }
        )
        XCTAssertTrue(silentProcess.start())
        silentClient.startReaderLoop()

        let data = silentClient.sendRequestSync(
            method: "status", params: [:], timeout: 0.05)
        XCTAssertNil(data)
        wait(for: [recovered], timeout: 0.5)
    }

    func testServerStopAndRestartRecovers() throws {
        // restart() waits for the old process off the main thread and
        // completes via onRestart on the main queue; wait(for:) pumps the
        // run loop so that completion can fire.
        let restarted = expectation(description: "server restarted")
        let prepared = expectation(description: "frontend prepared before restart")
        process.onWillRestart = {
            prepared.fulfill()
        }
        let previousOnRestart = process.onRestart
        process.onRestart = {
            previousOnRestart?()
            restarted.fulfill()
        }
        process.restart()
        wait(for: [prepared, restarted], timeout: 5.0)
        let data = client.sendRequestSync(method: "status", params: [:], timeout: 5.0)
        XCTAssertNotNil(data)
    }

    func testConcurrentRequestsAcrossRestartDoNotPoisonNewConnection() throws {
        for iteration in 0..<5 {
            let completions = expectation(
                description: "generation \(iteration) requests completed")
            completions.expectedFulfillmentCount = 20
            DispatchQueue.concurrentPerform(iterations: 20) { _ in
                client.sendRequest(method: "status", params: [:]) { _ in
                    completions.fulfill()
                }
            }

            let restarted = expectation(description: "generation \(iteration) restarted")
            let previousOnRestart = process.onRestart
            process.onRestart = {
                previousOnRestart?()
                restarted.fulfill()
            }
            process.restart()
            wait(for: [completions, restarted], timeout: 5.0)
            process.onRestart = previousOnRestart

            XCTAssertNotNil(
                client.sendRequestSync(method: "status", params: [:], timeout: 1.0),
                "new reader generation must remain usable after iteration \(iteration)"
            )
        }
    }
}

final class RestartPreeditTests: XCTestCase {
    func testFallbackCommitRemovesInternalSegmentSeparators() {
        XCTAssertEqual(visiblePreeditCommitText("今日\u{200B}は\u{200B}晴れ"), "今日は晴れ")
        XCTAssertEqual(visiblePreeditCommitText("通常の未確定文字列"), "通常の未確定文字列")
    }
}

final class EngineProcessFailureTests: XCTestCase {
    func testMissingServerDoesNotPublishPipesAndCanStopPendingRetry() {
        let process = EngineProcess(serverPath: "/path/that/does/not/exist/karukan-imserver")

        XCTAssertFalse(process.start())
        XCTAssertNil(process.stdinPipe)
        XCTAssertNil(process.stdoutPipe)
        process.stop()
    }
}
