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

Open: [http://localhost:3000](http://localhost:3000)

Downloaded files are saved in:

- `downloads/`

Optional cookie support for blocked/login-required sites:

- `RUSTUBE_COOKIES_FROM_BROWSER` (example: `chrome`, `firefox`, `safari`)
- `RUSTUBE_COOKIES_FILE` (path to Netscape-format cookies.txt)
- `RUSTUBE_COOKIES_HEADER` (raw `Cookie:` header value; server converts it to temporary Netscape cookies file)
- `RUSTUBE_YOUTUBE_COOKIE_HEADER` (same as above, legacy alias)
- `RUSTUBE_COOKIE_SCOOP_ENABLED` (`true` by default, set `false` to disable auto cookie-scoop attempts)
- `RUSTUBE_COOKIE_SCOOP_BIN` (default: `cookie-scoop`)
- `RUSTUBE_COOKIE_SCOOP_TIMEOUT_SECS` (default: `20`)
- `RUSTUBE_COOKIE_SCOOP_BROWSERS` (optional, comma-separated, e.g. `firefox,chrome`)
- `RUSTUBE_AUTO_COOKIE_BROWSER_RETRY` (default `false`; set `true` to probe fallback browser profiles automatically)

`cookie-scoop` integration:

- When enabled, Rustube runs `cookie-scoop --url <target>` (and `--header`) to collect cookies for the target URL.
- The temporary cookie file is passed to `yt-dlp --cookies ...` and then deleted after use.
- If `cookie-scoop` is not installed or fails, Rustube falls back to the existing cookie methods.
- On Linux servers without browser profiles, prefer `RUSTUBE_COOKIES_FILE` or `RUSTUBE_COOKIES_HEADER`.

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
