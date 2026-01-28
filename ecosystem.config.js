module.exports = {
  apps: [{
    name: 'polymarket-bot',
    script: './target/release/polymarket-arbitrage-bot',
    cwd: '/root/rust-pro/polymarket-arbitrage-bot-ultra',
    instances: 1,
    autorestart: true,
    watch: false,
    max_memory_restart: '1G',
    env: {
      RUST_LOG: 'info'
    },
    error_file: './logs/pm2-error.log',
    out_file: './logs/pm2-out.log',
    log_date_format: 'YYYY-MM-DD HH:mm:ss Z'
  }]
};
