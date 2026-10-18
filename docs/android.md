# WordCraft on Android (experimental)

WordCraft's desktop interface running on Android tablets, meant for use with a keyboard, an
S Pen and, if you like, a mouse. Built from the `android` branch by
`.github/workflows/android.yml` for arm64 devices running Android 11 or later. The approach is
the one PdfCraft's Android port uses (a NativeActivity APK built without Gradle).

## Install

1. On the tablet, open the repository's **Releases** page and download `wordcraft.apk` from the
   `android-latest` prerelease.
2. Open the downloaded file. Android asks to allow installs from your browser or Files app the
   first time; allow it, then install. If Play Protect warns about an unknown app, choose
   *More details ▸ Install anyway*.
3. Give WordCraft file access. It is a special access, not one of the app's regular permissions:
   the first time you open or save a file, the file browser shows **Allow access…**, which opens
   the switch in Settings. Or turn it on yourself: **Settings ▸ Apps ▸ ⋮ ▸ Special access ▸ All
   files access ▸ WordCraft**. Without it WordCraft can only open and save files in its own app
   folder.

Later builds install over the earlier one as long as they are signed with the same key.

## Updates

WordCraft updates itself from the GitHub repository it was built from. At start-up, and at most
once an hour after that (when it's open or comes back to the front), it asks GitHub for the
`android-latest` release. When that is a newer build than the one installed, a card at the bottom
right offers it:

1. **Update** downloads the APK (about 23 MB) inside the app, with a progress bar.
2. The first time, Android asks you to allow WordCraft to install apps: turn on **Allow from this
   source** in the settings that open, come back and tap **Install**.
3. Android shows its own install window: tap **Update**. WordCraft closes and comes back on the
   new build, so save your document first.

**Later** hides the card for that build until WordCraft next starts. Being offline is fine: a
check that fails says nothing and tries again later. Android only installs an update signed with
the same key as the installed app, so a download that isn't one of this repository's builds is
refused. Builds before build 2 have no updater: install build 2 or later by hand once.

## Open documents from other apps

WordCraft appears in **Open with** for Word documents (.docx, .docm, .dotx, .doc), OpenDocument
text (.odt), RTF, plain text and Markdown, from Files, My Files, Gmail, Drive and Chrome
downloads. Choose WordCraft and **Always** to make it the default.

A document from a file in your storage opens as that file, and Save writes it in place (with All
files access). A document that only exists in another app (an email attachment, a Drive file) is
first saved as a copy in **Documents/WordCraft** (or the app folder, without All files access),
and that copy is what you edit and save. Each document opens in a tab of its own.

## How it differs from the desktop app

- **Tabs:** each document has a tab along the top, as in PdfCraft. File ▸ New, the **+** at the
  end of the tabs, File ▸ Open and documents sent from other apps each open in a new tab (Open
  uses the front tab instead when it is an untouched blank document). A tab's **×**, File ▸
  Close or Ctrl+W closes that document (asking to save changes first) and leaves the app open;
  closing the last tab leaves a blank one. Ctrl+Tab and Ctrl+Shift+Tab move between tabs, and
  a dot after a tab's name marks unsaved changes. Opening a document that is already open brings
  its tab to the front.
- **System bars:** the status bar and taskbar take the tab strip's colour, with icons drawn
  dark or light so they stay readable in the light and dark themes.
- **Files:** Open, Save As, Export and Insert Picture use WordCraft's built-in file browser, with
  shortcuts to Internal storage, Documents, Download, SD cards and USB drives, and the app folder.
- **Touch:** a finger tap clicks, a finger drag scrolls the page (it keeps gliding after a
  flick), and a finger held still for half a second right-clicks (context menus). Two fingers
  pan, and pinch to zoom. Fingers don't select text: use the S Pen, a mouse or the keyboard.
- **S Pen:** works like a mouse with the button held: tap to place the cursor, drag to select,
  click buttons and menus. While the pen is on the screen, fingers and a resting palm are
  ignored.
- **Mouse:** a paired Bluetooth or USB mouse works as on a desktop (hover, right-click, wheel,
  Ctrl+wheel zoom), and so does a touchpad on a keyboard cover.
- **Keyboard:** with a keyboard cover or Bluetooth keyboard attached, the on-screen keyboard stays
  away; without one, it comes up when you tap into the document or a text field. Desktop
  shortcuts work (Ctrl+S, Ctrl+B, Ctrl+Z…).
- **Clipboard:** what you copy in WordCraft goes to other apps with its formatting (as HTML, with
  plain text for apps that want that), and Paste takes text and formatted copies from other apps.
  Pictures aren't exchanged with other apps.
- **Saving:** AutoSave works for documents that have been saved once. Unsaved work, untitled
  documents included, is also kept as a recovery copy: if Android stops WordCraft in the
  background, File ▸ Home offers it back the next time WordCraft starts.
- **Not on Android:** printing to a printer (File ▸ Print can still save a PDF) and the
  `--control` automation channel.
- Settings, the custom dictionary, added fonts and logs live in the app's private folder;
  uninstalling removes them. Fonts: Android's own (Roboto, Noto) plus WordCraft's bundled ones.

## Build it yourself

Needs the Android SDK and NDK, `rustup target add aarch64-linux-android` and
`cargo install cargo-ndk`:

```sh
ANDROID_HOME=~/Android/Sdk ANDROID_NDK_HOME=~/Android/Sdk/ndk/<version> packaging/android/build-apk.sh
adb install -r target/android-apk/wordcraft.apk
adb logcat -s WordCraft   # the app's log
```

The build log of every CI run is also pushed to the `ci-logs` branch (`build.log`, and the
compiler errors alone in `errors.txt`).

## Signing

The APK is signed with `packaging/android/debug.keystore` (password `android`), which is public,
like Android's own debug key. To use a private key instead, add the repository secrets
`ANDROID_KEYSTORE_B64` (the keystore, base64), `ANDROID_KEYSTORE_PASSWORD` and
`ANDROID_KEY_ALIAS`. Android won't install an update signed with a different key over an
installed app: uninstall the debug-signed one first.

## How it's put together

| Part | What it does |
|---|---|
| `apps/wordcraft-android` | The `cdylib` Android loads: `android_main`, the host services, `tablet.rs` (insets, touch routing, keyboard, clipboard sync), `file_access.rs`, `incoming.rs`, `clipboard.rs`, `updater.rs` (update check and card) |
| `crates/ui-egui` | Unchanged UI, plus two host options any platform can use: `Services::builtin_file_browser` and `Services::storage` (the access banner and storage shortcuts in the file browser) |
| `crates/fonts` | Scans `/system/fonts` and `/product/fonts` on Android |
| `vendor/winit` | winit 0.30.13 with PdfCraft's Android patches (tool type per touch, mouse and touchpad as a mouse); see `vendor/README.md` |
| `packaging/android` | Manifest, the Java activity (intents, the updater's HTTP and PackageInstaller install), signing key, `build-apk.sh` |
