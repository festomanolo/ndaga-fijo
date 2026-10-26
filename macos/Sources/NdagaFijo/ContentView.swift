/*
 * Ndaga Fijo - main window
 * Based on Rufus, Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 */
import SwiftUI
import UniformTypeIdentifiers

// MARK: - Liquid Glass helpers (real glass on macOS 26, material fallback before)

private struct GlassSurface: ViewModifier {
    var radius: CGFloat
    var tint: Color?
    var interactive: Bool
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency

    func body(content: Content) -> some View {
        let shape = RoundedRectangle(cornerRadius: radius, style: .continuous)
        if #available(macOS 26, *), !reduceTransparency {
            content.glassEffect(glass, in: shape)
        } else {
            content
                .background(reduceTransparency ? AnyShapeStyle(.background) : AnyShapeStyle(.regularMaterial), in: shape)
                .overlay(shape.strokeBorder(LinearGradient(colors: [.white.opacity(0.5), .white.opacity(0.06)],
                                                           startPoint: .top, endPoint: .bottom), lineWidth: 0.8))
                .shadow(color: .black.opacity(0.10), radius: 14, y: 6)
        }
    }

    @available(macOS 26, *)
    private var glass: Glass {
        var g: Glass = .regular
        if let tint { g = g.tint(tint) }
        return interactive ? g.interactive() : g
    }
}

private extension View {
    func glass(radius: CGFloat = 24, tint: Color? = nil, interactive: Bool = false) -> some View {
        modifier(GlassSurface(radius: radius, tint: tint, interactive: interactive))
    }

    @ViewBuilder func glassContainer(spacing: CGFloat = 20) -> some View {
        if #available(macOS 26, *) { GlassEffectContainer(spacing: spacing) { self } } else { self }
    }

    @ViewBuilder func glassButton(prominent: Bool = false) -> some View {
        if #available(macOS 26, *) {
            if prominent { self.buttonStyle(.glassProminent) } else { self.buttonStyle(.glass) }
        } else {
            if prominent { self.buttonStyle(.borderedProminent) } else { self.buttonStyle(.bordered) }
        }
    }
}

// MARK: - Window chrome

private struct VisualEffect: NSViewRepresentable {
    func makeNSView(context: Context) -> NSVisualEffectView {
        let v = NSVisualEffectView()
        v.material = .underWindowBackground; v.blendingMode = .behindWindow; v.state = .active
        return v
    }
    func updateNSView(_ v: NSVisualEffectView, context: Context) {}
}

/// Content runs under a transparent title bar. The window keeps the system's own corner radius,
/// because nothing here clips or masks the window shape.
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

/// Feedback starts on press: a quick, critically damped scale.
private struct PressStyle: ButtonStyle {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(configuration.isPressed && !reduceMotion ? 0.97 : 1)
            .animation(.spring(response: 0.25, dampingFraction: 1), value: configuration.isPressed)
    }
}

// MARK: - Main view

private enum Stage { case setup, writing, done, failed }

struct ContentView: View {
    @StateObject private var vm = ViewModel()
    @State private var showLog = false
    @State private var showOptions = false
    @State private var dropTargeted = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var spring: Animation? {
        reduceMotion ? .easeInOut(duration: 0.15) : .spring(response: 0.42, dampingFraction: 1)
    }

    private var stage: Stage {
        if vm.writing { return .writing }
        switch vm.outcome { case .success: return .done; case .failure: return .failed; case .idle: return .setup }
    }

    var body: some View {
        ZStack {
            background
            WindowConfigurator().frame(width: 0, height: 0)
            Group {
                switch stage {
                case .setup: setupStage.transition(.opacity.combined(with: .scale(scale: 0.98)))
                case .writing: writingStage.transition(.opacity.combined(with: .scale(scale: 0.98)))
                case .done: doneStage.transition(.opacity.combined(with: .scale(scale: 0.98)))
                case .failed: failedStage.transition(.opacity.combined(with: .scale(scale: 0.98)))
                }
            }
            .padding(.horizontal, 26).padding(.top, 34).padding(.bottom, 24)
        }
        .frame(width: 560)
        .animation(spring, value: stage == .setup)
        .animation(spring, value: stage == .writing)
        .animation(spring, value: stage == .done)
        .onAppear { vm.refreshDrives() }
        .onChange(of: vm.outcome) { new in
            if new == .success { NSHapticFeedbackManager.defaultPerformer.perform(.levelChange, performanceTime: .now) }
        }
        .sheet(isPresented: $showLog) { logSheet }
        .alert("Erase \(vm.selectedDriveTitle)?", isPresented: $vm.showConfirm) {
            Button("Cancel", role: .cancel) {}
            Button("Erase and Write", role: .destructive) { vm.start() }
        } message: {
            Text("Everything on this drive will be permanently erased. This can't be undone.")
        }
    }

    /// Soft colour behind the glass gives it something to refract.
    private var background: some View {
        ZStack {
            VisualEffect().ignoresSafeArea()
            Circle().fill(Color(red: 0.10, green: 0.62, blue: 0.62).opacity(0.38)).frame(width: 340).blur(radius: 90).offset(x: -190, y: -230)
            Circle().fill(Color(red: 0.12, green: 0.25, blue: 0.75).opacity(0.34)).frame(width: 380).blur(radius: 100).offset(x: 220, y: 260)
        }
        .ignoresSafeArea()
    }

    // MARK: Setup

    private var setupStage: some View {
        VStack(spacing: 16) {
            header
            VStack(spacing: 12) {
                imageStep
                driveStep
                optionsStep
            }
            .glassContainer(spacing: 16)
            writeButton
            footerLinks
        }
    }

    private var header: some View {
        VStack(spacing: 8) {
            Image(nsImage: NSApp.applicationIconImage)
                .resizable().frame(width: 76, height: 76)
                .shadow(color: .black.opacity(0.25), radius: 10, y: 5)
            Text("Ndaga Fijo").font(.system(size: 26, weight: .semibold)).tracking(-0.4)
            Text("Create a bootable USB drive").font(.system(size: 13)).foregroundStyle(.secondary)
        }
        .padding(.bottom, 4)
    }

    // Step 1 - image

    private var imageStep: some View {
        VStack(alignment: .leading, spacing: 12) {
            stepHeader(1, "Choose an image", done: vm.imageInfo != nil)
            Button { chooseImage() } label: {
                HStack(spacing: 14) {
                    Image(systemName: vm.imageInfo == nil ? "arrow.down.doc.fill" : "opticaldisc.fill")
                        .font(.system(size: 24, weight: .medium)).foregroundStyle(.white)
                        .frame(width: 46, height: 46)
                        .background(Circle().fill(Color.accentColor.gradient))
                    VStack(alignment: .leading, spacing: 2) {
                        Text(vm.imagePath.isEmpty ? "Drop an ISO or disk image" : (vm.imagePath as NSString).lastPathComponent)
                            .font(.system(size: 14, weight: .semibold)).lineLimit(1).truncationMode(.middle)
                        Text(vm.imageInfo?.summary ?? "or click to browse")
                            .font(.system(size: 12)).foregroundStyle(.secondary).lineLimit(2)
                    }
                    Spacer(minLength: 0)
                    if vm.imageInfo != nil { Text("Change").font(.system(size: 12, weight: .medium)).foregroundStyle(.secondary) }
                }
                .padding(12)
                .frame(maxWidth: .infinity)
                .contentShape(Rectangle())
                .background(
                    RoundedRectangle(cornerRadius: 16, style: .continuous)
                        .strokeBorder(dropTargeted ? Color.white : Color.white.opacity(0.0), lineWidth: 2)
                        .background(RoundedRectangle(cornerRadius: 16, style: .continuous).fill(Color.white.opacity(dropTargeted ? 0.18 : 0.08)))
                )
                .scaleEffect(dropTargeted && !reduceMotion ? 1.02 : 1)
            }
            .buttonStyle(PressStyle())
            .animation(spring, value: dropTargeted)
            if let w = vm.modeWarning {
                Label(w, systemImage: "exclamationmark.triangle.fill")
                    .font(.system(size: 12)).foregroundStyle(.orange).fixedSize(horizontal: false, vertical: true)
            }
            if vm.running && !vm.writing {
                ProgressView(value: vm.progress) { Text(vm.status).font(.system(size: 12)).foregroundStyle(.secondary) }
            }
        }
        .padding(16)
        .glass(radius: 26)
        .onDrop(of: [.fileURL], isTargeted: $dropTargeted) { providers in
            guard let p = providers.first else { return false }
            _ = p.loadObject(ofClass: URL.self) { url, _ in
                if let url { DispatchQueue.main.async { vm.imagePath = url.path } }
            }
            return true
        }
    }

    // Step 2 - drive

    private var driveStep: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                stepHeader(2, "Choose a drive", done: !vm.selectedDrive.isEmpty)
                Spacer()
                Button { vm.refreshDrives() } label: { Image(systemName: "arrow.clockwise") }
                    .buttonStyle(PressStyle()).foregroundStyle(.secondary).help("Refresh")
            }
            if vm.drives.isEmpty {
                HStack(spacing: 12) {
                    Image(systemName: "externaldrive.badge.plus").font(.system(size: 22)).foregroundStyle(.secondary)
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Insert a USB drive").font(.system(size: 14, weight: .semibold))
                        Text("It will appear here automatically.").font(.system(size: 12)).foregroundStyle(.secondary)
                    }
                    Spacer()
                }
                .padding(12)
                .background(RoundedRectangle(cornerRadius: 16, style: .continuous).fill(Color.white.opacity(0.08)))
            } else {
                VStack(spacing: 6) { ForEach(vm.drives) { driveRow($0) } }
            }
            Toggle("Include external hard drives", isOn: $vm.includeFixed)
                .toggleStyle(.checkbox).font(.system(size: 12)).foregroundStyle(.secondary)
        }
        .padding(16)
        .glass(radius: 26)
        .animation(spring, value: vm.drives)
    }

    private func driveRow(_ d: Drive) -> some View {
        let selected = vm.selectedDrive == d.id
        return Button { vm.selectedDrive = d.id } label: {
            HStack(spacing: 12) {
                Image(systemName: "externaldrive.fill").font(.system(size: 18))
                    .foregroundStyle(selected ? .white : .secondary).frame(width: 26)
                VStack(alignment: .leading, spacing: 1) {
                    Text(d.name.isEmpty ? "USB Drive" : d.name).font(.system(size: 14, weight: .semibold))
                    Text("\(ByteCountFormatter.string(fromByteCount: Int64(d.size), countStyle: .file)) · \(d.id)")
                        .font(.system(size: 12)).foregroundStyle(selected ? Color.white.opacity(0.8) : .secondary)
                }
                Spacer()
                if selected { Image(systemName: "checkmark.circle.fill").foregroundStyle(.white).transition(.scale.combined(with: .opacity)) }
            }
            .foregroundStyle(selected ? .white : .primary)
            .padding(.horizontal, 12).padding(.vertical, 10)
            .contentShape(Rectangle())
            .background(RoundedRectangle(cornerRadius: 16, style: .continuous)
                .fill(selected ? AnyShapeStyle(Color.accentColor.gradient) : AnyShapeStyle(Color.white.opacity(0.08))))
            .overlay(RoundedRectangle(cornerRadius: 16, style: .continuous)
                .strokeBorder(Color.white.opacity(selected ? 0.55 : 0), lineWidth: 1))
        }
        .buttonStyle(PressStyle())
        .animation(spring, value: selected)
    }

    // Step 3 - options (collapsed: the common path needs none)

    private var optionsSummary: String {
        vm.effectiveMode == .dd
            ? "Write image as-is\(vm.verify ? " · Verify" : "")"
            : "\(vm.scheme.rawValue) · \(vm.fileSystem.rawValue) · \(vm.label)"
    }

    private var optionsStep: some View {
        VStack(alignment: .leading, spacing: 12) {
            Button { withAnimation(spring) { showOptions.toggle() } } label: {
                HStack {
                    stepHeader(3, "Options", done: vm.imageInfo != nil)
                    Spacer()
                    Text(optionsSummary).font(.system(size: 12)).foregroundStyle(.secondary).lineLimit(1)
                    Image(systemName: "chevron.right").font(.system(size: 11, weight: .semibold)).foregroundStyle(.secondary)
                        .rotationEffect(.degrees(showOptions ? 90 : 0))
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            if showOptions {
                VStack(alignment: .leading, spacing: 12) {
                    optionRow("Write mode") {
                        Picker("", selection: $vm.mode) { ForEach(WriteMode.allCases) { Text($0.title).tag($0) } }.labelsHidden()
                    }
                    if vm.effectiveMode == .extract {
                        optionRow("Partition scheme") {
                            Picker("", selection: $vm.scheme) { ForEach(PartitionScheme.allCases) { Text($0.rawValue).tag($0) } }
                                .pickerStyle(.segmented).labelsHidden()
                        }
                        optionRow("File system") {
                            Picker("", selection: $vm.fileSystem) { ForEach(FileSystemChoice.allCases) { Text($0.rawValue).tag($0) } }
                                .pickerStyle(.segmented).labelsHidden()
                        }
                        optionRow("Volume label") { TextField("NDAGAFIJO", text: $vm.label).textFieldStyle(.roundedBorder) }
                        Text("Boots via: \(vm.scheme.target)").font(.system(size: 12)).foregroundStyle(.secondary)
                    } else {
                        Toggle("Verify after writing", isOn: $vm.verify).toggleStyle(.checkbox)
                        Text("The image is written exactly as-is, so it decides the partitioning and boot method.")
                            .font(.system(size: 12)).foregroundStyle(.secondary)
                    }
                }
                .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .padding(16)
        .glass(radius: 26)
    }

    private var writeButton: some View {
        VStack(spacing: 6) {
            Button { vm.showConfirm = true } label: {
                Text("Write to Drive").font(.system(size: 16, weight: .semibold)).frame(maxWidth: .infinity, minHeight: 34)
            }
            .glassButton(prominent: true)
            .controlSize(.large)
            .keyboardShortcut(.defaultAction)
            .disabled(!vm.canStart)
            if vm.canStart {
                Text("Erases everything on \(vm.drives.first { $0.id == vm.selectedDrive }?.name ?? "the selected drive")")
                    .font(.system(size: 12)).foregroundStyle(.secondary)
            }
        }
    }

    private var footerLinks: some View {
        HStack(spacing: 18) {
            Button("Log") { showLog = true }
            Button("Checksums") { vm.computeChecksums(); showLog = true }.disabled(vm.imageInfo == nil || vm.running)
        }
        .buttonStyle(.plain).font(.system(size: 12)).foregroundStyle(.secondary)
    }

    // MARK: Writing

    private var writingStage: some View {
        VStack(spacing: 22) {
            Spacer(minLength: 10)
            ZStack {
                Circle().stroke(Color.white.opacity(0.18), lineWidth: 14)
                Circle().trim(from: 0, to: max(vm.progress, 0.005))
                    .stroke(AngularGradient(colors: [Color.accentColor.opacity(0.7), .white], center: .center, startAngle: .degrees(0), endAngle: .degrees(360)),
                            style: StrokeStyle(lineWidth: 14, lineCap: .round))
                    .rotationEffect(.degrees(-90))
                    .animation(spring, value: vm.progress)
                VStack(spacing: 2) {
                    Text("\(Int(vm.progress * 100))")
                        .font(.system(size: 64, weight: .semibold, design: .rounded).monospacedDigit())
                        .contentTransition(.numericText())
                    Text("percent").font(.system(size: 12)).foregroundStyle(.secondary)
                }
            }
            .frame(width: 220, height: 220)
            .padding(30)
            .glass(radius: 140)

            VStack(spacing: 4) {
                Text(vm.status).font(.system(size: 16, weight: .semibold)).lineLimit(1)
                Text(vm.speed.isEmpty ? vm.selectedDriveTitle : "\(vm.speed) · \(vm.selectedDriveTitle)")
                    .font(.system(size: 12)).foregroundStyle(.secondary).lineLimit(1)
                HStack(spacing: 6) {
                    Text("\(ViewModel.clock(vm.elapsed)) elapsed")
            }
            Text("Keep the drive connected until this finishes.").font(.system(size: 12)).foregroundStyle(.tertiary)
            Button("Cancel") { vm.cancel() }.glassButton().controlSize(.large)
            Spacer(minLength: 10)
        }
        .frame(minHeight: 520)
    }

    // MARK: Done / failed

    private var doneStage: some View {
        resultStage(symbol: "checkmark", tint: .green, title: "Your drive is ready",
                    detail: vm.selectedDriveTitle) {
            Button("Eject") { vm.eject() }.glassButton(prominent: true).controlSize(.large)
            Button("Done") { vm.reset() }.glassButton().controlSize(.large)
        }
    }

    private var failedStage: some View {
        resultStage(symbol: "exclamationmark", tint: .red, title: "Couldn't finish",
                    detail: vm.errorMessage.isEmpty ? "Something went wrong." : vm.errorMessage) {
            Button("Back") { vm.reset() }.glassButton(prominent: true).controlSize(.large)
            Button("View Log") { showLog = true }.glassButton().controlSize(.large)
        }
    }

    private func resultStage<A: View>(symbol: String, tint: Color, title: String, detail: String,
                                      @ViewBuilder actions: () -> A) -> some View {
        VStack(spacing: 20) {
            Spacer(minLength: 10)
            Image(systemName: symbol)
                .font(.system(size: 54, weight: .bold)).foregroundStyle(.white)
                .frame(width: 130, height: 130)
                .glass(radius: 65, tint: tint.opacity(0.8))
            VStack(spacing: 6) {
                Text(title).font(.system(size: 24, weight: .semibold)).tracking(-0.3)
                Text(detail).font(.system(size: 13)).foregroundStyle(.secondary)
                    .multilineTextAlignment(.center).fixedSize(horizontal: false, vertical: true)
            }
            HStack(spacing: 12) { actions() }.glassContainer(spacing: 12)
            Spacer(minLength: 10)
        }
        .frame(minHeight: 520)
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
            .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
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

    private func stepHeader(_ n: Int, _ title: String, done: Bool) -> some View {
        HStack(spacing: 10) {
            ZStack {
                Circle().fill(done ? Color.white : Color.white.opacity(0.2))
                if done { Image(systemName: "checkmark").font(.system(size: 11, weight: .bold)).foregroundStyle(Color.black.opacity(0.7)) }
                else { Text("\(n)").font(.system(size: 12, weight: .semibold)) }
            }
            .frame(width: 24, height: 24)
            .animation(spring, value: done)
            Text(title).font(.system(size: 15, weight: .semibold))
        }
    }

    private func optionRow<C: View>(_ title: String, @ViewBuilder _ content: () -> C) -> some View {
        HStack {
            Text(title).font(.system(size: 13)).frame(width: 120, alignment: .leading)
            content()
            Spacer(minLength: 0)
        }
    }
}
