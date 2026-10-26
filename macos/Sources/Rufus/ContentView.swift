/*
 * Rufus for macOS - main window
 * Copyright © 2011-2026 Pete Batard <pete@akeo.ie>
 * Licensed under the GNU GPL v3 or later, see LICENSE.txt
 */
import SwiftUI
import UniformTypeIdentifiers

struct ContentView: View {
    @StateObject private var vm = ViewModel()
    @State private var showLog = false

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            section("Drive Properties") {
                row("Device") {
                    Picker("", selection: $vm.selectedDrive) {
                        if vm.drives.isEmpty { Text("No removable drive found").tag("") }
                        ForEach(vm.drives) { Text($0.title).tag($0.id) }
                    }.labelsHidden().disabled(vm.running)
                    Button { vm.refreshDrives() } label: { Image(systemName: "arrow.clockwise") }
                        .help("Refresh drive list").disabled(vm.running)
                }
                Toggle("List USB hard drives", isOn: $vm.includeFixed).disabled(vm.running)
                row("Boot selection") {
                    Text(vm.imagePath.isEmpty ? "Select an ISO or disk image" : (vm.imagePath as NSString).lastPathComponent)
                        .lineLimit(1).truncationMode(.middle)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 8).padding(.vertical, 4)
                        .background(RoundedRectangle(cornerRadius: 5).fill(.quaternary))
                    Button("SELECT") { chooseImage() }.disabled(vm.running)
                }
                if let info = vm.imageInfo { Text(info.summary).font(.caption).foregroundStyle(.secondary) }
                if let w = vm.modeWarning { Text(w).font(.caption).foregroundStyle(.orange) }
                row("Write mode") {
                    Picker("", selection: $vm.mode) {
                        ForEach(WriteMode.allCases) { Text($0.title).tag($0) }
                    }.labelsHidden().disabled(vm.running)
                }
            }
            section("Format Options") {
                row("Partition scheme") {
                    Picker("", selection: $vm.scheme) {
                        ForEach(PartitionScheme.allCases) { Text($0.rawValue).tag($0) }
                    }.labelsHidden().disabled(vm.running || vm.effectiveMode == .dd)
                }
                if vm.effectiveMode != .dd {
                    row("Target system") { Text(vm.scheme.target).foregroundStyle(.secondary) }
                }
                row("Volume label") {
                    TextField("", text: $vm.label).disabled(vm.running || vm.effectiveMode == .dd)
                }
                row("File system") {
                    Picker("", selection: $vm.fileSystem) {
                        ForEach(FileSystemChoice.allCases) { Text($0.rawValue).tag($0) }
                    }.labelsHidden().disabled(vm.running || vm.effectiveMode == .dd)
                }
                Toggle("Verify written data (DD mode)", isOn: $vm.verify)
                    .disabled(vm.running || vm.effectiveMode != .dd)
            }
            section("Status") {
                ProgressView(value: vm.progress).progressViewStyle(.linear)
                HStack {
                    Text(vm.status).font(.system(.caption, design: .monospaced)).lineLimit(1)
                    Spacer()
                    Text("\(Int(vm.progress * 100))%").font(.caption).foregroundStyle(.secondary)
                }
            }
            DisclosureGroup("Log", isExpanded: $showLog) {
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
