# macOS source-candidate checks

This local source candidate is not an approved public release. Licensing materials and final package checks remain pending.
Mac packages use free ad-hoc signing. Paid Developer ID signing/notarization is not planned;
Gatekeeper approval is an accepted limitation, not unfinished paid-signing work.

To open a trusted download, try launching `Falcon.app` once, then use **System Settings → Privacy &
Security → Open Anyway** for its blocked-app entry and confirm. Keep global security protections on.
See [Apple's instructions](https://support.apple.com/en-ie/102445); device-management policy may
prevent overrides.

Replace an older installed Falcon.app when checking normal file associations; several copies with
the same app identity can make macOS open the wrong one. Keep a backup of the prior app and use
copied or synthetic photos for tests that change review state or files.

Check ordinary-window and green-button full-screen toolbar placement, repeated entry/exit, top-edge
hover, resizing and moving between displays. Test F immersive mode, English and Pinyin input,
Settings, sorting, context menus and keyboard focus. Verify Open With/default associations open
this exact package, and existing settings and review data still load correctly.

Diagnostic logging is under Developer settings and defaults Off. Enable it only when investigating;
review logs for personal paths or image metadata before sharing. Report the source revision,
macOS version and exact steps alongside any screenshot or recording.

**Log out, restart and shut down** (builds with the log-out fix, after 1.0.15). Turn on
Diagnostic logging, use copies of photos and save work in other apps first. Rate or flag a few
photos, then log out straight away. macOS should not say that Falcon interrupted log out, and
one attempt should be enough. Log back in, reopen the folder and check the last ratings and
flags. Repeat with Restart and with Shut Down; do one of them with XMP sync on and check the
sidecar too. Then check that ⌘Q and Dock → Quit still close promptly with no new prompt and keep
changes, and that closing the window with unapplied rotations still shows the reminder. Last,
start Copy picks on copied photos and log out during it: finished copies open, originals are
unchanged and ratings survive. The log should show `terminate: system reason=` and later
`terminate: reply=now`. A line with `STILL running at the boundary deadline`,
`FLUSH BARRIER exceeded` or `final ratings flush failed` means that run proved nothing about
saving; check the files after logging back in either way.
