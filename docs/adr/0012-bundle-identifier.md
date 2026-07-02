# 12. Bundle identifier — io.github.seahsky.skillmon

Status: Accepted.

## Context

Tauri requires a unique reverse-DNS `identifier` in `tauri.conf.json`.
It drives macOS/Windows app identity, the Windows notification AppUserModelID, the webview data dir, autostart, and the updater — changing it later reissues all of that, so treat it as stable.

## Decision

Use **io.github.seahsky.skillmon** (the `io.github.<user>.<project>` pattern, standard when you don't own a domain, and guaranteed unique via the GitHub namespace).

An earlier pick, `com.skillmon.app`, was rejected: reverse-DNS is `<tld>.<org>.<appname>`, so that form makes the app-name segment literally "app" and ends in the reserved macOS `.app` bundle extension — a documented footgun. The app name belongs in the last segment.

## Consequences

- Allowed chars only: `A-Z a-z 0-9 - .`; no underscores; not a default; must not end in `.app`; each segment should start with a letter (Android compatibility).
