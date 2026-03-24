module.exports = {
  apps: [
    {
      name: "rustube",
      cwd: __dirname,
      script: "bash",
      args: ["-lc", "cargo build --release && exec ./target/release/rustube"],
      interpreter: "none",
      instances: 1,
      exec_mode: "fork",
      autorestart: true,
      restart_delay: 3000,
      max_restarts: 20,
      min_uptime: "10s",
      out_file: "./logs/rustube-out.log",
      error_file: "./logs/rustube-error.log",
      merge_logs: true,
      env: {
        RUST_LOG: "rustube=info,tower_http=info",
        // Optional auth inputs for blocked providers (YouTube/Instagram/etc):
        RUSTUBE_COOKIES_FILE: process.env.RUSTUBE_COOKIES_FILE || "",
        RUSTUBE_COOKIES_HEADER: process.env.RUSTUBE_COOKIES_HEADER || "",
        RUSTUBE_COOKIES_FROM_BROWSER: process.env.RUSTUBE_COOKIES_FROM_BROWSER || "",
        RUSTUBE_COOKIE_SCOOP_ENABLED: process.env.RUSTUBE_COOKIE_SCOOP_ENABLED || "true",
        RUSTUBE_COOKIE_SCOOP_BIN: process.env.RUSTUBE_COOKIE_SCOOP_BIN || "cookie-scoop",
        RUSTUBE_COOKIE_SCOOP_TIMEOUT_SECS: process.env.RUSTUBE_COOKIE_SCOOP_TIMEOUT_SECS || "20",
        RUSTUBE_AUTO_COOKIE_BROWSER_RETRY: process.env.RUSTUBE_AUTO_COOKIE_BROWSER_RETRY || "false"
      }
    }
  ]
};
