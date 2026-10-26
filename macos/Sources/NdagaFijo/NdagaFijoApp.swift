/*
 * Ndaga Fijo - application
 * Based on Rufus, Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 */
import SwiftUI

struct NdagaFijoApp: App {
    var body: some Scene {
        WindowGroup("Ndaga Fijo") {
            ContentView()
        }
        .windowStyle(.hiddenTitleBar)
        .windowResizability(.contentSize)
    }
}
