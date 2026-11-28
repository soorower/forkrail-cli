// Forkrail.app: the forkrail installer in a window of its own, for people who'd rather not use
// Terminal. It runs the forkrail CLI it carries (Contents/Helpers/forkrail) as `forkrail install`
// with FORKRAIL_PAGE_IN_WINDOW=1, which prints the setup page's address instead of opening a
// browser (crates/cli/src/install/page.rs), and shows that page here. Every other address the page
// opens (AWS's and GitHub's sign-in, Cloudflare, the dashboard at the end) goes to the default
// browser, where passkeys and saved sign-ins work.
//
// The installer's own output goes to ~/Library/Logs/Forkrail/install.log. Closing the window while
// it runs stops it; opening the app again carries on where it stopped, as `forkrail install` does.
//
// Built by deploy/macos/build-app.sh: no Xcode project, just swiftc.

import AppKit
import WebKit

let pageLine = "FORKRAIL_PAGE_URL="
/// The installer prints this when the install has ended, either way; it stays until the page has
/// shown the end (crates/cli/src/install/page.rs), and closing the window then stops nothing.
let endedLine = "FORKRAIL_INSTALL_ENDED"

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate, WKNavigationDelegate, WKUIDelegate {
    private var window: NSWindow!
    private var web: WKWebView!
    private var installer: Process?
    private var log: FileHandle?
    private var buffer = Data()
    private var lastLines: [String] = []
    private var pageShown = false
    /// The setup page's own address (127.0.0.1 and its port): the one address that stays in the window.
    private var page: URL?
    private var stopping = false
    /// The install has ended: the installer only waits for the page to show it.
    private var finished = false

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.mainMenu = mainMenu()
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        web = WKWebView(frame: .zero, configuration: config)
        web.navigationDelegate = self
        web.uiDelegate = self
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1080, height: 800),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Forkrail"
        window.minSize = NSSize(width: 640, height: 520)
        window.contentView = web
        window.delegate = self
        window.center()
        window.setFrameAutosaveName("Forkrail")
        web.loadHTMLString(Self.message("Starting the installer…", detail: ""), baseURL: nil)
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        start()
    }

    // MARK: the installer

    private func start() {
        // Not beside the app's own program in Contents/MacOS: "forkrail" and "Forkrail" are one name
        // on a Mac's disk.
        let cli = Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/forkrail")
        guard FileManager.default.isExecutableFile(atPath: cli.path) else {
            show("Forkrail is incomplete", detail: "This copy of the app has no installer in it. Download Forkrail again.")
            return
        }
        let process = Process()
        process.executableURL = cli
        process.arguments = ["install"]
        var env = ProcessInfo.processInfo.environment
        env["FORKRAIL_PAGE_IN_WINDOW"] = "1"
        // Opened from Finder, the app has the shortest PATH; the installer needs only the system's tools.
        env["PATH"] = (env["PATH"].map { $0 + ":" } ?? "") + "/usr/bin:/bin:/usr/sbin:/sbin"
        process.environment = env
        process.currentDirectoryURL = FileManager.default.homeDirectoryForCurrentUser
        let out = Pipe()
        process.standardOutput = out
        process.standardError = out
        process.standardInput = FileHandle.nullDevice
        openLog()
        out.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            if data.isEmpty {
                handle.readabilityHandler = nil
                return
            }
            DispatchQueue.main.async { self?.received(data) }
        }
        process.terminationHandler = { [weak self] process in
            DispatchQueue.main.async { self?.ended(process.terminationStatus) }
        }
        do {
            try process.run()
            installer = process
        } catch {
            show("The installer couldn't start", detail: error.localizedDescription)
        }
    }

    private func received(_ data: Data) {
        log?.write(data)
        buffer.append(data)
        while let newline = buffer.firstIndex(of: 0x0A) {
            let line = String(decoding: buffer[buffer.startIndex..<newline], as: UTF8.self)
            buffer.removeSubrange(buffer.startIndex...newline)
            handle(line)
        }
    }

    private func handle(_ line: String) {
        if !pageShown, line.hasPrefix(pageLine), let url = URL(string: String(line.dropFirst(pageLine.count))) {
            pageShown = true
            page = url
            web.load(URLRequest(url: url))
            return
        }
        if line == endedLine {
            finished = true
            return
        }
        lastLines.append(line)
        if lastLines.count > 12 { lastLines.removeFirst() }
    }

    private func ended(_ status: Int32) {
        installer = nil
        log?.write(Data("\n== the installer ended (\(status))\n".utf8))
        if stopping { return }
        // The page shows how it ended. Without one, the installer stopped before it could serve it.
        if !pageShown {
            show("The installer stopped", detail: lastLines.joined(separator: "\n"))
        }
    }

    private func stop() {
        stopping = true
        guard let process = installer, process.isRunning else { return }
        process.interrupt()
        let deadline = Date().addingTimeInterval(3)
        while process.isRunning && Date() < deadline {
            RunLoop.current.run(until: Date().addingTimeInterval(0.1))
        }
        if process.isRunning { process.terminate() }
    }

    private func openLog() {
        let dir = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Logs/Forkrail")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let file = dir.appendingPathComponent("install.log")
        if !FileManager.default.fileExists(atPath: file.path) {
            FileManager.default.createFile(atPath: file.path, contents: nil)
        }
        log = try? FileHandle(forWritingTo: file)
        log?.seekToEndOfFile()
        log?.write(Data("\n== Forkrail \(Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") ?? ""), \(Date())\n".utf8))
    }

    // MARK: closing while it runs

    private func mayClose() -> Bool {
        guard let process = installer, process.isRunning, pageShown, !finished else { return true }
        let alert = NSAlert()
        alert.messageText = "The install is still running"
        alert.informativeText = "Closing Forkrail stops it. Open Forkrail again later and it carries on where it stopped; nothing done so far is lost."
        alert.addButton(withTitle: "Keep installing")
        alert.addButton(withTitle: "Stop and close")
        return alert.runModal() == .alertSecondButtonReturn
    }

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        guard mayClose() else { return false }
        stop()
        return true
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard mayClose() else { return .terminateCancel }
        stop()
        return .terminateNow
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    func applicationWillTerminate(_ notification: Notification) {
        if installer?.isRunning == true { installer?.terminate() }
    }

    // MARK: the page

    /// The installer's page stays here; every other address opens in the default browser. That
    /// includes the installer's other local pages (AWS's sign-in and GitHub's app start on
    /// 127.0.0.1 too, on ports of their own): they hand on to AWS and GitHub, which belong in the
    /// browser.
    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction, decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
        guard let url = action.request.url else { return decisionHandler(.cancel) }
        let ours = url.scheme == "about" || (page.map { url.scheme == $0.scheme && url.host == $0.host && url.port == $0.port } ?? false)
        if ours && action.targetFrame != nil {
            return decisionHandler(.allow)
        }
        if url.scheme == "http" || url.scheme == "https" || url.scheme == "mailto" {
            NSWorkspace.shared.open(url)
        }
        decisionHandler(.cancel)
    }

    /// Links that open a new tab (`target="_blank"`) go to the default browser.
    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration, for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        if let url = action.request.url, url.scheme == "http" || url.scheme == "https" {
            NSWorkspace.shared.open(url)
        }
        return nil
    }

    func webView(_ webView: WKWebView, runJavaScriptAlertPanelWithMessage message: String, initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping () -> Void) {
        let alert = NSAlert()
        alert.messageText = message
        alert.runModal()
        completionHandler()
    }

    func webView(_ webView: WKWebView, runJavaScriptConfirmPanelWithMessage message: String, initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping (Bool) -> Void) {
        let alert = NSAlert()
        alert.messageText = message
        alert.addButton(withTitle: "OK")
        alert.addButton(withTitle: "Cancel")
        completionHandler(alert.runModal() == .alertFirstButtonReturn)
    }

    private func show(_ title: String, detail: String) {
        web.loadHTMLString(Self.message(title, detail: detail), baseURL: nil)
    }

    private static func escape(_ text: String) -> String {
        text.replacingOccurrences(of: "&", with: "&amp;").replacingOccurrences(of: "<", with: "&lt;").replacingOccurrences(of: ">", with: "&gt;")
    }

    /// A page of the app's own, in the installer page's light look.
    private static func message(_ title: String, detail: String) -> String {
        let more = detail.isEmpty ? "" : "<pre>\(escape(detail))</pre><p>The whole log is in ~/Library/Logs/Forkrail/install.log.</p>"
        return """
        <!doctype html><html><head><meta charset="utf-8"><style>
        body{margin:0;font:15px/1.5 -apple-system,system-ui,sans-serif;background:#fafafa;color:#17171c;display:flex;min-height:100vh;align-items:center;justify-content:center}
        main{max-width:560px;padding:32px;text-align:center}h1{font-size:22px;font-weight:650;margin:0 0 12px}
        p{color:#62626f;margin:8px 0}pre{text-align:left;white-space:pre-wrap;background:#fff;border:1px solid #e4e4ea;border-radius:10px;padding:12px;font:12px/1.5 ui-monospace,Menlo,monospace;color:#17171c}
        </style></head><body><main><h1>\(escape(title))</h1>\(more)</main></body></html>
        """
    }

    // MARK: menus

    /// The app menu, and Edit so that copy and paste work in the page (the Cloudflare token).
    private func mainMenu() -> NSMenu {
        let menu = NSMenu()
        let appItem = NSMenuItem()
        menu.addItem(appItem)
        let app = NSMenu()
        app.addItem(withTitle: "Hide Forkrail", action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        app.addItem(.separator())
        app.addItem(withTitle: "Quit Forkrail", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        appItem.submenu = app

        let editItem = NSMenuItem()
        menu.addItem(editItem)
        let edit = NSMenu(title: "Edit")
        edit.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
        edit.addItem(withTitle: "Redo", action: Selector(("redo:")), keyEquivalent: "Z")
        edit.addItem(.separator())
        edit.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        edit.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        edit.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        edit.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
        editItem.submenu = edit

        let windowItem = NSMenuItem()
        menu.addItem(windowItem)
        let windows = NSMenu(title: "Window")
        windows.addItem(withTitle: "Minimize", action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        windows.addItem(withTitle: "Close", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        windowItem.submenu = windows
        NSApp.windowsMenu = windows
        return menu
    }
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
