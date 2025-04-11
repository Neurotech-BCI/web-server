#!/bin/bash

echo "📦 ---Pulling files from Remote Repository--- 📦"
# Move to repo, pull from remote
cd web-server && git pull

echo "🔧 ---Testing web server configuration--- 🔧"
# Copy the webserver config to the configure system services directory
cp nginx.conf /etc/nginx/
# Restart the web server and test config
rc-service nginx restart
nginx -t

echo "✅ ---Production application updated--- ✅"