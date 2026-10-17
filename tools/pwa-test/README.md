# PWA test app

A tiny installable web app for checking ferry's local CA on real devices. It
reports whether the page is a secure context, whether its service worker is
registered and controlling the page, and whether it's running as a Home
Screen app. Service workers only run on trusted HTTPS, so this shows at a
glance whether trusting the CA worked.

## Run it

```sh
cd tools/pwa-test
python3 -m http.server 8000 --bind 127.0.0.1
# in another terminal
ferry 6005:8000
```

Python only listens on 127.0.0.1, so the phone reaches the app through ferry.
Scan the QR code in ferry's banner to trust the CA, then open
`https://<lan-ip>:6005/` on the phone.

## What to check

- **Secure context: yes.** The phone trusts the certificate. On iOS, "no"
  usually means full trust isn't on yet in Settings → General → About →
  Certificate Trust Settings.
- **Service worker: registered and active.** Chrome refuses to register one
  on a certificate error, even after a click-through.
- **This page: controlled**, after one reload: the service worker is serving
  it from its cache.
- **Display mode: standalone**, when launched from the Home Screen. Force-quit
  and relaunch; it should still be controlled. Stop ferry and relaunch again:
  it should still open, from the cache.

Also worth checking each time:

- **iOS:** does the QR code open in Safari? Does the profile appear under
  Settings → General → VPN & Device Management, and after installing it, does
  the CA show up in Certificate Trust Settings? Does the fingerprint under Settings → General →
  VPN & Device Management → the profile → More Details match the banner?
- **Android:** does `/.ferry/ferry-ca.crt` land in Downloads, and can it be
  installed from Settings → CA certificate?
- Both `https://<lan-ip>:6005/` and `https://<hostname>.local:6005/` should
  load without a warning.
- Switch the Mac to another network while ferry runs: within 30 seconds the
  log should show a reissue, and the phone should reach the new address
  without a warning.

After changing `sw.js`, bump `CACHE` in it, or the phone keeps serving the
old cached files.
