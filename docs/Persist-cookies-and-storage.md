`--storage-dir` persists the browser CookieJar to disk so cookies survive across
runs. The current engine does not persist `localStorage` or `sessionStorage`.

## CLI

```bash
obscura fetch https://example.com --storage-dir ./obscura-data
obscura fetch https://example.com --storage-dir ./obscura-data
```

The second invocation starts with the cookies left by the first.

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

The format is stable. Inspect with `jq`:

```bash
jq '.[] | select(.domain == "example.com")' ./obscura-data/cookies.json
```

## When state is written

- On clean process exit (Ctrl-C, SIGTERM), including the persistent Host.
- Before the persistent Host drops its Page for an explicit `close_session`.
- A crash or forced kill can lose changes since the last clean save.

## Login once, scrape many

```bash
obscura serve --storage-dir ./session-1
```

Drive a login flow once via Puppeteer or Playwright. Stop the server cleanly.
Subsequent runs against the same `--storage-dir` send the saved cookies.

## Multiple identities

```bash
obscura serve --port 9222 --storage-dir ./identity-a
obscura serve --port 9223 --storage-dir ./identity-b
```

## Clear state

```bash
rm -rf ./obscura-data
```
