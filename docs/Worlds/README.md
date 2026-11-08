# Worlds

Worlds runs one Fabric Minecraft: Java Edition server on the Linux computer. There is no Worlds app. You use the `worlds` command on that computer.

[Getting started](../Getting%20Started/README.md) covers the rest of Jumpstart. [Mass](../Mass/README.md) backs up the Minecraft files together with your other libraries. Worlds itself creates the world backups you restore from.

## Development CLI

The Linux development binary currently reads the schema-4 catalog through the
running Jumpstart daemon's protected socket. Run it as root or the daemon's
service user. For a scratch daemon, pass `--socket PATH`.

```sh
cargo run -p jumpstart-server --bin worlds -- status --json
cargo run -p jumpstart-server --bin worlds -- backup list --limit 50 --json
```

`status` reports `absent` when the catalog has no registered primary for managed
storage. If a primary is registered and runtime evidence is unavailable, it
prints the catalog evidence with a null lifecycle and exits with code 4.
`backup list` reports indexed manifest identities and UTC Unix millisecond
timestamps. It returns the newest captures first, with `truncated` set when
more records exist. These records describe prior verification; checking the
actual files and determining retention or protection need the filesystem
catalog. The sections below describe the full planned Worlds surface.

## See what it is doing

```sh
worlds status
```

Status tells you whether the server is stopped, starting, running, stopping, crashed, or needs recovery, and whether a backup or restore is also in progress. Players can join only when it says they can.

```sh
worlds logs
worlds operations list
```

Every backup, restore, start, stop, and mod change is queued, running, completed, failed, or canceled. A restart of the computer does not mark a half-finished operation as successful.

Starting a server that is already running, or stopping one that is already stopped, prints a clear message and changes nothing.

## Create and replace

Create is for the first server. If one already exists, create does nothing until you archive it or replace it. Replace makes and checks a backup first.

```sh
worlds create --name NAME --minecraft VERSION --fabric VERSION --accept-eula
worlds archive-existing --plan
worlds replace --name NAME --minecraft VERSION --fabric VERSION --plan
worlds apply --plan PLAN_ID --confirm TOKEN
```

`--plan` shows what will happen and gives you a confirmation code. `apply` runs that exact plan. An old code does nothing. Make a new plan.

## Start, stop, and restart

```sh
worlds start
worlds stop --countdown SECONDS
worlds restart --countdown SECONDS
```

Stop and restart can warn players for the countdown you set. If the server quits unexpectedly, Worlds keeps the log and tries at most three automatic restarts in five minutes. After that it waits for you.

If the world can no longer be saved, players are stopped, and Worlds tells you whether the last save completed. You confirm a check or a restore before it opens again.

## Backups

A daily backup runs at the local time you set, and only if the world or player data changed. A missed time runs once at the next chance. It does not stack several missed days into several backups.

```sh
worlds backup schedule --time HH:MM --timezone IANA_ZONE
worlds backup create --label TEXT
worlds backup create --protect --label TEXT
worlds backup list
worlds backup verify BACKUP_ID
worlds backup protect BACKUP_ID
worlds backup unprotect BACKUP_ID
```

While people are playing, Worlds warns them, pauses saving long enough to freeze a copy, then turns saving back on before it finishes writing the backup. If that copy fails, the backup is not kept and saving is turned back on. If saving cannot be turned back on, the server does not quietly keep going.

The newest five ordinary backups are kept. A backup you protect is not counted in those five and is not deleted automatically. A backup taken before a mod or version change is protected until you accept the change or roll it back.

A backup records the Minecraft version, Fabric, and mods. If some of that software cannot be restored with the world, Worlds says so when the backup is created.

A full or disconnected backup disk warns you. It does not delete protected backups to make room.

## Restore

Stop the Minecraft server first.

```sh
worlds restore BACKUP_ID --plan
```

Worlds checks the backup again and shows its date, world name, Minecraft version, and mods before anything is replaced. It makes a safety backup of the current world unless you explicitly continue without one. If that safety copy fails, the restore does not start.

The backup you picked and the safety copy are kept until the restore finishes or you cancel. A failed restore leaves the current world as it was. Afterward, Worlds compares the restored world's recorded mods with the mods that are installed, and asks before starting on a mismatch.

## Mods and version changes

```sh
worlds mods plan --install SOURCE@VERSION
worlds mods plan --remove MOD_ID
worlds mods plan --enable MOD_ID
worlds mods plan --disable MOD_ID
worlds mods plan --update MOD_ID@VERSION
worlds configuration plan --minecraft VERSION --fabric VERSION --api VERSION
worlds changes list
worlds changes accept CHANGE_ID
worlds changes rollback CHANGE_ID --plan
```

The plan lists every mod it will add, remove, or change. Required dependencies that have one clear choice are included. A missing dependency or a hard conflict stops the change. You also get a warning when a mod does not match the Minecraft or Fabric version, and when removing a mod may affect blocks or items it added.

Nothing is applied until you confirm the plan. If the server fails after the change, roll back the world and the matching mods together. The rollback tells you if newer play would be replaced.

If Minecraft, Fabric, or a mod site is down, the installed server stays as it is. A half-downloaded update is not applied.
