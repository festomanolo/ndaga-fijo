/*
 * Rufus for macOS - application
 * Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 */
import SwiftUI

struct RufusApp: App {
    var body: some Scene {
        WindowGroup("Rufus") {
            ContentView()
        }
        .windowResizability(.contentSize)
    }
}
