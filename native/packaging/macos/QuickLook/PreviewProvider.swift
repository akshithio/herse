// Herse's Quick Look preview: Finder's space bar on a notebook shows it as
// Herse would, rendered to HTML by the Rust library (crates/quicklook) in a
// few milliseconds, with no Python and no network.

import Cocoa
import QuickLookUI
import UniformTypeIdentifiers

class PreviewProvider: QLPreviewProvider, QLPreviewingController {
    func providePreview(for request: QLFilePreviewRequest) async throws -> QLPreviewReply {
        let path = request.fileURL.path
        var length = 0
        guard let bytes = path.withCString({ herse_ql_render($0, &length) }) else {
            throw CocoaError(.fileReadCorruptFile)
        }
        let page = Data(bytes: bytes, count: length)
        herse_ql_free(bytes, length)
        return QLPreviewReply(dataOfContentType: .html, contentSize: CGSize(width: 900, height: 1100)) { reply in
            reply.stringEncoding = .utf8
            return page
        }
    }
}
