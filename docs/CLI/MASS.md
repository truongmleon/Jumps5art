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

The desktop release still needs the writable shadow-file journal, indexer and preview gates, paired remote access, macOS File Provider, and Windows WinFsp.

This slice uses the local catalog and socket. Paired remote file access, provider downloads, shared application views, and native desktop packaging have separate delivery gates.


## Paired Mass endpoint

On Linux, start the daemon with `serve --mass-listen SERVER_IP:PORT` in addition to its existing metadata and socket arguments. Pick a concrete address and a UDP port separate from the temporary pairing port. The regular listener and pairing use the same root-managed `jumpstart-iroh.key` systemd credential. In the service's address-family drop-in allow `AF_UNIX AF_NETLINK` and the IP families used by the listener and pairing. Configure the host firewall to allow the selected ports only from the intended network.

The paired endpoint offers read-only Mass catalog and verified file ranges for the exact approved installation. The Finder helper dials a configured address and pins the server's public key and authority ID. The Linux browser and FUSE mount continue to use the local socket.
