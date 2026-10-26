/*
 * Rufus for macOS - main window
 * Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 */
import SwiftUI
import UniformTypeIdentifiers

// MARK: - Materials and window chrome

private struct VisualEffect: NSViewRepresentable {
    var material: NSVisualEffectView.Material
    func makeNSView(context: Context) -> NSVisualEffectView {
        let v = NSVisualEffectView()
        v.material = material; v.blendingMode = .behindWindow; v.state = .active
        return v
    }
    func updateNSView(_ v: NSVisualEffectView, context: Context) { v.material = material }
}

/// Lets the content run under a transparent title bar and the window be dragged from anywhere.
private struct WindowConfigurator: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView {
        let v = NSView()
        DispatchQueue.main.async {
            guard let w = v.window else { return }
            w.titlebarAppearsTransparent = true
            w.titleVisibility = .hidden
            w.styleMask.insert(.fullSizeContentView)
            w.isMovableByWindowBackground = true
            w.standardWindowButton(.zoomButton)?.isEnabled = false
        }
        return v
    }
    func updateNSView(_ v: NSView, context: Context) {}
}

/// Feedback begins on press, not release: a quick, critically damped scale.
private struct PressStyle: ButtonStyle {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(configuration.isPressed && !reduceMotion ? 0.97 : 1)
            .opacity(configuration.isPressed ? 0.85 : 1)
            .animation(.spring(response: 0.25, dampingFraction: 1), value: configuration.isPressed)
    }
}

private struct Card: ViewModifier {
    func body(content: Content) -> some View {
        content
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: 16, style: .continuous)
                    .strokeBorder(LinearGradient(colors: [.white.opacity(0.22), .white.opacity(0.04)],
                                                 startPoint: .top, endPoint: .bottom), lineWidth: 0.8)
            )
            .shadow(color: .black.opacity(0.12), radius: 12, y: 4)
    }
}
private extension View { func card() -> some View { modifier(Card()) } }

// MARK: - Main view

struct ContentView: View {
    @StateObject private var vm = ViewModel()
    @State private var showLog = false
    @State private var dropTargeted = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var spring: Animation? {
        reduceMotion ? .easeInOut(duration: 0.15) : .spring(response: 0.38, dampingFraction: 1)
    }

    var body: some View {
        ZStack {
            VisualEffect(material: .underWindowBackground).ignoresSafeArea()
            WindowConfigurator().frame(width: 0, height: 0)

            VStack(spacing: 14) {
                header
                driveCard
                imageCard
                if vm.imageInfo != nil { optionsCard.transition(.opacity.combined(with: .move(edge: .top))) }
                statusCard
                footer
            }
            .padding(.horizontal, 24).padding(.bottom, 22).padding(.top, 14)
        }
        .frame(width: 580)
        .animation(spring, value: vm.imageInfo != nil)
        .animation(spring, value: vm.effectiveMode)
        .onAppear { vm.refreshDrives() }
        .sheet(isPresented: $showLog) { logSheet }
        .alert("Erase \(vm.selectedDriveTitle)?", isPresented: $vm.showConfirm) {
            Button("Cancel", role: .cancel) {}
            Button("Erase and Write", role: .destructive) { vm.start() }
        } message: {
            Text("All data on this drive will be permanently destroyed. This cannot be undone.")
        }
    }

    // MARK: Header

    private var header: some View {
        HStack(spacing: 14) {
            Image(nsImage: NSApp.applicationIconImage)
                .resizable().frame(width: 52, height: 52)
                .shadow(color: .black.opacity(0.2), radius: 6, y: 3)
            VStack(alignment: .leading, spacing: 1) {
                Text("Rufus").font(.system(size: 28, weight: .semibold)).tracking(-0.5)
                Text("Create bootable USB drives").font(.system(size: 13)).foregroundStyle(.secondary)
            }
            Spacer()
        }
    }

    // MARK: Drive

    private var driveCard: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                label("Drive")
                Spacer()
                Toggle("Show hard drives", isOn: $vm.includeFixed)
                    .toggleStyle(.checkbox).font(.caption).disabled(vm.running)
                Button { vm.refreshDrives() } label: { Image(systemName: "arrow.clockwise") }
                    .buttonStyle(PressStyle()).help("Refresh").disabled(vm.running)
            }
            if vm.drives.isEmpty {
                HStack(spacing: 10) {
                    Image(systemName: "externaldrive").font(.system(size: 22)).foregroundStyle(.tertiary)
                    VStack(alignment: .leading, spacing: 2) {
                        Text("No removable drive found").font(.system(size: 13, weight: .medium))
                        Text("Plug in a USB drive or SD card.").font(.caption).foregroundStyle(.secondary)
                    }
                }
                .frame(maxWidth: .infinity, minHeight: 54, alignment: .leading)
            } else {
                ScrollView {
                    Text(vm.log).font(.system(size: 11, design: .monospaced))
                        .frame(maxWidth: .infinity, alignment: .leading).textSelection(.enabled)
                }.frame(height: 130)
            }
            HStack {
                Button("Checksums") { vm.computeChecksums(); showLog = true }
                    .disabled(vm.running || vm.imageInfo == nil)
                Spacer()
                if vm.writing { Button("CANCEL") { vm.cancel() } }
                Button("START") { vm.showConfirm = true }
                    .keyboardShortcut(.defaultAction).disabled(!vm.canStart)
            }
        }
        .padding(18)
        .frame(width: 520)
        .onAppear { vm.refreshDrives() }
        .onDrop(of: [.fileURL], isTargeted: nil) { providers in
            guard let p = providers.first else { return false }
            _ = p.loadObject(ofClass: URL.self) { url, _ in
                if let url { DispatchQueue.main.async { vm.imagePath = url.path } }
            }
            return true
        }
        .alert("WARNING: ALL DATA ON DEVICE WILL BE DESTROYED", isPresented: $vm.showConfirm) {
            Button("Cancel", role: .cancel) {}
            Button("OK", role: .destructive) { vm.start() }
        } message: {
            Text("To continue with this operation, click OK. To quit, click Cancel.\n\n\(vm.selectedDriveTitle)")
        }
    }

    private func chooseImage() {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = false
        panel.canChooseDirectories = false
        panel.allowedContentTypes = ["iso", "img", "dd", "raw", "bin"].compactMap { UTType(filenameExtension: $0) } + [.data]
        if panel.runModal() == .OK, let url = panel.url { vm.imagePath = url.path }
    }

    private func section<C: View>(_ title: String, @ViewBuilder _ content: () -> C) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.headline)
            content()
        }
    }

    private func row<C: View>(_ title: String, @ViewBuilder _ content: () -> C) -> some View {
        HStack {
            Text(title).frame(width: 120, alignment: .leading)
            content()
        }
    }
}
