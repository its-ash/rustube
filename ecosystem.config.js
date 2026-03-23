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
        RUST_LOG: "rustube=info,tower_http=info"
      }
    }
  ]
};
