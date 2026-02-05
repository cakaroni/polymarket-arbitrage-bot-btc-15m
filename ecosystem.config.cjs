module.exports = {
  apps: [
    {
      name: "polymarket-bot",
      script: "./target/release/polymarket-arbitrage-bot",
      args: "--simulation --config config.json",
      cwd: __dirname,
      interpreter: "none",
      autorestart: true,
      watch: false,
      max_memory_restart: "500M",
    },
  ],
};
