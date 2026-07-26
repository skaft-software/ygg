# Native delivery

Native applications begin only after the real-server web client and secure LAN
pairing pass their acceptance gates.

The preferred implementation is one shared React frontend hosted by thin
system-webview shells. Tauri 2 is the current default candidate; a small spike
must validate it before it becomes a permanent dependency. Electron is out of
scope.

## macOS

The macOS shell should:

- start or attach to the bundled `ygg serve`;
- host the shared frontend;
- use native file and folder pickers;
- support drag and drop, notifications, and deep links;
- store device identity in Keychain;
- integrate system appearance and reduced motion;
- expose LAN pairing and connected-device management.

A distributable build requires the user's Apple Developer ID credentials,
code signing, hardened runtime, and notarization.

## iOS

The first iOS app is a companion client. It does not run the Ygg agent locally.
It discovers and pairs with a LAN host, stores its device identity in Keychain,
uses the responsive shared frontend, and adds native files/photos,
notifications, and deep links.

A device build requires Apple development signing and provisioning. TestFlight
is Apple's optional beta distribution channel; it is not required merely to
compile or install on a provisioned development device.

## Android

The first Android app is also a companion client. It uses the shared responsive
frontend, LAN discovery and pairing, Android secure storage, native
file/photo attachment, notifications, and deep links.

Initial testing can use a user-owned signed APK. Play internal testing and a
signed Android App Bundle can follow later.

## Shared-code rule

Native shells contain platform integration only. Session reducers, protocol
types, transcript rendering, product rules, themes, approvals, and preview
behavior remain shared. Platform-specific forks of the core interface are not
accepted.
