// Guides the owner through LAN pairing and registers the native Finder domain.
// Only a verified signed grant is written to the shared Keychain access group.

import AppKit
import FileProvider
import Foundation
import SwiftUI

/// The macOS host application for the Mass File Provider extension.
@main
struct MassFinderApp: App {
    @StateObject private var setup = Setup()

    var body: some Scene {
        WindowGroup("Mass Finder") {
            SetupView(setup: setup)
                .frame(minWidth: 540, minHeight: 470)
                .task { setup.load() }
        }
        .windowResizability(.contentMinSize)
    }
}

/// Displays the server invitation, six-word comparison, and Finder actions.
private struct SetupView: View {
    @ObservedObject var setup: Setup

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                Label("Mass in Finder", systemImage: "externaldrive.connected.to.line.below")
                    .font(.largeTitle.weight(.semibold))
                Text("Browse your Mass library as a read-only Finder location.")
                    .foregroundStyle(.secondary)

                if setup.profile == nil {
                    VStack(alignment: .leading, spacing: 10) {
                        TextField("Device name", text: $setup.name)
                        TextField("Mass server address, for example 192.168.1.20:37520", text: $setup.address)
                        Text("Paste the server’s private pairing invitation JSON.")
                            .font(.subheadline)
                        TextEditor(text: $setup.invitation)
                            .font(.system(.body, design: .monospaced))
                            .frame(height: 100)
                            .privacySensitive()
                            .overlay(RoundedRectangle(cornerRadius: 5).strokeBorder(.quaternary))
                        Button("Request pairing") { setup.pair() }
                            .buttonStyle(.borderedProminent)
                            .disabled(setup.busy || setup.address.isEmpty || setup.invitation.isEmpty)
                    }
                } else {
                    Text(setup.profile?.address ?? "")
                        .font(.system(.body, design: .monospaced))
                    HStack {
                        Button("Open in Finder") { Task { await setup.openFinder() } }
                            .buttonStyle(.borderedProminent)
                        Button("Refresh Finder") { Task { await setup.refresh() } }
                        Button("Remove and forget Mac", role: .destructive) { setup.confirmForget = true }
                    }
                    .disabled(setup.busy)
                    .confirmationDialog("Remove the Mass location and forget this Mac’s pairing?", isPresented: $setup.confirmForget) {
                        Button("Remove and forget", role: .destructive) { Task { await setup.forget() } }
                    }
                    Text("In Finder, right-click a file and choose Download from Mass to make it available.")
                        .foregroundStyle(.secondary)
                }

                if let review = setup.review {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Compare these words with the server operator").font(.headline)
                        Text(review.phrase).font(.title3.monospaced()).textSelection(.enabled)
                        Text("Key fingerprint \(review.fingerprint)").textSelection(.enabled)
                        Text(setup.approvalCommand(review)).font(.system(.footnote, design: .monospaced)).textSelection(.enabled)
                        Button("Copy approval command") { setup.copyCommand(review) }
                    }
                    .padding(13)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(.quaternary.opacity(0.45), in: RoundedRectangle(cornerRadius: 10))
                }

                HStack {
                    if setup.busy { ProgressView().controlSize(.small) }
                    Text(setup.status).foregroundStyle(setup.failed ? Color.red : Color.secondary)
                        .textSelection(.enabled)
                }
            }
            .padding(26)
        }
    }
}

/// Owns the approved local credential and the native domain lifecycle.
@MainActor
private final class Setup: ObservableObject {
    @Published var name = Host.current().localizedName ?? "Mass Mac"
    @Published var address = ""
    @Published var invitation = ""
    @Published var profile: MassProfile?
    @Published var review: Review?
    @Published var status = "Start a pairing window on the Linux server."
    @Published var busy = false
    @Published var failed = false
    @Published var confirmForget = false

    /// Load an existing approved Mac installation when the window opens.
    func load() {
        guard !busy else { return }
        do {
            profile = try MassKeychain(bundle: .main).load()
            if profile != nil { show("This Mac is paired. Open the Mass location in Finder.") }
        } catch { show(error.localizedDescription, failed: true) }
    }

    /// Request pairing and show the pending phrase without retaining the invitation text.
    func pair() {
        guard !busy, profile == nil else { return }
        do {
            let invite = try JSONDecoder().decode(Invitation.self, from: Data(invitation.utf8))
            let server = address.trimmingCharacters(in: .whitespacesAndNewlines)
            let device = name.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !device.isEmpty, device.utf8.count <= 100, server.contains(":") else {
                throw MassFailure(message: "Enter a device name and the regular Mass server IP address and port.")
            }
            invitation = ""
            review = nil
            busy = true
            show("Connecting to the temporary pairing listener.")
            Task {
                do {
                    let enrolled = try await Task.detached(priority: .userInitiated) {
                        let helper = MassHelper(bundle: .main)
                        let identity = try helper.run(HelperRequest(action: "generate"), as: Identity.self)
                        let profile = MassProfile(address: server, server_key: invite.server_key,
                            server_id: invite.server_id, instance_id: identity.instance_id, seed: identity.seed)
                        _ = try helper.run(
                            HelperRequest(action: "enroll", profile: profile, invitation: invite, name: device),
                            as: Grant.self,
                            pending: { review in
                                Task { @MainActor in
                                    self.review = review
                                    self.show("Approve the matching request on the server before the window closes.")
                                }
                            }
                        )
                        return profile
                    }.value
                    try MassKeychain(bundle: .main).save(enrolled)
                    profile = enrolled
                    review = nil
                    show("This Mac is paired. Open the Mass location in Finder.")
                } catch {
                    review = nil
                    show(error.localizedDescription, failed: true)
                }
                busy = false
            }
        } catch { show(error.localizedDescription, failed: true) }
    }

    /// Verify the regular endpoint, register the File Provider domain, and open Finder.
    func openFinder() async {
        guard let profile, !busy else { return }
        busy = true
        defer { busy = false }
        do {
            try await checkEndpoint(profile)
            let domain = domain(profile)
            let domains = try await NSFileProviderManager.domains()
            if !domains.contains(where: { $0.identifier == domain.identifier }) {
                try await NSFileProviderManager.add(domain)
            }
            guard let manager = NSFileProviderManager(for: domain) else {
                throw MassFailure(message: "macOS could not load the signed Mass File Provider.")
            }
            let url = try await manager.getUserVisibleURL(for: .rootContainer)
            guard NSWorkspace.shared.open(url) else {
                throw MassFailure(message: "Find Mass under Locations in Finder.")
            }
            show("Mass is available under Locations in Finder.")
        } catch { show(error.localizedDescription, failed: true) }
    }

    /// Request a fresh root enumeration for the already mounted preview.
    func refresh() async {
        guard let profile, !busy else { return }
        do {
            guard let manager = NSFileProviderManager(for: domain(profile)) else {
                throw MassFailure(message: "Open Mass in Finder before refreshing.")
            }
            try await manager.signalEnumerator(for: .rootContainer)
            show("Finder refresh requested.")
        } catch { show(error.localizedDescription, failed: true) }
    }

    /// Remove the native domain before forgetting its local pairing key.
    func forget() async {
        guard let profile, !busy else { return }
        busy = true
        defer { busy = false }
        do {
            let domain = domain(profile)
            if try await NSFileProviderManager.domains().contains(where: { $0.identifier == domain.identifier }) {
                try await NSFileProviderManager.remove(domain)
            }
            try MassKeychain(bundle: .main).forget()
            self.profile = nil
            show("Mass was removed from Finder. A fresh invitation is needed to pair again.")
        } catch { show(error.localizedDescription, failed: true) }
    }

    /// Render the exact server command for this displayed review.
    func approvalCommand(_ review: Review) -> String {
        "jumpstart-server pair approve \(review.request_id) --confirm-transcript \(review.transcript)"
    }

    /// Copy only the local approval command, without the invitation token or seed.
    func copyCommand(_ review: Review) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(approvalCommand(review), forType: .string)
    }

    /// Check the pinned application listener before creating a native domain.
    private func checkEndpoint(_ profile: MassProfile) async throws {
        _ = try await Task.detached(priority: .userInitiated) {
            try MassHelper(bundle: .main).run(HelperRequest(action: "probe", profile: profile), as: Probe.self)
        }.value
    }

    /// Give each paired authority a stable Finder domain identity.
    private func domain(_ profile: MassProfile) -> NSFileProviderDomain {
        NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier("mass-" + profile.server_id), displayName: "Mass")
    }

    /// Show a local setup result or refusal.
    private func show(_ text: String, failed: Bool = false) {
        status = text
        self.failed = failed
    }
}
