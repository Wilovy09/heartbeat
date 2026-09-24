#!/usr/bin/env bash
# One-time bootstrap for a fresh Ubuntu 24.04 EC2 box that will run heartbeat under
# pm2, behind nginx with a Let's Encrypt cert.
#
# Usage: ./prepare_ec2.sh [project_dir]   (default: /arena/heartbeat)
set -e
PROJECT_DIR="${1:-/arena/heartbeat}"

sudo apt update
sudo apt install -y nginx certbot python3-certbot-nginx nodejs npm

if ! command -v cargo &>/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
fi

if ! command -v pm2 &>/dev/null; then
    sudo npm install -g pm2
fi

cd "$PROJECT_DIR"
cargo build --release

echo
echo "Build done. Next steps (see README.md 'Despliegue en EC2'):"
echo "  1. cp .env.example .env   # then fill in real prod values (LOGIN_URL, COOKIE_SECURE=true)"
echo "  2. pm2 start ./target/release/heartbeat --name heartbeat --cwd $PROJECT_DIR"
echo "  3. pm2 save && pm2 startup   # survive reboots"
echo "  4. Add an nginx server block (see deploy/nginx.conf.example) and run certbot"
