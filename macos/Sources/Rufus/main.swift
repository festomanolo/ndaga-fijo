/*
 * Rufus for macOS - entry point
 * Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 */
import SwiftUI

if CommandLine.arguments.contains("--helper") {
    exit(Helper.main(Array(CommandLine.arguments.dropFirst())))
} else if CommandLine.arguments.contains("--list-drives") {
    for d in DiskService.listDrives(includeFixed: true) { print("\(d.id)\t\(d.size)\t\(d.title)") }
    exit(0)
} else if CommandLine.arguments.count == 5, CommandLine.arguments[1] == "--split-wim" {
    // Developer tool: Rufus --split-wim <install.wim> <outdir> <max part bytes>
    do {
        let parts = try WimSplit.split(source: CommandLine.arguments[2], destDir: CommandLine.arguments[3],
                                       maxPart: UInt64(CommandLine.arguments[4]) ?? 0, onBytes: { _ in }, cancelled: { false })
        parts.forEach { print($0) }
        exit(0)
    } catch let e as WimSplit.Failure { print("error: \(e.message)"); exit(1) }
} else {
    RufusApp.main()
}
