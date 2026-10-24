import CoreGraphics
import Foundation
let list = CGWindowListCopyWindowInfo([.optionAll], kCGNullWindowID) as? [[String: Any]] ?? []
for w in list where (w[kCGWindowOwnerName as String] as? String) == (CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "herse") {
    print(w[kCGWindowNumber as String] ?? "", w[kCGWindowLayer as String] ?? "", w[kCGWindowBounds as String] ?? "", w[kCGWindowIsOnscreen as String] ?? "", w[kCGWindowName as String] ?? "")
}
