# Rustube Queue Downloader

Rust web server + modern browser UI for downloading videos from providers supported by `yt-dlp` (including YouTube, Instagram, and Reddit).

## Features

- Paste a video URL and fetch available formats
- See resolution, extension, and estimated size
- Add multiple videos/formats to a queue
- Start all downloads in parallel (threaded async workers)
- Track live progress per file in the browser
- Persist queue/download state across page refresh
- Format toggle:
  - default shows only combined audio+video formats
  - checkbox shows audio-only / video-only formats
- For YouTube-style split streams, combined audio+video options are auto-generated as `video+bestaudio` merge choices
- Download completed files from the UI
  - output file names use the original video title
- If `yt-dlp` download fails, server attempts a custom Rust direct-media fallback
- Failed downloads can be retried or removed directly from the UI
- `Clear Completed` removes completed jobs and deletes their files from server storage
- Files older than 1 day are auto-deleted from `downloads/` by background cleanup
- Dark theme UI

Note:
- Works with URLs supported by `yt-dlp`.
- Some websites may block automated requests; in those cases, use cookies.
- The Rust fallback can help on simple pages with direct `<video>/<source>` links, but cannot bypass hard anti-bot restrictions.

## Requirements

- Rust (stable)
- `yt-dlp` available in `PATH`
- `ffmpeg` (recommended for HLS/DASH fallback streams like `.m3u8` / `.mpd`)

Install `yt-dlp` (example):

```bash
python3 -m pip install -U yt-dlp
```

## Run

```bash
cargo run
```

For PM2 deploys, environment variables can be stored in `.env` (auto-loaded by `ecosystem.config.js`).

Open: [http://localhost:3000](http://localhost:3000)

Downloaded files are saved in:

- `downloads/`

Optional cookie support for blocked/login-required sites:

- `RUSTUBE_COOKIES_FROM_BROWSER` (example: `chrome`, `firefox`, `safari`)
- `RUSTUBE_COOKIES_FILE` (path to Netscape-format cookies.txt)
- `RUSTUBE_COOKIES_HEADER` (raw `Cookie:` header value; server converts it to temporary Netscape cookies file)
- `RUSTUBE_YOUTUBE_COOKIE_HEADER` (same as above, legacy alias)
- `RUSTUBE_COOKIE_SCOOP_ENABLED` (`false` by default, set `true` to enable cookie-scoop attempts)
- `RUSTUBE_COOKIE_SCOOP_BIN` (default: `cookie-scoop`)
- `RUSTUBE_COOKIE_SCOOP_TIMEOUT_SECS` (default: `20`)
- `RUSTUBE_COOKIE_SCOOP_BROWSERS` (optional, comma-separated, e.g. `firefox,chrome`)
- `RUSTUBE_AUTO_COOKIE_BROWSER_RETRY` (default `false`; set `true` to probe fallback browser profiles automatically)

### Scraping Cookies with Puppeteer

For sites requiring authentication, Rustube automatically uses Puppeteer to scrape cookies in headless mode:

- By default, Puppeteer is enabled (`RUSTUBE_PUPPETEER_ENABLED=true`)
- When downloading from sites that need cookies, Rustube automatically launches a headless browser
- The browser navigates to the target URL and waits for cookies to be set
- Cookies are saved in Netscape format and passed to yt-dlp
- Set `RUSTUBE_PUPPETEER_ENABLED=false` to disable this feature
- Configure timeout with `RUSTUBE_PUPPETEER_TIMEOUT_SECS` (default: 30 seconds)

Manual cookie scraping (legacy):

1. Install Node.js dependencies: `npm install`
2. Run the scraper: `node scrape-cookies.js <URL>` (e.g., `node scrape-cookies.js https://www.youtube.com`)
3. The script runs headless and automatically saves cookies to `cookies.txt`
4. The cookies will be used automatically if `RUSTUBE_COOKIES_FILE=./cookies.txt` is set (default in `.env`).

`cookie-scoop` integration (fallback):

- When Puppeteer fails or is disabled, Rustube tries `cookie-scoop` if enabled
- The temporary cookie file is passed to `yt-dlp --cookies ...` and then deleted after use
- If `cookie-scoop` is not installed or fails, Rustube falls back to the existing cookie methods
- If `RUSTUBE_COOKIES_FILE` is set, Rustube uses that file and skips automatic cookie extraction
- On Linux servers without browser profiles, prefer `RUSTUBE_COOKIES_FILE` or `RUSTUBE_COOKIES_HEADER`
- For YouTube bot-check failures, make sure cookies are from a signed-in YouTube account (look for cookies like `SID` / `HSID` / `SSID` / `SAPISID`).

Example:

```bash
RUSTUBE_COOKIES_FROM_BROWSER=chrome cargo run
```

## API

- `POST /api/options`
  - body: `{ "url": "https://..." }`
  - returns: formats list

- `POST /api/downloads/start`
  - body:
    ```json
    {
      "items": [
        { "url": "https://...", "format_id": "..." }
      ]
    }
    ```
  - starts multiple downloads concurrently

- `POST /api/downloads/status`
  - body: `{ "ids": ["job1", "job2"] }`
  - returns current status/progress for each job

- `GET /api/downloads/all`
  - returns all jobs currently tracked by the server

- `POST /api/downloads/clear-completed`
  - removes completed jobs and deletes their files

- `POST /api/downloads/remove`
  - body: `{ "ids": ["job1", "job2"] }`
  - removes selected jobs and deletes associated files (if present)

- `GET /downloads/<file_name>`
  - serves completed files
