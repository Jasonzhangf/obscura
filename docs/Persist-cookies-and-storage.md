`--storage-dir` persists the browser CookieJar and current document's
`localStorage` to disk so both survive across clean runs. `sessionStorage` is
not persisted because it is scoped to one browsing session. The profile is not
a complete Chromium profile: IndexedDB, CacheStorage, HTTP cache and service
worker storage are not yet persisted by this engine.

## CLI

```bash
obscura fetch https://example.com --storage-dir ./obscura-data
obscura fetch https://example.com --storage-dir ./obscura-data
```

The second invocation starts with the cookies and localStorage left by the first.

## Server

```bash
obscura serve --storage-dir ./obscura-data
```

All CDP sessions read and write to the same cookie jar. Run separate
`obscura serve` processes with different `--storage-dir` paths for isolated
profiles. The persistent Host uses the same option together with its required
socket directory:

```bash
obscura-host --socket-dir ./obscura-sockets --storage-dir ./obscura-data
```

## Layout

Inside `./obscura-data`:

- `cookies.json`: cookie jar in a stable format with `same_site`, `expires`, `http_only`, `secure`.
- `local-storage/<origin-bytes-hex>.json`: key/value entries for one serialized
  origin. The origin string is hex encoded so distinct origins cannot collide
  through filesystem naming.

The format is stable. Inspect with `jq`:

```bash
jq '.[] | select(.domain == "example.com")' ./obscura-data/cookies.json
```

## When state is written

- On clean process exit (Ctrl-C, SIGTERM), including the persistent Host.
- Before navigation replaces the current document.
- Before `suspend_js` replaces the current runtime.
- Before the persistent Host drops its Page for an explicit `close_session`.
- A crash or forced kill can lose changes since the last clean save.

Malformed localStorage files are reported as profile errors. They are not
silently treated as an empty storage area.

## Login once, scrape many

```bash
obscura serve --storage-dir ./session-1
```

Drive a login flow once via Puppeteer or Playwright. Stop the server cleanly.
Subsequent runs against the same `--storage-dir` send the saved cookies and
restore persisted localStorage entries for the same origin.

## Multiple identities

```bash
obscura serve --port 9222 --storage-dir ./identity-a
obscura serve --port 9223 --storage-dir ./identity-b
```

## Clear state

```bash
rm -rf ./obscura-data
```
