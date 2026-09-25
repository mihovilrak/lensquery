# Watch mode

`lq watch DIRECTORY` keeps an index current instead of rebuilding it. It
arms a filesystem watcher on `DIRECTORY` (recursively), waits for the
directory to go quiet, and then hands whatever changed to the same indexing
code `lq index` runs.

```console
$ lq watch ~/Screenshots --db ~/.local/share/lensquery/index.db
watching /home/me/Screenshots (4 workers, 3.0s debounce) — Ctrl-C to stop
1 indexed, 0 updated, 0 skipped, 0 failed (in 0.4s)
moved: /home/me/Screenshots/shot1.png -> /home/me/Screenshots/receipt.png
dropped 1 removed file(s) from the index
```

Everything above goes to **stderr**. Watch mode writes nothing to stdout —
stdout carries results, and a watcher has none — so `lq watch ~/shots
2>>watch.log` is the whole of "run it and keep a log".

## It is not a second indexer

Watch mode decides *which paths to offer*; it never decides whether a file
needs OCR. That call belongs to `indexer::index_paths`, the same function
`lq index` reaches through, and it is made the same way: compare the file's
mtime against the indexed one, skip if unchanged. A file you touch without
editing costs a `stat`, not an OCR pass.

The consequence worth knowing: `lq watch` and `lq index` over the same
directory produce the same database. Running `lq index` once to catch up and
then `lq watch` to stay current is the normal pattern, and the two cannot
disagree about what counts as an image or about what counts as changed.

## Debounce

`--debounce` (default `3.0` seconds, range `0.1`–`300`) is how long the
directory must be quiet before a batch is indexed.

A camera import, an unzip, or a `rsync` writes files for as long as it takes,
and every write is an event. OCR'ing a file that is still being written wastes
the work twice — once on the truncated image, once on the finished one.
Waiting for silence collapses the burst into one batch.

A batch is also forced out after **60 seconds** regardless of quiet, so a
directory under continuous write still makes progress instead of starving.

Shorten the debounce for hand-dropped single files; lengthen it for a source
that copies in slow, stuttering bursts.

## Renames and deletes

A move within the watched tree is detected by pairing the vanished path with
the appeared one inside a single debounce window, matching on mtime. It costs
one `UPDATE` and no OCR — the text is already indexed and did not change.

A file that vanishes without a partner is deleted from the index. A file that
vanishes and was never indexed is nothing at all.

Both are best-effort within the window: a move that straddles two debounce
windows falls back to a delete plus a fresh index, which is correct, just
slower.

## Concurrency

`--workers` defaults to **half** the CPU count (minimum 1), not all of it.
`lq index` is a job you wait for; `lq watch` runs while you are still working
in the directory it watches, and taking every core to OCR a screenshot you
just took is the wrong trade. Pass `--workers` explicitly to override.

## The index may not live in the watched tree

Every write to the database — and to its `-wal` and `-shm` siblings — is a
filesystem event. Inside the watched tree, that event arrives back at the
watcher, which is a feedback loop. `lq watch` refuses to start in that
configuration:

```console
$ lq watch ~/shots --db ~/shots/index.db
error: the index /home/me/shots/index.db is inside the watched directory
/home/me/shots; indexing would trigger the watcher, which would index again.
Pass --db pointing outside /home/me/shots.
```

Exit code 2. The extension filter would drop `.db` today; the guard is against
the design, not against one filename.

## Running it in the background

**`lq watch` is foreground-only in v0.1.0.** It does not fork, does not write
a PID file, and does not manage a log. That is deliberate: every platform
already has a supervisor that does all three better, and each one wants to own
restart policy and log rotation itself. Below is one working example per
platform.

### systemd (Linux, user unit)

`~/.config/systemd/user/lensquery-watch.service`:

```ini
[Unit]
Description=LensQuery watch
After=default.target

[Service]
ExecStart=%h/.cargo/bin/lq watch %h/Pictures/Screenshots --db %h/.local/share/lensquery/index.db
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
```

```console
$ systemctl --user enable --now lensquery-watch
$ journalctl --user -u lensquery-watch -f
```

stderr goes to the journal, so there is no log file to rotate. If the machine
should watch while you are logged out, `loginctl enable-linger $USER`.

### launchd (macOS)

`~/Library/LaunchAgents/com.lensquery.watch.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.lensquery.watch</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/me/.cargo/bin/lq</string>
    <string>watch</string>
    <string>/Users/me/Desktop/Screenshots</string>
    <string>--db</string>
    <string>/Users/me/Library/Application Support/lensquery/index.db</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>/Users/me/Library/Logs/lensquery-watch.log</string>
</dict>
</plist>
```

```console
$ launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.lensquery.watch.plist
$ launchctl print gui/$(id -u)/com.lensquery.watch
```

launchd does not expand `~`; the absolute paths above are required. The
watched directory may need Full Disk Access granted to the terminal or to `lq`
itself, depending on where it lives.

### Task Scheduler (Windows)

```powershell
$action  = New-ScheduledTaskAction -Execute "$env:USERPROFILE\.cargo\bin\lq.exe" `
  -Argument "watch `"$env:USERPROFILE\Pictures\Screenshots`" --db `"$env:LOCALAPPDATA\lensquery\index.db`""
$trigger = New-ScheduledTaskTrigger -AtLogOn
$settings = New-ScheduledTaskSettingsSet -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName "LensQuery watch" -Action $action -Trigger $trigger -Settings $settings
```

Task Scheduler shows a console window unless the task runs whether the user is
logged on or not; if that flicker matters, run it under `-WindowStyle Hidden`
via `powershell.exe` or use the "Run whether user is logged on or not" option
in the GUI. There is no built-in log capture — redirect stderr yourself if you
want one.

## What it does not do

- **No daemonization.** See above.
- **No cross-device move detection.** A file moved in from outside the watched
  tree is a new file and gets OCR'd; a file moved out is a delete.
- **No resume.** State lives entirely in the database, so a restart picks up
  from whatever is indexed — but changes made while the watcher was down are
  invisible to it. Run `lq index` once after any downtime that matters.
- **No network watching.** Watching an SMB or NFS mount depends on whether the
  server sends change notifications at all. Prefer a periodic `lq index` there.
