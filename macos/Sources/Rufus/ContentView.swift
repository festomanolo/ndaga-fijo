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
                    VStack(spacing: 6) {
                        ForEach(vm.drives) { driveRow($0) }
                    }
                }.frame(maxHeight: 132)
            }
        }
        .card()
        .animation(spring, value: vm.drives)
    }

    private func driveRow(_ d: Drive) -> some View {
        let selected = vm.selectedDrive == d.id
        return Button { vm.selectedDrive = d.id } label: {
            HStack(spacing: 12) {
                Image(systemName: "externaldrive.fill")
                    .font(.system(size: 20)).foregroundStyle(selected ? Color.accentColor : .secondary)
                    .frame(width: 28)
                VStack(alignment: .leading, spacing: 1) {
                    Text(d.name.isEmpty ? "USB Drive" : d.name).font(.system(size: 13, weight: .medium))
                    Text("\(ByteCountFormatter.string(fromByteCount: Int64(d.size), countStyle: .file)) · \(d.id)")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Spacer()
                if selected { Image(systemName: "checkmark.circle.fill").foregroundStyle(Color.accentColor).transition(.scale.combined(with: .opacity)) }
            }
            .padding(.horizontal, 12).padding(.vertical, 8)
            .contentShape(Rectangle())
            .background(RoundedRectangle(cornerRadius: 10, style: .continuous)
                .fill(selected ? Color.accentColor.opacity(0.14) : Color.primary.opacity(0.04)))
            .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous)
                .strokeBorder(selected ? Color.accentColor.opacity(0.6) : .clear, lineWidth: 1.2))
        }
        .buttonStyle(PressStyle())
        .disabled(vm.running)
        .animation(spring, value: selected)
    }

    // MARK: Image

    private var imageCard: some View {
        VStack(alignment: .leading, spacing: 10) {
            label("Boot image")
            Button { chooseImage() } label: {
                HStack(spacing: 12) {
                    Image(systemName: vm.imageInfo == nil ? "arrow.down.doc" : "opticaldisc.fill")
                        .font(.system(size: 22)).foregroundStyle(vm.imageInfo == nil ? .secondary : Color.accentColor)
                        .frame(width: 28)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(vm.imagePath.isEmpty ? "Drop an ISO or disk image here" : (vm.imagePath as NSString).lastPathComponent)
                            .font(.system(size: 13, weight: .medium)).lineLimit(1).truncationMode(.middle)
                        Text(vm.imageInfo?.summary ?? "or click to choose a file")
                            .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                    }
                    Spacer()
                    Text(vm.imageInfo == nil ? "Select" : "Change")
                        .font(.system(size: 12, weight: .medium)).foregroundStyle(Color.accentColor)
                }
                .padding(12)
                .frame(maxWidth: .infinity)
                .contentShape(Rectangle())
                .background(RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .fill(dropTargeted ? Color.accentColor.opacity(0.12) : Color.primary.opacity(0.04)))
                .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .strokeBorder(dropTargeted ? Color.accentColor : Color.primary.opacity(0.18),
                                  style: StrokeStyle(lineWidth: dropTargeted ? 1.6 : 1, dash: vm.imageInfo == nil ? [5, 4] : []))
                )
                .scaleEffect(dropTargeted && !reduceMotion ? 1.015 : 1)
            }
            .buttonStyle(PressStyle()).disabled(vm.running)
            .animation(spring, value: dropTargeted)
            if let w = vm.modeWarning {
                Label(w, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption).foregroundStyle(.orange).fixedSize(horizontal: false, vertical: true)
            }
        }
        .card()
        .onDrop(of: [.fileURL], isTargeted: $dropTargeted) { providers in
            guard let p = providers.first, !vm.running else { return false }
            _ = p.loadObject(ofClass: URL.self) { url, _ in
                if let url { DispatchQueue.main.async { vm.imagePath = url.path } }
            }
            return true
        }
    }

    // MARK: Options

    private var optionsCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            label("Options")
            optionRow("Write mode") {
                Picker("", selection: $vm.mode) {
                    ForEach(WriteMode.allCases) { Text($0.title).tag($0) }
                }.labelsHidden().disabled(vm.running)
            }
            if vm.effectiveMode == .extract {
                optionRow("Partition scheme") {
                    Picker("", selection: $vm.scheme) {
                        ForEach(PartitionScheme.allCases) { Text($0.rawValue).tag($0) }
                    }.pickerStyle(.segmented).labelsHidden().disabled(vm.running)
                }
                optionRow("File system") {
                    Picker("", selection: $vm.fileSystem) {
                        ForEach(FileSystemChoice.allCases) { Text($0.rawValue).tag($0) }
                    }.pickerStyle(.segmented).labelsHidden().disabled(vm.running)
                }
                optionRow("Volume label") {
                    TextField("RUFUS", text: $vm.label).textFieldStyle(.roundedBorder).disabled(vm.running)
                }
                optionRow("Boots via") {
                    Text(vm.scheme.target).font(.system(size: 12)).foregroundStyle(.secondary)
                }
            } else {
                optionRow("Verify") {
                    Toggle("Read back and compare after writing", isOn: $vm.verify)
                        .toggleStyle(.checkbox).disabled(vm.running)
                }
                Text("The image is written exactly as-is, so it decides the partitioning and boot method.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
        .card()
    }

    // MARK: Status

    private var statusCard: some View {
        VStack(spacing: 10) {
            HStack(spacing: 8) {
                statusIcon
                Text(statusTitle).font(.system(size: 13, weight: .medium)).lineLimit(1)
                Spacer()
                Text("\(Int(vm.progress * 100))%")
                    .font(.system(size: 13, weight: .medium).monospacedDigit())
                    .foregroundStyle(.secondary).contentTransition(.numericText())
            }
            GeometryReader { geo in
                ZStack(alignment: .leading) {
                    Capsule().fill(Color.primary.opacity(0.1))
                    Capsule()
                        .fill(LinearGradient(colors: [barColor.opacity(0.85), barColor], startPoint: .leading, endPoint: .trailing))
                        .frame(width: max(8, geo.size.width * vm.progress))
                        .opacity(vm.progress > 0 || vm.outcome != .idle ? 1 : 0)
                }
            }
            .frame(height: 8)
            .animation(spring, value: vm.progress)
        }
        .card()
        .animation(spring, value: vm.outcome)
    }

    private var barColor: Color {
        switch vm.outcome { case .failure: return .red; case .success: return .green; case .idle: return .accentColor }
    }

    @ViewBuilder private var statusIcon: some View {
        switch vm.outcome {
        case .success: Image(systemName: "checkmark.circle.fill").foregroundStyle(.green).transition(.scale.combined(with: .opacity))
        case .failure: Image(systemName: "xmark.octagon.fill").foregroundStyle(.red).transition(.scale.combined(with: .opacity))
        case .idle:
            if vm.running { ProgressView().controlSize(.small) }
            else { Image(systemName: "circle.dotted").foregroundStyle(.secondary) }
        }
    }

    private var statusTitle: String {
        switch vm.outcome {
        case .success: return "Done. The drive is ready."
        case .failure: return "Something went wrong. Open the log for details."
        case .idle: return vm.running ? vm.status.capitalized : (vm.canStart ? "Ready" : "Choose a drive and an image")
        }
    }

    // MARK: Footer

    private var footer: some View {
        HStack(spacing: 10) {
            Button { showLog = true } label: { Label("Log", systemImage: "text.alignleft") }
                .buttonStyle(PressStyle())
            Button { vm.computeChecksums(); showLog = true } label: { Label("Checksums", systemImage: "number") }
                .buttonStyle(PressStyle()).disabled(vm.running || vm.imageInfo == nil)
            Spacer()
            if vm.writing {
                Button("Cancel") { vm.cancel() }.buttonStyle(PressStyle()).controlSize(.large)
            }
            Button { vm.showConfirm = true } label: {
                Text(vm.writing ? "Writing…" : "Start")
                    .font(.system(size: 15, weight: .semibold))
                    .frame(width: 130, height: 38)
                    .foregroundStyle(.white)
                    .background(Capsule().fill(LinearGradient(colors: [Color.accentColor.opacity(0.85), Color.accentColor],
                                                              startPoint: .top, endPoint: .bottom)))
                    .shadow(color: Color.accentColor.opacity(0.35), radius: 8, y: 3)
            }
            .buttonStyle(PressStyle())
            .keyboardShortcut(.defaultAction)
            .disabled(!vm.canStart)
            .opacity(vm.canStart ? 1 : 0.4)
            .animation(spring, value: vm.canStart)
        }
        .font(.system(size: 12))
    }

    // MARK: Log

    private var logSheet: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Log").font(.system(size: 17, weight: .semibold))
                Spacer()
                Button("Copy") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(vm.log, forType: .string)
                }
                Button("Done") { showLog = false }.keyboardShortcut(.defaultAction)
            }
            ScrollView {
                Text(vm.log.isEmpty ? "Nothing yet." : vm.log)
                    .font(.system(size: 11, design: .monospaced))
                    .frame(maxWidth: .infinity, alignment: .leading).textSelection(.enabled).padding(10)
            }
            .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        }
        .padding(20).frame(width: 540, height: 340)
    }

    // MARK: Helpers

    private func chooseImage() {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = false
        panel.canChooseDirectories = false
        panel.allowedContentTypes = ["iso", "img", "dd", "raw", "bin"].compactMap { UTType(filenameExtension: $0) } + [.data]
        if panel.runModal() == .OK, let url = panel.url { vm.imagePath = url.path }
    }

    private func label(_ s: String) -> some View {
        Text(s.uppercased()).font(.system(size: 11, weight: .semibold)).tracking(0.6).foregroundStyle(.secondary)
    }

    private func optionRow<C: View>(_ title: String, @ViewBuilder _ content: () -> C) -> some View {
        HStack {
            Text(title).font(.system(size: 13)).frame(width: 120, alignment: .leading)
            content()
            Spacer(minLength: 0)
        }
    }
}
