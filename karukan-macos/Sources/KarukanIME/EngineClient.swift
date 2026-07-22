import Foundation

/// Newline-delimited JSON-RPC 2.0 client for karukan-imserver.
///
/// Requests are written to the child's stdin; a dedicated reader queue
/// splits stdout on 0x0A and dispatches responses to pending completions.
/// Key processing uses the synchronous API (the IMK `handle` callback must
/// answer "consumed?" synchronously, the same trade-off Mozc makes); slow
/// or fire-and-forget calls use the async API.
class EngineClient {
    private let serverProcess: EngineProcess
    private let timeoutRecovery: () -> Void
    private var nextID = 1
    private let requestQueue = DispatchQueue(label: "dev.togatoga.karukan.jsonrpc.request")

    private let lock = NSLock()
    private struct PendingRequest {
        let readerGeneration: Int
        let completion: (Data?) -> Void
    }
    private var readerGeneration = 0
    /// stdin paired with `readerGeneration`. Both are replaced under
    /// `lock`, so a request can never bind an old pipe to a new reader.
    private var currentStdin: Pipe?
    private var pendingRequests: [Int: PendingRequest] = [:]

    /// `autoInit` re-sends `init` whenever the server (re)starts. Tests
    /// disable it to avoid loading models.
    init(
        serverProcess: EngineProcess,
        autoInit: Bool = true,
        timeoutRecovery: (() -> Void)? = nil
    ) {
        self.serverProcess = serverProcess
        self.timeoutRecovery = timeoutRecovery ?? {
            // A timed-out request may still mutate the child engine after
            // the frontend has let the key pass through. Replacing that
            // process is the only reliable way to prevent split state.
            if Thread.isMainThread {
                serverProcess.restart()
            } else {
                DispatchQueue.main.async { serverProcess.restart() }
            }
        }
        self.serverProcess.onRestart = { [weak self] in
            self?.startReaderLoop()
            if autoInit {
                self?.initAsync()
            }
        }
        self.serverProcess.onConnectionInvalidated = { [weak self] in
            self?.invalidateCurrentConnection()
        }
    }

    // MARK: - Engine methods

    func initAsync() {
        sendRequest(method: "init", params: [:]) { [weak self] data in
            guard let self else { return }
            guard let data,
                let result = try? makeProtocolDecoder().decode(InitResult.self, from: data)
            else {
                NSLog("KarukanIME: engine init failed")
                return
            }
            guard supportsEngineProtocol(result.protocolVersion) else {
                NSLog(
                    "KarukanIME: incompatible engine protocol v\(result.protocolVersion); expected v\(supportedEngineProtocolVersion)"
                )
                // Do not continue exchanging a wire format whose action or
                // position semantics may be incompatible with this frontend.
                self.serverProcess.stop()
                return
            }
            self.serverProcess.resetBackoff()
            NSLog(
                "KarukanIME: engine initialized (protocol v\(result.protocolVersion), model=\(result.modelName))"
            )
        }
    }

    func processKeySync(_ key: EngineKeyEvent) -> KeyResult? {
        let params: [String: Any] = [
            "keysym": key.keysym,
            "modifiers": key.modifiers.jsonObject,
            "is_release": false,
        ]
        // Normal typing is rule-based; cold dictionaries/models initialize
        // independently in the server. Keep a finite allowance for explicit
        // Space conversion while bounding the freeze from a wedged child.
        return keyResultSync(method: "process_key", params: params, timeout: 1.5)
    }

    func refreshLiveConversionAsync(completion: @escaping (KeyResult?) -> Void) {
        sendRequest(method: "refresh_live_conversion", params: [:]) { data in
            guard let data else {
                completion(nil)
                return
            }
            do {
                completion(try makeProtocolDecoder().decode(KeyResult.self, from: data))
            } catch {
                NSLog("KarukanIME: failed to decode refresh_live_conversion result: \(error)")
                completion(nil)
            }
        }
    }

    func commitSync() -> KeyResult? {
        keyResultSync(method: "commit", params: [:], timeout: 1.0)
    }

    func selectCandidateSync(pageIndex: Int) -> KeyResult? {
        keyResultSync(
            method: "select_candidate",
            params: ["page_index": pageIndex],
            timeout: 1.0
        )
    }

    func saveLearningAsync() {
        sendRequest(method: "save_learning", params: [:]) { _ in }
    }

    func resetAsync() {
        sendRequest(method: "reset", params: [:]) { _ in }
    }

    /// Pipes usually survive sleep. Probe the existing child first so a
    /// normal wake does not reload both models and stall the first key for
    /// roughly half a second. Replace the child only when the round trip
    /// actually fails.
    func verifyConnectionAfterWake() {
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            guard let self else { return }
            let healthy = self.sendRequestSync(
                method: "status",
                params: [:],
                timeout: 0.25,
                recoverOnTimeout: false
            ) != nil
            if !healthy {
                DispatchQueue.main.async { [weak self] in
                    self?.serverProcess.restart()
                }
            }
        }
    }

    func setSurroundingTextAsync(text: String, cursorPos: Int) {
        sendRequest(
            method: "set_surrounding_text",
            params: ["text": text, "cursor_pos": cursorPos]
        ) { _ in }
    }

    private func keyResultSync(method: String, params: [String: Any], timeout: TimeInterval)
        -> KeyResult?
    {
        guard let data = sendRequestSync(method: method, params: params, timeout: timeout) else {
            return nil
        }
        do {
            return try makeProtocolDecoder().decode(KeyResult.self, from: data)
        } catch {
            NSLog("KarukanIME: failed to decode \(method) result: \(error)")
            return nil
        }
    }

    // MARK: - JSON-RPC transport

    func startReaderLoop() {
        guard
            let stdout = serverProcess.stdoutPipe,
            let stdin = serverProcess.stdinPipe
        else { return }
        lock.lock()
        readerGeneration &+= 1
        let generation = readerGeneration
        currentStdin = stdin
        lock.unlock()

        let queue = DispatchQueue(
            label: "dev.togatoga.karukan.jsonrpc.reader.\(generation)")
        queue.async { [weak self] in
            let handle = stdout.fileHandleForReading
            var buffer = Data()

            while true {
                let chunk = handle.availableData
                if chunk.isEmpty {
                    // EOF: server terminated
                    self?.failPending(readerGeneration: generation)
                    break
                }
                buffer.append(chunk)

                while let newlineRange = buffer.range(of: Data([0x0A])) {
                    let lineData = buffer.subdata(in: buffer.startIndex..<newlineRange.lowerBound)
                    buffer.removeSubrange(buffer.startIndex...newlineRange.lowerBound)
                    guard !lineData.isEmpty else { continue }
                    self?.handleResponse(lineData)
                }
            }
        }
    }

    @discardableResult
    func sendRequest(
        method: String, params: [String: Any], completion: @escaping (Data?) -> Void
    ) -> Int {
        lock.lock()
        let id = nextID
        nextID += 1
        // Bind the pipe and reader generation atomically. Capturing stdin
        // before taking this lock allowed startReaderLoop() to install a new
        // generation in between, leaving an old-pipe request tagged as new.
        let stdin = currentStdin
        let generation = readerGeneration
        pendingRequests[id] = PendingRequest(
            readerGeneration: generation,
            completion: completion
        )
        lock.unlock()

        let request: [String: Any] = [
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        ]

        requestQueue.async { [weak self] in
            guard let self,
                let stdin,
                self.requestIsCurrent(id: id, readerGeneration: generation),
                var data = try? JSONSerialization.data(withJSONObject: request)
            else {
                self?.takePending(id: id)?(nil)
                return
            }
            data.append(0x0A)
            do {
                try stdin.fileHandleForWriting.write(contentsOf: data)
            } catch {
                NSLog("KarukanIME: failed to write request: \(error)")
                self.takePending(id: id)?(nil)
            }
        }
        return id
    }

    func sendRequestSync(
        method: String,
        params: [String: Any],
        timeout: TimeInterval,
        recoverOnTimeout: Bool = true
    ) -> Data? {
        let semaphore = DispatchSemaphore(value: 0)
        var result: Data?
        let id = sendRequest(method: method, params: params) { data in
            result = data
            semaphore.signal()
        }
        if semaphore.wait(timeout: .now() + timeout) == .timedOut {
            NSLog("KarukanIME: \(method) timed out after \(timeout)s")
            takePending(id: id)?(nil)
            if recoverOnTimeout {
                timeoutRecovery()
            }
            return nil
        }
        return result
    }

    private func handleResponse(_ lineData: Data) {
        guard
            let json = try? JSONSerialization.jsonObject(with: lineData) as? [String: Any]
        else {
            NSLog("KarukanIME: unparsable response line")
            return
        }
        guard let id = json["id"] as? Int else {
            // id:null happens only for parse errors on our side; log and drop.
            NSLog("KarukanIME: response without id: \(json)")
            return
        }
        if let error = json["error"] as? [String: Any] {
            NSLog("KarukanIME: engine error for request \(id): \(error)")
            takePending(id: id)?(nil)
            return
        }
        guard let result = json["result"],
            let data = try? JSONSerialization.data(withJSONObject: result)
        else {
            takePending(id: id)?(nil)
            return
        }
        takePending(id: id)?(data)
    }

    private func takePending(id: Int) -> ((Data?) -> Void)? {
        lock.lock()
        defer { lock.unlock() }
        return pendingRequests.removeValue(forKey: id)?.completion
    }

    private func requestIsCurrent(id: Int, readerGeneration: Int) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return self.readerGeneration == readerGeneration
            && pendingRequests[id]?.readerGeneration == readerGeneration
            && currentStdin != nil
    }

    private func invalidateCurrentConnection() {
        lock.lock()
        let generation = readerGeneration
        currentStdin = nil
        lock.unlock()
        failPending(readerGeneration: generation)
        // Let a write that passed requestIsCurrent just before invalidation
        // finish before EngineProcess closes the pipe. Queued requests now
        // see currentStdin == nil and complete without writing.
        requestQueue.sync {}
    }

    private func failPending(readerGeneration: Int) {
        lock.lock()
        let matching = pendingRequests.filter { $0.value.readerGeneration == readerGeneration }
        for id in matching.keys {
            pendingRequests.removeValue(forKey: id)
        }
        if self.readerGeneration == readerGeneration {
            currentStdin = nil
        }
        lock.unlock()
        for request in matching.values {
            request.completion(nil)
        }
    }
}
