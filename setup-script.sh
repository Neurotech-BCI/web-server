#!/bin/bash

echo "📦 ---Pulling files from Remote Repository--- 📦"
# Move to repo, pull from remote
cd /root/web-server || exit 1
git pull

echo
echo "🔧 --- Updating NGINX server configuration --- 🔧"
cp nginx.conf /etc/nginx/
cp bci-uscneuro.tech /etc/nginx/sites-available/bci-uscneuro.tech
ln -sf /etc/nginx/sites-available/bci-uscneuro.tech \
       /etc/nginx/sites-enabled/bci-uscneuro.tech

echo
echo "📁 --- Syncing front-end build to /var/www/bci-uscneuro.tech --- 📁"
rsync -a --delete /root/web-server/web-build /var/www/bci-uscneuro.tech

echo
echo "🛑 --- Shutting down any existing processes on ports 5000, 6000, and 8000 --- 🛑"
for port in 5000 6000 8000; do
    pids=$(lsof -t -i:$port 2>/dev/null)
    if [ -n "$pids" ]; then
        echo "⚠️  Killing process(es) on port $port: $pids"
        kill -9 $pids
    else
        echo "✅ No process found on port $port"
    fi
done

echo
echo "🎯 --- Starting Dart test server (port 5000) --- 🎯"
cd /root/web-server/backend || exit 1
dart run testserver.dart &

echo
echo "🦀 --- Building and starting Actix Web server (port 6000) --- 🦀"
cd /root/web-server/rust-backend || exit 1
echo "🔨 Building Rust project..."
cargo build --release
echo "🚀 Launching Actix backend..."
cargo run --release &

echo
echo "🧠 --- Starting Inference API with Uvicorn (port 8000) --- 🧠"
# Activate Python virtual environment
source /root/bin/activate
echo "📦 Installing Python dependencies quietly..."
pip3 install -q -r /root/InferenceAPI/requirements.txt
echo "🚀 Launching inference_api:app..."
cd /root/InferenceAPI || exit 1
uvicorn inference_api:app --reload >> /var/log/uvicorn.log 2>&1 &

echo
echo "🕒 --- Waiting for services to become available --- 🕒"
wait_for_port() { # polls a port until it becomes open
    local port=$1
    local timeout=60
    local elapsed=0

    echo -n "⏳ Waiting for port $port to open... "
    while ! nc -z localhost $port >/dev/null 2>&1; do
        sleep 1
        elapsed=$((elapsed + 1))
        if [ $elapsed -ge $timeout ]; then
            echo "❌ Timed out after $timeout seconds."
            return 1
        fi
    done
    echo "✅ Open!"
}

wait_for_port 5000
wait_for_port 6000
wait_for_port 8000 # uvicorn takes a while to start, so no worries if this gets stuck

echo
echo "✅ --- Testing and restarting NGINX configuration --- ✅"
nginx -t && systemctl restart nginx

echo "🚀 ---Production application updated--- 🚀"

