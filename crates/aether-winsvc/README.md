# aether-winsvc — AetherLink Windows service

A small `LocalSystem` service that keeps the AetherLink agent running in the interactive
session(s), so a remote operator can also control the **logon screen, the lock screen, the
Ctrl+Alt+Del screen and UAC prompts** — things a normal per-user process can never reach.

```text
aether-winsvc install [--agent <path>] [--include-remote-sessions] [--no-sas-policy] [-- <agent args>]
aether-winsvc uninstall
aether-winsvc status
aether-winsvc run --console        # supervisor in the foreground (debugging, e.g. `psexec -s`)
```

`install` (administrator) registers `AetherLinkService` (auto-start, LocalSystem, restart
on failure ×3 after 5 s), enables the `SoftwareSASGeneration` policy for services (so the
agent may send Ctrl+Alt+Del) and starts it. `uninstall` stops and deletes it and restores
the previous policy value.

## How it works

1. The service watches `WTSGetActiveConsoleSessionId()` and `SERVICE_CONTROL_SESSIONCHANGE`
   (plus, with `--include-remote-sessions`, every `WTSActive` session from
   `WTSEnumerateSessionsW`) and polls once a second as a safety net.
2. For each target session it duplicates **its own SYSTEM token** (`DuplicateTokenEx`,
   primary), moves the copy into the session (`SetTokenInformation(TokenSessionId)`, which
   requires `SeTcbPrivilege`), and starts the agent with `CreateProcessAsUserW` on
   `winsta0\default` when a user is logged on (`WTSQueryUserToken` succeeds) or
   `winsta0\winlogon` otherwise. The agent's injector and capturer then follow input-desktop
   switches themselves (`OpenInputDesktop` + `SetThreadDesktop`, DXGI duplication re-created
   on `ACCESS_LOST`).
3. The environment is SYSTEM's own (`CreateEnvironmentBlock` of the duplicated token) plus
   `AETHERLINK_LAUNCHED_BY_SERVICE=1`, `AETHERLINK_SESSION_ID`, `AETHERLINK_USER_LOGGED_ON`.
4. Agents run inside a `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` job: if the service dies, so do
   they. A crashed agent is relaunched with exponential backoff (1 s … 60 s, reset after
   60 s of stable running). Logon/logoff restarts the session's agent so it starts on the
   right desktop. Sessions that disappear get their agent terminated.

Logs: `%ProgramData%\AetherLink\logs\aether-winsvc.log` (`AETHERLINK_LOG=debug` for more).

## Security model

Running a remote-control agent as SYSTEM inside a user's session is deliberately powerful;
the design keeps that power from leaking to local users:

* **Binary integrity is the trust anchor.** Whoever can replace the agent or service binary
  gets SYSTEM. `install` refuses a service binary or agent outside `%ProgramFiles%`,
  `%ProgramFiles(x86)%` or `%SystemRoot%` — locations only administrators can write — and
  `run` re-checks the agent every time the service starts, unless
  `--allow-unprotected-agent-path` is given (development only; it prints a warning). Relative paths, UNC paths and `..` are always rejected. The command line is
  quoted per `CommandLineToArgvW` rules and `lpApplicationName` is passed explicitly, so a
  path with spaces can never be hijacked (`C:\Program.exe`).
* **No new network surface.** The service itself opens no sockets and has no IPC endpoint;
  it only starts processes. All remote access goes through the agent's authenticated QUIC
  session (TLS 1.3 + pinned Ed25519 keys or SPAKE2 one-time password, interactive consent
  on the host) — the same rules as the per-user app. Running as SYSTEM does not bypass them.
* **Who is controlled.** The agent can see and drive whatever is on the console (including
  another user's session after fast user switching and the logon screen). Unattended access
  must therefore be restricted to pinned, trusted peer keys; the one-time password flow
  still requires someone at the machine to read the password.
* **Ctrl+Alt+Del.** Windows only accepts a software SAS from services when the
  `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System\SoftwareSASGeneration`
  policy allows it (`1` = services). `install` sets bit 1 and remembers the old value;
  `--no-sas-policy` skips this (the agent then reports `PermissionDenied` for Ctrl+Alt+Del).
  `SendSAS` is loaded at run time from `sas.dll`.
* **Per-user state.** The SYSTEM agent's `%APPDATA%` is the system profile. Machine-wide
  identity/trust data should live under `%ProgramData%\AetherLink` (ACL'd to
  Administrators/SYSTEM); the agent must not read or write the logged-on user's profile with
  SYSTEM rights. Drive mappings (`aether-vfs` WebDAV mounts) made by a SYSTEM process are
  invisible to the user's Explorer — mounting stays a per-user-app feature.
* **Containment.** Agents run in a kill-on-close job; stopping or uninstalling the service
  removes them. The SCM restarts the service on failure; the supervisor rate-limits agent
  restarts so a crash loop cannot spin the CPU.

## Status

Compile-verified (clippy `-D warnings`, `x86_64-pc-windows-gnu` in Docker and
`x86_64-pc-windows-msvc` check from macOS); the portable CLI/backoff/quoting/path-policy logic
is unit-tested on every OS. **Not yet exercised on a real Windows machine.**
