cp sshd_config /etc/ssh/sshd_config
cp authorized_keys /~/.ssh/authorized_keys
cp duckdns.sh /etc/cron.hourly/
chmod +x /etc/cron.hourly/duckdns.sh