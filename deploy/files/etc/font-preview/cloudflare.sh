#!/bin/bash
# Cloudflare's address ranges for the firewall and Caddy's trusted proxies.
#
#   cloudflare.sh rules <v4-file>   iptables rules-save for these IPv4 ranges, on stdout
#   cloudflare.sh refresh           on the host: fetch the ranges; when they changed, replace the
#                                   DOCKER-USER chain and Caddy's trusted proxies. A failed or
#                                   implausible fetch keeps the current set.
set -euo pipefail
ETC=/etc/font-preview

rules() {
	echo '*filter'
	echo ':INPUT DROP [0:0]'
	echo ':FORWARD DROP [0:0]'
	echo ':OUTPUT ACCEPT [0:0]'
	echo ':DOCKER-USER - [0:0]'
	echo '-A INPUT -i lo -j ACCEPT'
	echo '-A INPUT -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT'
	echo '-A INPUT -p icmp -j ACCEPT'
	echo '-A INPUT -p tcp --dport 22 -j ACCEPT'
	docker_user "$1"
	echo 'COMMIT'
}

# Published ports are reachable from Cloudflare only.
docker_user() {
	echo '-A DOCKER-USER -m conntrack --ctstate RELATED,ESTABLISHED -j RETURN'
	echo '-A DOCKER-USER -i docker+ -j RETURN'
	echo '-A DOCKER-USER -i br-+ -j RETURN'
	local r
	while read -r r; do
		echo "-A DOCKER-USER -s $r -p tcp -m conntrack --ctorigdstport 80 -j RETURN"
		echo "-A DOCKER-USER -s $r -p tcp -m conntrack --ctorigdstport 443 -j RETURN"
	done <"$1"
	echo '-A DOCKER-USER -j DROP'
}

fetch() {
	local v=$1 out=$2
	curl -fsS --max-time 20 "https://www.cloudflare.com/ips-$v" | grep -E '^[0-9a-f.:]+/[0-9]+$' >"$out" || return 1
	# Cloudflare publishes about 15 IPv4 and 7 IPv6 ranges; far fewer means a bad response.
	[[ $(wc -l <"$out") -ge 4 ]]
}

refresh() {
	local tmp
	tmp=$(mktemp -d)
	trap 'rm -rf "$tmp"' EXIT
	if ! fetch v4 "$tmp/v4" || ! fetch v6 "$tmp/v6"; then
		echo "cloudflare.sh: fetch failed; keeping the current ranges" >&2
		exit 0
	fi
	if cmp -s "$tmp/v4" "$ETC/cloudflare-v4.txt" && cmp -s "$tmp/v6" "$ETC/cloudflare-v6.txt"; then
		exit 0
	fi
	{ echo '*filter'; echo ':DOCKER-USER - [0:0]'; docker_user "$tmp/v4"; echo 'COMMIT'; } >"$tmp/docker-user"
	# --noflush replaces only the declared DOCKER-USER chain, in one transaction.
	iptables-restore --noflush <"$tmp/docker-user"
	rules "$tmp/v4" >/var/lib/iptables/rules-save
	cp "$tmp/v4" "$ETC/cloudflare-v4.txt"
	cp "$tmp/v6" "$ETC/cloudflare-v6.txt"
	sed -i "s|^FPS_TRUSTED_PROXIES=.*|FPS_TRUSTED_PROXIES=$(cat "$tmp/v4" "$tmp/v6" | tr '\n' ' ' | sed 's/ $//')|" "$ETC/site.env"
	systemctl restart fps-proxy.service
	echo "cloudflare.sh: ranges changed; firewall and proxy updated"
}

case ${1:-} in
rules) rules "$2" ;;
refresh) refresh ;;
*) echo "usage: cloudflare.sh rules <v4-file> | refresh" >&2; exit 2 ;;
esac
