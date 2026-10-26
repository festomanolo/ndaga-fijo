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
} else {
    RufusApp.main()
}
