# Mass local development preview

Mass uses the Linux server's protected local socket. Run the daemon and the Mass helper on the same host as the service user. An existing metadata root must be on schema 8.

Start the daemon and register a primary with `jumpstart-server storage register PATH --plan --role primary` followed by `storage apply`. The [server CLI reference](README.md#volume-registration) covers registration and systemd write permissions.

Run the helper and open the printed URL in a browser on that host.

```sh
cargo run -p jumpstart-server --bin mass -- web
```

For a scratch daemon, add `--socket PATH`. The URL includes a private session capability. The browser keeps it in the tab session and removes it from the address bar. Keep the helper running while using the tab.

The file browser offers folders, breadcrumbs, list and grid views, filename search, details, drag-and-drop uploads, and downloads. Choose a registered primary for root uploads. New entries inside a folder use that folder's primary. The preview accepts files up to 32 MiB. A rejected upload can be retried with the same request while the original remains available to the tab.

This slice uses the local catalog and socket. Paired remote file access, provider downloads, shared application views, and native desktop packaging have separate delivery gates.
