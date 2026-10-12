# Native resolution canary

The native canary fails when signed-in Rust resolution fails or exceeds its
latency budget. A successful fallback cannot make the canary pass. Run it
against each candidate release and on the installed release every 15 minutes.

## Pass conditions

The canary resolves three samples in one process. It calls
`resolve_traced` with only `NativeSignedIn` for every sample. It skips local
files, the stream cache, anonymous resolution and the yt-dlp extractor.
Native resolution still uses its Node solver and token provider dependencies.

| Probe | Sample | Expected tier | Default budget |
| --- | --- | --- | --- |
| 1 | `1-Fxb9jsOuI` | Premium, itag 774 or 141 | 3000 ms |
| 2 | `Bq56cLjoIgo` | Standard, itag 251 or 140 | 1000 ms |
| 3 | `1-Fxb9jsOuI` | Premium, itag 774 or 141 | 1000 ms |

Probe 1 measures startup in a fresh process with the existing disk cache.
Probes 2 and 3 measure the same process with its resident solver and context.
The canary does not clear the player script cache or use a cached stream URL.
Each probe must produce exactly one signed-in attempt and a CDN 200 or 206.
A different tier fails even if the stream is playable.

These budgets are acceptance targets, not measured performance claims. The
whole canary has a 6000 ms deadline. Probe timeouts cancel the current request.
The resolver is dropped at completion to end its resident solver process.
The user service also kills its process group after 10 seconds.

The Premium sample requires a signed-in Premium account. Changed availability,
expired cookies, a network outage or an upstream protocol change all fail the
check. Review the failure before treating it as a code regression.

## Release gate

After the repository commit checks pass, build the candidate release. Run:

```sh
target/release/redwax-resolve --native-canary
```

Exit 0 means every sample met its native, tier, CDN and latency conditions.
Every other exit code blocks a release that claims native playback is healthy.
The generic `--health` command is not this gate. It checks fallback paths too.

Repeat the canary after installation. The final JSON report includes the
package version and a SHA-1 fingerprint of the running executable. It contains
no executable path. The fingerprint identifies a build and is not a signature
or authenticity check. A failed fingerprint read produces a null value.
A passing run establishes the sampled behavior at that time. It does not
guarantee that every track or future YouTube player version will work.

For a different measured deployment budget, set explicit limits:

```sh
redwax-resolve --native-canary --cold-budget-ms 3000 --warm-budget-ms 1000 --deadline-ms 6000
```

Each limit must be between 1 and 60000 milliseconds. Cookies, account headers
and player cache paths use the resolver defaults. The normal `--cookies`,
`--authuser`, `--page-id` and `--cache-dir` overrides also apply.

## Recurring checks

After the release gate, build this checkout and enable the timer:

```sh
./install.sh --build --native-timer
```

The installer builds the workspace and runs the candidate native canary before
it overwrites any installed executable. A failed canary stops installation.
After installation, it runs the installed native canary before enabling the
user timer. A failed installed canary stops timer activation. Installation
without `--native-timer` does not run these account-dependent checks.
The timer runs every 15 minutes with up to 30 seconds of jitter. A missed check
runs when the user timer starts again. It does not wake a suspended machine.

Inspect failures locally:

```sh
systemctl --user status redwax-native.service
journalctl --user -u redwax-native.service --since today
```

Output includes probe numbers, phase, expected tier, fixed status labels,
timings, package version and executable fingerprint. It excludes video IDs,
account identifiers, signed URLs, cookies,
tokens and resolver error text. The service sends no external notifications.
A failed service remains visible in systemd until the next invocation changes
its state. The timer does not repair protocol changes or deploy source edits.

## Related links

- [Installation](../README.md#install)
- [Resolver commands](usage.md#redwax-resolve)
