#!/usr/bin/env swift
import Carbon
import Foundation

private let karukanInputSourceID = "dev.togatoga.inputmethod.Karukan.Japanese"
private let inputMethodActivationSettleTime: TimeInterval = 0.5

private func stringProperty(_ source: TISInputSource, _ key: CFString) -> String? {
    guard let raw = TISGetInputSourceProperty(source, key) else { return nil }
    return Unmanaged<CFString>.fromOpaque(raw).takeUnretainedValue() as String
}

private func currentInputSourceID() -> String? {
    guard let source = TISCopyCurrentKeyboardInputSource()?.takeRetainedValue() else {
        return nil
    }
    return stringProperty(source, kTISPropertyInputSourceID)
}

private func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("select_input_source: \(message)\n".utf8))
    exit(1)
}

let arguments = Array(CommandLine.arguments.dropFirst())
if arguments == ["--current"] {
    guard let current = currentInputSourceID() else {
        fail("could not read the current input source")
    }
    print(current)
    exit(0)
}

guard arguments.count <= 1 else {
    fail("usage: select_input_source.swift [--current|INPUT_SOURCE_ID]")
}

let targetID = arguments.first ?? karukanInputSourceID
let filter = [kTISPropertyInputSourceID: targetID as CFString] as CFDictionary
guard let sourceList = TISCreateInputSourceList(filter, false) else {
    fail("input source is not enabled: \(targetID)")
}
let sources = sourceList.takeRetainedValue() as! [TISInputSource]
guard let source = sources.first else {
    fail("input source is not enabled: \(targetID)")
}

let status = TISSelectInputSource(source)
guard status == noErr else {
    fail("TISSelectInputSource failed for \(targetID) with status \(status)")
}

let deadline = Date().addingTimeInterval(1)
repeat {
    if currentInputSourceID() == targetID {
        // TIS reports the new ID before the focused app's InputMethodKit
        // context has necessarily activated it. Returning immediately can
        // leak the first few automated keystrokes through the old source.
        Thread.sleep(forTimeInterval: inputMethodActivationSettleTime)
        if currentInputSourceID() == targetID {
            print(targetID)
            exit(0)
        }
    }
    RunLoop.current.run(until: Date().addingTimeInterval(0.01))
} while Date() < deadline

fail("timed out waiting for input source: \(targetID)")
