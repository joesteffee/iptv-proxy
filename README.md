# IPTV Proxy

A reverse proxy for IPTV M3U playlists and Xtream Codes server APIs. It rewrites a subscription so
that every stream URL points at this proxy instead of the provider, then forwards the streams, the
API and the EPG on behalf of the client.

The application is a single static Rust binary in a `FROM scratch` container image.

## Quick start

```bash
docker run -d \
  --name iptv-proxy \
  -p 8080:8080 \
  -v ./iptv:/root/iptv \
  -e M3U_URL="https://example.com/playlist.m3u" \
  -e HOSTNAME="iptv.example.com" \
  -e USER="myuser" \
  -e PASSWORD="mypassword" \
  ghcr.io/joesteffee/iptv-proxy:latest
```

Then point a player at `http://iptv.example.com:8080/iptv.m3u?username=myuser&password=mypassword`.

Pass flags instead of environment variables with `command:` / unRAID "Extra Parameters":

```yaml
command: ["--m3u-url", "https://example.com/playlist.m3u", "--port", "9000"]
```

### unRAID

Pull `ghcr.io/joesteffee/iptv-proxy:latest`. The repository is public, so no registry credentials
are needed. The image contains both `linux/amd64` and `linux/arm64`. Map container port `8080` to
the host port you want, and set `PORT` to the same value. To keep using a local playlist file, add
the path `/root/iptv` as an application variable (for example `/mnt/user/media/iptv` on the host) —
that is the directory the default `M3U_URL=/root/iptv/iptv.m3u` and the default
`CATEGORY_FILTERS_PATH=/root/iptv/category_filters.json` both read.

### docker compose

`docker-compose.yml` in this repository is a working example. `traefik/` labels work the same way:
route to container port 8080 and set `HOSTNAME`, `ADVERTISED_PORT` and `HTTPS` to the public
address.

## Configuration

Every setting can be given as a flag or as an environment variable. The variable name is the flag
name in upper case with `-` replaced by `_`. An explicit flag wins over the variable, which wins
over the default. An empty variable counts as unset.

| Environment variable | Flag | Default | Meaning |
| --- | --- | --- | --- |
| `M3U_URL` | `--m3u-url` | *(empty)* | Subscription playlist: an `http(s)` URL or a local path such as `/root/iptv/iptv.m3u` |
| `M3U_FILE_NAME` | `--m3u-file-name` | `iptv.m3u` | File name the playlist is served under |
| `CUSTOM_ENDPOINT` | `--custom-endpoint` | *(empty)* | Path prefix added in front of every route, for example `tv` |
| `CUSTOM_ID` | `--custom-id` | *(random per start)* | Fixed segment used to separate stream routes in plain-M3U mode |
| `PORT` | `--port` | `8080` | Port the proxy listens on |
| `ADVERTISED_PORT` | `--advertised-port` | `PORT` | Port written into generated URLs (use the public port behind a reverse proxy) |
| `HOSTNAME` | `--hostname` | *(empty)* | Host written into generated URLs |
| `HTTPS` | `--https` | `false` | Write `https://` into generated URLs |
| `USER` | `--user` | `usertest` | Username clients must present |
| `PASSWORD` | `--password` | `passwordtest` | Password clients must present |
| `XTREAM_USER` | `--xtream-user` | *(detected)* | Provider username |
| `XTREAM_PASSWORD` | `--xtream-password` | *(detected)* | Provider password |
| `XTREAM_BASE_URL` | `--xtream-base-url` | *(detected)* | Provider base URL, e.g. `http://provider.example:8080` |
| `M3U_CACHE_EXPIRATION` | `--m3u-cache-expiration` | `1` | Hours a generated or fetched playlist stays valid |
| `XTREAM_API_GET` | `--xtream-api-get` | `false` | Serve `get.php` from the Xtream API instead of passing the provider playlist through |
| `RATE_LIMIT_RETRY_TIMEOUT` | `--rate-limit-retry-timeout` | `10` | Minutes to keep retrying a provider that answers `458` (too many connections) |
| `CATEGORY_FILTERS_PATH` | `--category-filters-path` | `/root/iptv/category_filters.json` | Where the category selection from the web UI is stored. Empty keeps it in memory only |
| `REDIS_URL` | `--redis-url` | *(empty)* | Redis address used to cache provider catalogue data across restarts, e.g. `redis://127.0.0.1:6379` |
| `RUST_LOG` | — | `info` | Log filter, e.g. `RUST_LOG=iptv_proxy=debug` |

`--iptv-proxy-config` is still accepted and ignored. The Go build never managed to read that file,
so stacks that still pass it keep starting.

### Choosing a mode

- **Plain M3U**: set `M3U_URL` to a playlist that is not an Xtream `get.php` subscription. The
  playlist is read once at start-up, every track URL is rewritten to
  `http(s)://{HOSTNAME}:{ADVERTISED_PORT}/{CUSTOM_ID}/{USER}/{PASSWORD}/{index}/{file}` and each
  track is streamed from the provider on demand.
- **Xtream**: set `XTREAM_BASE_URL`, `XTREAM_USER` and `XTREAM_PASSWORD`. Playlists are generated
  from the provider API, filtered by the categories enabled in the web UI at `/`.
- **Automatic Xtream**: if `M3U_URL` is a `get.php` subscription and the three `XTREAM_*` settings
  are empty, the credentials are read from the URL and the provider is the URL's host. `M3U_FILE_NAME`
  then serves the generated playlist.

## Endpoints

Plain-M3U mode:

| Path | Notes |
| --- | --- |
| `GET /` | Web UI: category manager (Xtream mode only — otherwise it reports that no provider is configured) |
| `GET /api/categories`, `POST /api/categories` | Read and change the enabled categories |
| `GET /{M3U_FILE_NAME}` | The rewritten playlist (`username`/`password` required, `Range` supported) |
| `GET /{CUSTOM_ID}/{USER}/{PASSWORD}/{index}/{file}` | Stream one track from the playlist |

Xtream mode (in place of the last two rows):

| Path | Notes |
| --- | --- |
| `GET /get.php` | Playlist built from the provider API |
| `GET /apiget` | Playlist from the Xtream API (`XTREAM_API_GET`) |
| `GET`/`POST /player_api.php` | Authenticated Xtream API: categories, streams, series info, VOD info, EPG, login |
| `GET /xmltv.php` | EPG |
| `GET /live/{USER}/{PASSWORD}/{id}` | Live stream (a trailing `.ts` is ignored) |
| `GET /movie/{USER}/{PASSWORD}/{id}` | Movie |
| `GET /series/{USER}/{PASSWORD}/{id}` | Series episode |
| `GET /timeshift/{USER}/{PASSWORD}/{duration}/{start}/{id}` | Timeshift |
| `GET /{USER}/{PASSWORD}/{id}` | Generic stream |
| `GET /hls/{token}/{chunk}`, `GET /hlsr/{token}/{USER}/{PASSWORD}/{channel}/{hash}/{chunk}` | HLS segments |
| `GET /play/{token}/{type}` | Token-based play route |

`CUSTOM_ENDPOINT` prefixes every path above.

## Behaviour worth knowing

- **Rate limiting.** Providers answer status `458` when the connection limit is reached. The proxy
  waits and retries for up to `RATE_LIMIT_RETRY_TIMEOUT` minutes, then passes the `458` through.
  While one request is retrying, other requests for the same URL wait instead of adding load.
- **Redirects.** Up to 10 are followed; beyond that the request fails with `502` and
  `Too many redirects (N), possible redirect loop`.
- **Caching.** Generated playlists are written to the container's temp directory and reused until
  `M3U_CACHE_EXPIRATION` hours have passed. Concurrent requests for the same playlist share one
  build. With `REDIS_URL` set, catalogue data is cached in Redis for 7 days and refreshed in the
  background before it expires.
- **Fetching.** Categories and per-category stream lists are fetched concurrently and cached, and
  the provider login is reused for an hour. A full playlist build is much faster than one request
  per category.

## Differences from the Go release

The wire behaviour is the same: same environment variables, same flags, same routes, same playlist
bytes, same status codes and error texts. The following differences are deliberate.

1. **HTTP/2 to the provider** is negotiated with ALPN instead of prior-knowledge h2c. Plain HTTP/1.1
   is unaffected.
2. **Hop-by-hop headers** (`Connection`, `Keep-Alive`, `Proxy-Connection`, `Transfer-Encoding`,
   `Upgrade`) are not copied between client and provider. They were copied verbatim before, which is
   incorrect for a proxy.
3. **The provider login is cached** for one hour. The Go build logged in again for every
   `player_api.php` call.
4. **Playlists larger than 64 KiB per line** are accepted. The Go reader stopped at 64 KiB lines.
5. **A tag value containing `=`** keeps its full value; the Go parser truncated it at the second `=`.
6. **Seasons are listed in provider order** and generated playlists are stable between runs; the Go
   build iterated a map, so multi-season series could change order.
7. **Startup work is smaller.** In automatic Xtream mode the Go build downloaded the playlist twice
   at start-up; this build does not, so it starts faster and puts less load on the provider.
8. **JSON strings are not HTML-escaped.** Go's encoder escaped `<`, `>` and `&`; that escaping is not
   applied, so a provider name containing those characters is written literally.
9. **Invalid configuration fails fast** with one clear message per problem. Go accepted some invalid
   values and ignored them.

## Development

Rust 1.85 or newer.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
```

The test suites:

- **Unit tests** inside `src/` — M3U parsing and marshalling, URL rewriting, configuration, Xtream
  models, caching.
- `tests/golden_m3u.rs` — replays a 65-line slice of a playlist produced by the Go release
  (`tests/fixtures/go-playlist-slice.m3u`) and requires the same bytes out.
- `tests/playlist_contract.rs` — plain-M3U mode end to end against a mock provider: playlist bytes,
  download headers, authentication, byte ranges, streaming, redirects, `458` retry, the web UI.
- `tests/xtream_contract.rs` — Xtream mode end to end: playlist generation, category filters,
  caching, every stream route, HLS redirects, the player API and EPG, and automatic detection from a
  `get.php` URL.
- `tests/environment.rs` — pins the flag set, the defaults and the environment-variable precedence to
  the Go release.

### Container

`docker build -t iptv-proxy:latest .` produces the static image. Pushing to `main` or tagging a
`v*` release runs `.github/workflows/publish-container.yml`, which builds `linux/amd64` and
`linux/arm64` and publishes `ghcr.io/joesteffee/iptv-proxy` with `latest`, the branch name, the
semantic version and the commit SHA.

## License

GPL-3.0-only — see `LICENSE` (unchanged from the Go release).
