/*
 * Rufus for macOS - view model
 * Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 */
import Foundation
import SwiftUI

@MainActor
final class ViewModel: ObservableObject {
    @Published var drives: [Drive] = []
    @Published var selectedDrive: String = ""
    @Published var imagePath: String = "" { didSet { imageChanged() } }
    @Published var imageInfo: ImageInfo?
    @Published var scheme: PartitionScheme = .mbr
    @Published var fileSystem: FileSystemChoice = .fat32
    @Published var label: String = "RUFUS"
    @Published var mode: WriteMode = .auto
    @Published var verify = false

    @Published var progress: Double = 0
    @Published var status: String = "READY"
    @Published var running = false
    @Published var writing = false          // helper job active (cancellable)
    @Published var includeFixed = false { didSet { refreshDrives() } }
    @Published var log: String = ""
    @Published var showConfirm = false
    @Published var outcome: Outcome = .idle
    enum Outcome { case idle, success, failure }

    private var progressFile = ""
    private var cancelFile = ""

    var effectiveMode: WriteMode { mode == .auto ? (imageInfo?.recommendedMode ?? .dd) : mode }
    var canStart: Bool { !running && !selectedDrive.isEmpty && imageInfo != nil }
    var selectedDriveTitle: String { drives.first { $0.id == selectedDrive }?.title ?? selectedDrive }

    var modeWarning: String? {
        guard let info = imageInfo else { return nil }
        if effectiveMode == .dd && info.isOptical && !info.isHybrid {
            return "This ISO is not hybrid; writing it in DD mode will not produce a bootable drive."
        }
        if effectiveMode == .extract && !info.isOptical { return "Only ISO images can be extracted." }
        if effectiveMode == .extract && fileSystem == .fat32 && info.size > 4_000_000_000 {
            return "Windows ISOs whose install.wim exceeds 4 GB cannot be written to FAT32 yet (ExFAT works for copying, but most UEFI firmware cannot boot it)."
        }
        return nil
    }

    func appendLog(_ s: String) {
        let f = DateFormatter(); f.dateFormat = "HH:mm:ss"
        log += "[\(f.string(from: Date()))] \(s)\n"
    }

    func refreshDrives() {
        Task {
            let fixed = includeFixed
            let found = await Task.detached { DiskService.listDrives(includeFixed: fixed) }.value
            drives = found
            if !found.contains(where: { $0.id == selectedDrive }) { selectedDrive = found.first?.id ?? "" }
            appendLog("\(found.count) removable drive(s) found")
        }
    }

    private func imageChanged() {
        imageInfo = imagePath.isEmpty ? nil : ImageInfo.inspect(imagePath)
        if let info = imageInfo {
            appendLog("Image: \((imagePath as NSString).lastPathComponent) - \(info.summary)")
            let base = ((imagePath as NSString).lastPathComponent as NSString).deletingPathExtension
            let cleaned = String(base.uppercased().filter { $0.isLetter || $0.isNumber }.prefix(11))
            if !cleaned.isEmpty { label = cleaned }
        } else if !imagePath.isEmpty {
            appendLog("Cannot read \(imagePath)")
        }
    }

    func computeChecksums() {
        let path = imagePath
        guard !path.isEmpty else { return }
        status = "COMPUTING CHECKSUMS"
        running = true
        Task {
            let r = await Task.detached { [weak self] () -> Checksums.Result? in
                Checksums.compute(path: path) { f in Task { @MainActor in self?.progress = f } }
            }.value
            running = false; progress = 0; status = "READY"
            if let r {
                appendLog("MD5:    \(r.md5)\nSHA-1:  \(r.sha1)\nSHA-256: \(r.sha256)\nSHA-512: \(r.sha512)")
            } else { appendLog("Could not read the image") }
        }
    }

    func cancel() {
        FileManager.default.createFile(atPath: cancelFile, contents: Data())
        status = "CANCELLING..."
    }

    // MARK: - Write

    func start() {
        guard canStart, let exe = Bundle.main.executablePath else { return }
        let tmp = NSTemporaryDirectory()
        let tag = UUID().uuidString
        progressFile = tmp + "rufus-\(tag).progress"
        cancelFile = tmp + "rufus-\(tag).cancel"
        FileManager.default.createFile(atPath: progressFile, contents: Data())
        try? FileManager.default.removeItem(atPath: cancelFile)

        let args = ["--helper", "--image", imagePath, "--disk", selectedDrive,
                    "--mode", effectiveMode == .extract ? "extract" : "dd",
                    "--scheme", scheme.rawValue, "--fs", fileSystem.rawValue, "--label", label,
                    "--progress", progressFile, "--cancel", cancelFile, "--verify", verify ? "1" : "0"]
        var cmd = Shell.quote(exe) + " " + args.map(Shell.quote).joined(separator: " ")
        if DiskService.allowDiskImages { cmd = "RUFUS_ALLOW_DISK_IMAGES=1 " + cmd }
        let script = "do shell script \"\(cmd.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\""))\" with administrator privileges"

        running = true; writing = true; outcome = .idle; progress = 0; status = "STARTING..."
        appendLog("Starting \(effectiveMode == .extract ? "ISO" : "DD") write to \(selectedDrive)")
        var finished = false, failed = false
        var offset = 0

        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
        proc.arguments = ["-e", script]
        let errPipe = Pipe()
        proc.standardError = errPipe
        proc.standardOutput = FileHandle.nullDevice
        proc.terminationHandler = { [weak self] p in
            let err = String(decoding: errPipe.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
            Task { @MainActor in
                guard let self else { return }
                self.drain(&offset, &finished, &failed)
                if !finished && !failed {
                    self.finish(error: err.contains("-128") ? "Authorization was cancelled" :
                                (err.isEmpty ? "The write helper exited unexpectedly" : err.trimmingCharacters(in: .whitespacesAndNewlines)))
                } else { self.finish(error: failed ? "" : nil) }
            }
        }
        do { try proc.run() } catch { finish(error: error.localizedDescription); return }

        Task { @MainActor in
            while running {
                drain(&offset, &finished, &failed)
                try? await Task.sleep(nanoseconds: 250_000_000)
            }
        }
    }

    private func drain(_ offset: inout Int, _ finished: inout Bool, _ failed: inout Bool) {
        guard let data = FileManager.default.contents(atPath: progressFile), data.count > offset else { return }
        let new = data.subdata(in: offset..<data.count)
        guard let end = new.lastIndex(of: 0x0A) else { return }
        offset += end + 1
        for line in String(decoding: new[..<(end + 1)], as: UTF8.self).split(separator: "\n") {
            if line.hasPrefix("P ") {
                let parts = line.dropFirst(2).split(separator: " ", maxSplits: 1)
                if let f = parts.first.flatMap({ Double($0) }) { progress = f }
                if parts.count > 1 { status = String(parts[1]).uppercased() }
            } else if line.hasPrefix("L ") { appendLog(String(line.dropFirst(2))) }
            else if line == "DONE" { finished = true }
            else if line.hasPrefix("ERR ") { failed = true; appendLog("ERROR: " + line.dropFirst(4)); status = "FAILED" }
        }
    }

    private func finish(error: String?) {
        running = false; writing = false
        if let error {
            if !error.isEmpty { appendLog("ERROR: \(error)") }
            status = "FAILED"; outcome = .failure
        } else {
            progress = 1; status = "READY"; outcome = .success
            appendLog("Done. The drive is ready.")
        }
        try? FileManager.default.removeItem(atPath: progressFile)
        try? FileManager.default.removeItem(atPath: cancelFile)
        refreshDrives()
    }
}
