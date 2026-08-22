# Security

## Reporting

Please report vulnerabilities privately via [GitHub's private vulnerability reporting](https://github.com/mattcree/meshcast/security/advisories/new) rather than a public issue. You should get an acknowledgement within a few days. This is a hobby project; there is no bounty, but fixes will be prioritised.

## Scope and model (short version — full detail in `docs/DESIGN.md` §5)

- **Consent**: nothing is captured until the streamer clicks *Share Screen* in the app (and picks a source in the OS portal on Wayland). Requests name the Discord server and expire after 90 s.
- **Credentials**: each bot↔user link is a random 32-byte gossip topic exchanged during a one-shot, 10-minute, 8-char-PIN pairing over QUIC/TLS. The daemon only accepts signals from the paired bot's endpoint ID.
- **Secrets at rest**: `~/.config/meshcast/config.toml` (link topics + daemon keys), bot `state.json` (bot key + topics), bot token env file — all `0600`, written atomically. They are never logged.
- **Discord**: non-privileged intents only, no message-content access, only *Send Messages* + *Embed Links* requested.
- **Viewer launch**: `WatchStream` from a paired bot opens `meshcast watch <ticket>`; tickets are validated (character set, length) and concurrently open viewers are capped at 5. The viewer only connects to the ticket's endpoint and renders media.
- **Transport**: all network traffic is iroh QUIC (TLS 1.3, endpoint keys). iroh's public relays may carry encrypted traffic when hole-punching fails; they can't read it.

## Known limitations

- Stream tickets are handed to any server member who clicks Watch (fallback for unlinked viewers), so a stream is as private as the channel it's posted in. Role gating is on the backlog.
- A guessed live PIN (40 bits, 10 min, one-shot) plus the bot's public endpoint ID would allow hijacking that one pairing attempt.
- Binaries are not code-signed yet (macOS/Windows warnings; `install.sh` clears the quarantine flag).

## Supported versions

Only the latest release receives fixes.
