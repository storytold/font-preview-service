#!/usr/bin/env bash
# Deploys font-preview-service to a Flatcar host over SSH. Run from any Linux machine with ssh,
# tar and docker; nothing runs on the host except systemd, docker and iptables.
#
#   deploy.sh setup    <ssh-dest> <domain> <image@sha256:…>   first install, or re-apply files
#   deploy.sh deploy   <ssh-dest> <image@sha256:…>            new service image, rolls back if unhealthy
#   deploy.sh rollback <ssh-dest>                             previous service image
#   deploy.sh status   <ssh-dest>
#   deploy.sh digest   <image:tag>                            prints image@sha256 for a tag
#   deploy.sh ignition <domain> <image@sha256:…> <out.ign>    Ignition config for a new host
#
# <ssh-dest> is anything ssh accepts (user@host or an alias from ~/.ssh/config). The user needs
# passwordless sudo. No host names live in this repository.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=versions.env
# shellcheck disable=SC1091
. "$HERE/versions.env"

# Rootless docker on the deploying machine, when no DOCKER_HOST is set.
if [[ -z ${DOCKER_HOST:-} && ! -S /var/run/docker.sock && -S /run/user/$(id -u)/docker.sock ]]; then
	DOCKER_HOST=unix:///run/user/$(id -u)/docker.sock
	export DOCKER_HOST
fi

die() { echo "deploy.sh: $*" >&2; exit 1; }
IMAGE_RE='^[a-z0-9][a-z0-9._/-]*(:[A-Za-z0-9._-]+)?@sha256:[0-9a-f]{64}$'
need_digest() { [[ $1 =~ $IMAGE_RE ]] || die "image must be name@sha256:<64 hex>, got $1"; }
remote() { local dest=$1; shift; ssh -o BatchMode=yes "$dest" "$@"; }

cloudflare_ranges() {
	local v=$1
	curl -fsS --max-time 20 "https://www.cloudflare.com/ips-$v" | grep -E '^[0-9a-f.:]+/[0-9]+$' \
		|| die "could not fetch Cloudflare's $v ranges"
}

# Writes the host's file tree into $1: the static files plus the generated ones.
render() {
	local out=$1 domain=$2 image=$3
	cp -a "$HERE/files/." "$out/"
	install -d "$out/etc/font-preview" "$out/var/lib/iptables" "$out/var/lib/ip6tables" "$out/etc/flatcar"
	cloudflare_ranges v4 >"$out/etc/font-preview/cloudflare-v4.txt"
	cloudflare_ranges v6 >"$out/etc/font-preview/cloudflare-v6.txt"
	# verify_if_given until Cloudflare's Authenticated Origin Pulls is on for the zone; then
	# run setup with ORIGIN_PULL=require.
	local client_auth=verify_if_given
	[[ ${ORIGIN_PULL:-} == require ]] && client_auth=require_and_verify
	printf 'FPS_DOMAIN=%s\nFPS_TRUSTED_PROXIES=%s\nFPS_CLIENT_AUTH=%s\n' "$domain" \
		"$(cat "$out/etc/font-preview/cloudflare-v4.txt" "$out/etc/font-preview/cloudflare-v6.txt" | tr '\n' ' ' | sed 's/ $//')" \
		"$client_auth" >"$out/etc/font-preview/site.env"
	printf 'FPS_IMAGE=%s\nCADDY_IMAGE=%s\n' "$image" "$CADDY_IMAGE" >"$out/etc/font-preview/release.env"
	bash "$HERE/files/etc/font-preview/cloudflare.sh" rules "$out/etc/font-preview/cloudflare-v4.txt" \
		>"$out/var/lib/iptables/rules-save"
	# Ports are published on IPv4 only; should Docker start managing IPv6, its forwarded
	# traffic from outside is dropped too.
	{
		echo '*filter'
		echo ':INPUT DROP [0:0]'
		echo ':FORWARD DROP [0:0]'
		echo ':OUTPUT ACCEPT [0:0]'
		echo ':DOCKER-USER - [0:0]'
		echo '-A INPUT -i lo -j ACCEPT'
		echo '-A INPUT -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT'
		echo '-A INPUT -p ipv6-icmp -j ACCEPT'
		echo '-A INPUT -p tcp --dport 22 -j ACCEPT'
		echo '-A DOCKER-USER -m conntrack --ctstate RELATED,ESTABLISHED -j RETURN'
		echo '-A DOCKER-USER -i docker+ -j RETURN'
		echo '-A DOCKER-USER -i br-+ -j RETURN'
		echo '-A DOCKER-USER -j DROP'
		echo 'COMMIT'
	} >"$out/var/lib/ip6tables/rules-save"
	# LTS channel; reboot for updates only inside a quiet window.
	printf 'GROUP=lts\nREBOOT_STRATEGY=reboot\nLOCKSMITHD_REBOOT_WINDOW_START=04:00\nLOCKSMITHD_REBOOT_WINDOW_LENGTH=1h\n' \
		>"$out/etc/flatcar/update.conf"
}

# Health from inside the host: the proxy container asks the service directly, so a Cloudflare
# setting can't trigger a rollback.
healthy() {
	local dest=$1
	for _ in $(seq 1 30); do
		if remote "$dest" 'sudo docker exec fps-proxy wget -qO- -T 3 http://fps:8000/healthz' 2>/dev/null | grep -qx ok; then
			return 0
		fi
		sleep 2
	done
	return 1
}

restart_service() {
	remote "$1" 'sudo systemctl restart fps.service'
}

# Keeps the images release.env and release.env.prev name, and Caddy's; removes the rest.
prune_images() {
	# shellcheck disable=SC2016 # expands on the host
	remote "$1" 'cd /etc/font-preview &&
		keep=$(sed -n "s/^[A-Z_]*IMAGE=.*@//p" release.env release.env.prev 2>/dev/null | sort -u) &&
		sudo docker image ls --digests --format "{{.ID}} {{.Digest}}" | while read -r id digest; do
			echo "$keep" | grep -qx "$digest" || sudo docker rmi "$id" >/dev/null 2>&1 || true
		done'
}

cmd_setup() {
	local dest=$1 domain=$2 image=$3
	need_digest "$image"
	local tree
	tree=$(mktemp -d)
	trap 'rm -rf "$tree"' RETURN
	render "$tree" "$domain" "$image"
	remote "$dest" 'sudo test -f /etc/font-preview/release.env && sudo cp /etc/font-preview/release.env /etc/font-preview/release.env.prev || true'
	tar -C "$tree" --owner=0 --group=0 -czf - . | remote "$dest" 'sudo tar -xzf - -C / --no-same-permissions --no-overwrite-dir'
	remote "$dest" 'sudo install -d -o 1000 -g 1000 -m 0750 /var/lib/font-preview/caddy/data /var/lib/font-preview/caddy/config'
	# If the new firewall cuts SSH, this puts INPUT back to ACCEPT after 3 minutes.
	remote "$dest" 'sudo systemd-run --quiet --unit fps-firewall-guard --on-active=180 /bin/sh -c "iptables -P INPUT ACCEPT; ip6tables -P INPUT ACCEPT"'
	# Restoring flushes Docker's chains; restarting Docker recreates them.
	if ! remote "$dest" 'sudo systemctl daemon-reload &&
		sudo systemctl enable iptables-restore.service ip6tables-restore.service &&
		sudo systemctl restart iptables-restore.service ip6tables-restore.service &&
		sudo systemctl restart docker.service'; then
		if remote "$dest" 'sudo systemctl stop fps-firewall-guard.timer'; then
			die "applying the firewall failed; the guard is disarmed, check iptables on $dest"
		fi
		die "applying the firewall failed and SSH is unreachable; the guard sets INPUT to ACCEPT within 3 minutes"
	fi
	remote "$dest" true || die "SSH failed after the firewall change; the guard sets INPUT to ACCEPT within 3 minutes"
	remote "$dest" 'sudo systemctl stop fps-firewall-guard.timer'
	remote "$dest" 'sudo systemctl restart systemd-journald.service locksmithd.service &&
		sudo systemctl enable fps.service fps-proxy.service fps-cloudflare.timer &&
		sudo systemctl start fps-cloudflare.timer &&
		sudo systemctl restart fps.service fps-proxy.service'
	healthy "$dest" || die "service not healthy after setup; see: deploy.sh status $dest"
	prune_images "$dest"
	echo "setup done: $image"
}

cmd_deploy() {
	local dest=$1 image=$2
	need_digest "$image"
	local q
	q=$(printf '%q' "$image")
	# Pull before touching anything: a bad digest or a registry outage stops here, with the
	# running release untouched.
	remote "$dest" "sudo docker pull $q >/dev/null" || die "pull failed for $image; nothing changed"
	remote "$dest" "sudo cp /etc/font-preview/release.env /etc/font-preview/release.env.prev &&
		sudo sed -i 's|^FPS_IMAGE=.*|FPS_IMAGE='$q'|' /etc/font-preview/release.env"
	if restart_service "$dest" && healthy "$dest"; then
		prune_images "$dest"
		echo "deployed: $image"
	else
		echo "deploy.sh: $image is not healthy; rolling back" >&2
		cmd_rollback "$dest"
		exit 1
	fi
}

cmd_rollback() {
	local dest=$1
	remote "$dest" 'test -f /etc/font-preview/release.env.prev' || die "no previous release on $dest"
	remote "$dest" 'cd /etc/font-preview && sudo cp release.env release.env.next &&
		sudo mv release.env.prev release.env && sudo mv release.env.next release.env.prev'
	if ! restart_service "$dest" || ! healthy "$dest"; then
		die "previous release is not healthy either"
	fi
	echo "rolled back to: $(remote "$dest" 'grep ^FPS_IMAGE= /etc/font-preview/release.env')"
}

cmd_status() {
	local dest=$1
	remote "$dest" 'systemctl is-active fps.service fps-proxy.service fps-cloudflare.timer;
		grep ^FPS_IMAGE= /etc/font-preview/release.env;
		sudo docker ps --format "{{.Names}} {{.Status}}";
		sudo journalctl -u fps.service -u fps-proxy.service -n 15 --no-pager -o cat'
	local domain
	domain=$(remote "$dest" 'sed -n "s/^FPS_DOMAIN=//p" /etc/font-preview/site.env')
	echo "public /healthz: $(curl -s -m 15 -o /dev/null -w '%{http_code}' "https://$domain/healthz")"
}

cmd_digest() {
	local ref=$1 digest
	# The first Digest line is the index (multi-arch) digest.
	digest=$(docker buildx imagetools inspect "$ref" | sed -n 's/^Digest: *\(sha256:[0-9a-f]*\)$/\1/p' | head -1)
	[[ -n $digest ]] || die "no digest for $ref"
	echo "${ref%%:*}@$digest"
}

cmd_ignition() {
	local domain=$1 image=$2 out=$3
	need_digest "$image"
	local tree
	tree=$(mktemp -d)
	trap 'rm -rf "$tree"' RETURN
	render "$tree" "$domain" "$image"
	docker run --rm -i -v "$tree:/files:ro" "$BUTANE_IMAGE" --strict --files-dir /files \
		<"$HERE/font-preview.bu" >"$out"
	echo "wrote $out"
}

case ${1:-} in
setup) [[ $# -eq 4 ]] || die "usage: setup <ssh-dest> <domain> <image@sha256:…>"; cmd_setup "$2" "$3" "$4" ;;
deploy) [[ $# -eq 3 ]] || die "usage: deploy <ssh-dest> <image@sha256:…>"; cmd_deploy "$2" "$3" ;;
rollback) [[ $# -eq 2 ]] || die "usage: rollback <ssh-dest>"; cmd_rollback "$2" ;;
status) [[ $# -eq 2 ]] || die "usage: status <ssh-dest>"; cmd_status "$2" ;;
digest) [[ $# -eq 2 ]] || die "usage: digest <image:tag>"; cmd_digest "$2" ;;
ignition) [[ $# -eq 4 ]] || die "usage: ignition <domain> <image@sha256:…> <out.ign>"; cmd_ignition "$2" "$3" "$4" ;;
*) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
