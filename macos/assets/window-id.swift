// Prints the CGWindowID of the first on-screen window owned by the given process name.
import CoreGraphics
import Foundation
let owner = CommandLine.arguments.dropFirst().first ?? "NdagaFijo"
let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
