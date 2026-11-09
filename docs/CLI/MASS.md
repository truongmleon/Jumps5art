# Mass local development preview

Mass uses the Linux server's protected local socket. Run the daemon and the Mass helper on the same host as the service user. An existing metadata root must be on schema 8.

Start the daemon and register a primary with `jumpstart-server storage register PATH --plan --role primary` followed by `storage apply`. The [server CLI reference](README.md#volume-registration) covers registration and systemd write permissions.

Run the helper and open the printed URL in a browser on that host.

```sh
cargo run -p jumpstart-server --bin mass -- web
```

For a scratch daemon, add `--socket PATH`. The URL includes a private session capability. The browser keeps it in the tab session and removes it from the address bar. Keep the helper running while using the tab.

The file browser offers folders, breadcrumbs, list and grid views, filename search, details, drag-and-drop uploads, and downloads. Choose a registered primary for root uploads. New entries inside a folder use that folder's primary. The preview accepts files up to 32 MiB. A rejected upload can be retried with the same request while the original remains available to the tab.

## Native Linux mount

Install FUSE3 and run the helper as the same service user with an empty, owned mount directory.

```sh
mkdir -p "$HOME/Mass"
cargo run -p jumpstart-server --bin mass -- mount "$HOME/Mass"
```

Use `--socket PATH` if the daemon uses a non-default socket. Keep the foreground helper running. Stop it with Ctrl+C or unmount it from another terminal with `fusermount3 -u "$HOME/Mass"`.

The mount is private to its owner and read-only. It lists the same Mass folders as the browser. Each open pins a file version and retrieves only the verified chunks needed by the request. If those bytes are unavailable, Linux receives an I/O error. Use the browser to add files and folders.

The desktop release still needs the writable shadow-file journal, the full indexer and preview matrix, and Windows WinFsp. The separate Finder preview below uses paired remote reads.

## Paired Mass endpoint

On Linux, start the daemon with `serve --mass-listen SERVER_IP:PORT` in addition to its existing metadata and socket arguments. Pick a concrete address and a UDP port separate from the temporary pairing port. The regular listener and pairing use the same root-managed `jumpstart-iroh.key` systemd credential. In the service's address-family drop-in allow `AF_UNIX AF_NETLINK` and the IP families used by the listener and pairing. Configure the host firewall to allow the selected ports only from the intended network.

The paired endpoint offers read-only Mass catalog and verified file ranges for the exact approved installation. The Finder helper dials a configured address and pins the server's public key and authority ID. The Linux browser and FUSE mount continue to use the local socket.

## Native Finder preview

Mass Finder targets macOS 14 or newer. It registers a read-only File Provider location that browses the same server catalog. The Mac connects to the configured regular Mass UDP endpoint and uses the separate temporary LAN invitation to pair.

### Build a signed app

Build the Rust helper for the architecture of the Mac that will run the app.

```sh
cargo build --locked -p jumpstart-server --bin mass-device
```

Use an installed Apple signing identity and separate macOS provisioning profiles for the app and its embedded provider. The profiles must authorize the corresponding application identifiers and the same Keychain access group. Use an app identifier owned by your developer team if the default is unavailable.

| Build input | What to supply |
|---|---|
| `MASS_SIGNING_IDENTITY` | An Apple signing identity installed in the local Keychain. |
| `MASS_TEAM_ID` | The developer team ID. |
| `MASS_BUNDLE_ID` | The host app identifier. Defaults to `com.jumpstart.mass.finder`. The provider app ID adds `.provider`. |
| `MASS_APP_IDENTIFIER_PREFIX` | The profiles' app identifier prefix if it differs from the team ID. |
| `MASS_APP_PROFILE` | Path to the host app's macOS provisioning profile. |
| `MASS_PROVIDER_PROFILE` | Path to the provider's macOS provisioning profile. |

The Keychain access group is `PREFIX.BUNDLE_ID.shared`. With these values set, build to a fresh path and open the signed app on the Mac.

```sh
bash apps/mass-macos/build.sh --signed "$HOME/Applications/Mass Finder.app" target/debug/mass-device
open "$HOME/Applications/Mass Finder.app"
```

Use `--unsigned` for a compile and bundle check without signing materials. Finder registration and Keychain sharing require the signed build. [Apple provisioning guidance](https://developer.apple.com/documentation/technotes/tn3125-inside-code-signing-provisioning-profiles)

### Pair and open Finder

1. Enable the regular Mass listener on the Linux server and create a temporary invitation with `jumpstart-server pair create --interface IFACE --listen IPV4:PAIRING_PORT --json`.
2. Paste that invitation into Mass Finder and enter the **regular Mass address**, which uses the port passed to `serve --mass-listen`.
3. Compare the six words with `jumpstart-server pair pending --json`. Use the exact approval command shown after reviewing the matching request.
4. In Mass Finder, choose **Open in Finder**. The registered domain is called **Mass**.
5. Right-click a file and choose **Download from Mass** before opening it. Ordinary placeholder fetches only use an explicitly downloaded and verified local file. The helper stages one file at a time and supports up to 32 MiB per file.

Use **Refresh Finder** to refresh the root catalog. **Remove and forget Mac** removes the native domain and its local credential. Revoke that installation on the server with `jumpstart-server devices revoke DEVICE_ID` to cancel its active Mass sessions and prevent reconnects.

The signed Finder registration, App Sandbox and Keychain handoff, and the Finder action require a real user-session acceptance run. The unsigned CI build verifies the native bundle and persistent filename mapping. The isolated Linux integration verifies the paired helper against real published bytes and confirms that a mismatched or revoked instance is refused.
