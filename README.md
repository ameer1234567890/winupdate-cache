# winupdate-cache

A small, self-hosted caching proxy for Windows Update downloads, written in Rust.
Point your DNS at it and every Windows device on your network downloads each update
from the internet only once.

* Single container, started with Docker Compose
* Web UI with dashboard, a devices page for naming IP addresses, cached-file browser, DNS snippets, and a dark mode switch
* Login with the username/password you set in `docker-compose.yml`
* Range-request aware (BITS / Delivery Optimization), concurrent clients share one download
* LRU eviction by size, plus expiry of files nobody has requested for a while

## Quick start

```bash
cp .env.example .env        # then edit ADMIN_PASSWORD etc.
docker compose up -d
```

This pulls `ameer1234567890/winupdate-cache:latest`. To build the image yourself instead, run
`docker build -t ameer1234567890/winupdate-cache:latest .` in this folder first (or swap `image:` for `build: .` in the compose file).

* Web UI: `http://<server>:8081` (default login `admin` / `changeme` – change it!)
* Cache endpoint: port `80` on the server (this is what DNS must point to)

## DNS

Make these names (and their subdomains) resolve to the server's IP:

```
download.windowsupdate.com
dl.delivery.mp.microsoft.com
download.microsoft.com
```

The UI's **DNS setup** page generates ready-to-paste snippets for dnsmasq/Pi-hole,
Unbound and Windows DNS Server.

**The cache must not use that DNS itself**, or it would call itself. Compose therefore
sets the container's resolvers to `1.1.1.1` / `9.9.9.9` (`UPSTREAM_DNS_1/2`). If your network
blocks outside DNS, use a resolver that doesn't carry the override. A request loop is
detected and answered with HTTP 508.

## Configuration (environment variables in `docker-compose.yml` / `.env`)

| Variable | Default | Meaning |
|---|---|---|
| `ADMIN_USER` | `admin` | UI username |
| `ADMIN_PASSWORD` | `changeme` | UI password (a warning is shown while it's the default) |
| `MAX_CACHE_SIZE_GB` | `200` | Disk budget; least recently used files are evicted beyond it |
| `CACHE_MAX_AGE_DAYS` | `90` | Remove files not requested for this long (`0` = never) |
| `ALLOWED_HOSTS` | the three domains above | Comma-separated host suffixes the cache will serve |
| `SESSION_HOURS` | `168` | Login session lifetime |
| `PROXY_PORT` / `UI_PORT` | `80` / `8081` | Published host ports |
| `DATA_DIR` | named volume | Volume name or host path (ownership is fixed automatically on start) |
| `CACHE_KEY_INCLUDES_QUERY` | `false` | By default the query string (expiring tokens) is ignored for the cache key |

## How it works

1. A client asks `download.windowsupdate.com` over HTTP; DNS sends it to the cache.
2. Complete file on disk: it is served directly (with `Range` support).
3. Otherwise the cache downloads the whole file from Microsoft *once*, writing to disk while
   streaming to the client. Other clients asking for the same file meanwhile follow the same download.
   A range far ahead of the download is passed through directly instead of waiting.
4. Only `200` responses with a known length are cached; everything else is passed through.
   Small files (< 4 MiB, e.g. `.cab` indexes) are re-checked upstream hourly.

## Limitations

* Only plain-HTTP content can be cached. Windows Update metadata and anything served over HTTPS
  (parts of Microsoft Store, Office CDN, etc.) goes straight to Microsoft. Windows update downloads
  are normally HTTP, but Microsoft can change this.
* Devices using DNS-over-HTTPS or a hard-coded DNS server bypass the cache.
* Peer-to-peer Delivery Optimization is unaffected; set the DO download mode to HTTP-only via
  Group Policy if you want every byte to pass through the cache.
* Log in over plain HTTP on a trusted LAN only, or put a TLS reverse proxy in front of the UI.

## Test it

```bash
curl -s -o /dev/null -D - -H "Host: download.windowsupdate.com" http://<server>/<any-update-path>
```
Run twice with a real update URL: the response header `x-cache` goes from `MISS` to `HIT`.

## License

MIT, see [LICENSE](LICENSE).
