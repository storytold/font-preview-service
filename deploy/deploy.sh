#!/usr/bin/env bash
# Deploys font-preview-service to a Flatcar host over SSH. Run from any Linux machine with ssh,
# tar and docker; nothing runs on the host except systemd, docker and iptables.
#
#   deploy.sh setup    <ssh-dest> <domain> <image>            first install, or re-apply files
#   deploy.sh deploy   <ssh-dest> <image>                     new service image, rolls back if unhealthy
#   deploy.sh rollback <ssh-dest>                             previous service image
#   deploy.sh status   <ssh-dest>
#   deploy.sh digest   <image:tag>                            prints image@sha256 for a tag
#   deploy.sh ignition <domain> <image@sha256:…> <out.ign>    Ignition config for a new host
#   deploy.sh load     <ssh-dest> <local-image-ref>           sideload a local image; prints its image ID
#   deploy.sh sync     <ssh-dest>                             run the fonts sync now and follow it
#   deploy.sh index    <ssh-dest> <commit> <sources-sha256>   pin the font index, then sync
#
# <image> is name@sha256:<digest> (pulled from the registry). A bare image ID sha256:<id> (already
# on the host, see load) is for test hosts only: set ALLOW_SIDELOAD=1 for setup, which marks the
# host with /etc/font-preview/test-host; deploy refuses IDs on a host without the marker.
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
ID_RE='^sha256:[0-9a-f]{64}$'
need_digest() { [[ $1 =~ $IMAGE_RE ]] || die "image must be name@sha256:<64 hex>, got $1"; }
is_id() { [[ $1 =~ $ID_RE ]]; }
need_image() {
	[[ $1 =~ $IMAGE_RE || $1 =~ $ID_RE ]] || die "image must be name@sha256:<64 hex> or sha256:<64 hex> (image ID), got $1"
	if is_id "$1" && [[ ${ALLOW_SIDELOAD:-} != 1 ]]; then
		die "a bare image ID is only for test hosts; set ALLOW_SIDELOAD=1 to deploy $1, or use name@sha256:<digest>"
	fi
	return 0
}
# A bare image ID can't be pulled: it has to be on the host already.
need_local_image() {
	remote "$1" "sudo docker image inspect $(printf '%q' "$2") >/dev/null 2>&1" \
		|| die "image $2 is not on $1; run: deploy.sh load $1 <local-image-ref>"
}
remote() { local dest=$1; shift; ssh -o BatchMode=yes "$dest" "$@"; }
require_test_host() {
	remote "$1" 'test -f /etc/font-preview/test-host' \
		|| die "$1 is not marked as a test host (/etc/font-preview/test-host); image IDs are refused. Re-run setup with ALLOW_SIDELOAD=1 if it is one, or use name@sha256:<digest>"
}

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
	# A new host comes up with the index pinned in versions.env; an existing pin on a host is kept.
	printf 'INDEX_COMMIT=%s\nINDEX_SHA256=%s\n' "$INDEX_COMMIT" "$INDEX_SHA256" >>"$out/etc/font-preview/sync.env"
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

# Readiness is reported, not waited on for long: with an empty fonts volume the service stays
# not ready until the first sync.
report_ready() {
	for _ in $(seq 1 5); do
		if remote "$1" 'sudo docker exec fps-proxy wget -qO- -T 3 http://fps:8000/readyz' 2>/dev/null | grep -q .; then
			echo "ready: a font generation is loaded"
			return 0
		fi
		sleep 2
	done
	echo "not ready: no font generation loaded yet (run: deploy.sh index/sync $1 …)"
}

restart_service() {
	remote "$1" 'sudo systemctl restart fps.service'
}

# Keeps the images release.env and release.env.prev name (by digest or by image ID), and
# Caddy's; removes the rest.
prune_images() {
	# shellcheck disable=SC2016 # expands on the host
	remote "$1" 'cd /etc/font-preview &&
		keep=$(sed -n "s/^[A-Z_]*IMAGE=//p" release.env release.env.prev 2>/dev/null | sed "s/^.*@//" | sort -u) &&
		sudo docker image ls --no-trunc --digests --format "{{.ID}} {{.Digest}}" | while read -r id digest; do
			echo "$keep" | grep -qxF -e "$digest" -e "$id" || sudo docker rmi "$id" >/dev/null 2>&1 || true
		done'
}

cmd_setup() {
	local dest=$1 domain=$2 image=$3
	need_image "$image"
	is_id "$image" && need_local_image "$dest" "$image"
	local tree
	tree=$(mktemp -d)
	trap 'rm -rf "$tree"' RETURN
	render "$tree" "$domain" "$image"
	remote "$dest" 'sudo test -f /etc/font-preview/release.env && sudo cp /etc/font-preview/release.env /etc/font-preview/release.env.prev || true'
	# sync.env holds the pinned index; a re-run must not reset it, so it is only created when missing.
	tar -C "$tree" --owner=0 --group=0 --exclude=./etc/font-preview/sync.env -czf - . \
		| remote "$dest" 'sudo tar -xzf - -C / --no-same-permissions --no-overwrite-dir'
	tar -C "$tree" --owner=0 --group=0 -czf - ./etc/font-preview/sync.env \
		| remote "$dest" 'sudo tar -xzf - -C / --no-same-permissions --skip-old-files'
	# A sync.env from an earlier setup may have no pin yet.
	remote "$dest" "sudo grep -q '^INDEX_COMMIT=' /etc/font-preview/sync.env ||
		printf 'INDEX_COMMIT=%s\nINDEX_SHA256=%s\n' $INDEX_COMMIT $INDEX_SHA256 | sudo tee -a /etc/font-preview/sync.env >/dev/null"
	if [[ ${ALLOW_SIDELOAD:-} == 1 ]]; then
		remote "$dest" 'sudo touch /etc/font-preview/test-host'
	fi
	remote "$dest" 'sudo install -d -o 1000 -g 1000 -m 0750 /var/lib/font-preview/caddy/data /var/lib/font-preview/caddy/config &&
		sudo install -d -o 65532 -g 65532 -m 0755 /var/lib/font-preview/fonts'
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
		sudo systemctl enable fps.service fps-proxy.service fps-cloudflare.timer fps-sync.timer &&
		sudo systemctl start fps-cloudflare.timer fps-sync.timer &&
		sudo systemctl restart fps.service fps-proxy.service'
	healthy "$dest" || die "service not healthy after setup; see: deploy.sh status $dest"
	prune_images "$dest"
	echo "setup done: $image"
	report_ready "$dest"
}

cmd_deploy() {
	local dest=$1 image=$2
	need_image "$image"
	local q
	q=$(printf '%q' "$image")
	# Pull (or check for a sideloaded image) before touching anything: a bad digest, a registry
	# outage or a missing image stops here, with the running release untouched.
	if is_id "$image"; then
		require_test_host "$dest"
		need_local_image "$dest" "$image"
	else
		remote "$dest" "sudo docker pull $q >/dev/null" || die "pull failed for $image; nothing changed"
	fi
	remote "$dest" "sudo cp /etc/font-preview/release.env /etc/font-preview/release.env.prev &&
		sudo sed -i 's|^FPS_IMAGE=.*|FPS_IMAGE='$q'|' /etc/font-preview/release.env"
	if restart_service "$dest" && healthy "$dest"; then
		prune_images "$dest"
		echo "deployed: $image"
		report_ready "$dest"
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
	# shellcheck disable=SC2016 # expands on the host
	remote "$dest" 'systemctl is-active fps.service fps-proxy.service fps-cloudflare.timer fps-sync.timer;
		grep ^FPS_IMAGE= /etc/font-preview/release.env;
		grep -qE "^FPS_IMAGE=sha256:[0-9a-f]{64}$" /etc/font-preview/release.env && echo "sideloaded image";
		echo "generation: $(readlink /var/lib/font-preview/fonts/current || echo none)";
		grep -E "^INDEX_(REPO|COMMIT)=" /etc/font-preview/sync.env;
		systemctl show fps-sync.service -p Result -p ExecMainStatus -p ExecMainExitTimestamp;
		systemctl list-timers fps-sync.timer --no-legend;
		sudo docker ps --format "{{.Names}} {{.Status}}";
		sudo journalctl -u fps.service -u fps-proxy.service -n 15 --no-pager -o cat'
	local domain
	domain=$(remote "$dest" 'sed -n "s/^FPS_DOMAIN=//p" /etc/font-preview/site.env')
	echo "public /healthz: $(curl -s -m 15 -o /dev/null -w '%{http_code}' "https://$domain/healthz")"
	report_ready "$dest"
}

# Streams a local image to the host (compressed) and prints the image ID that setup and deploy take.
cmd_load() {
	local dest=$1 ref=$2
	docker image inspect "$ref" >/dev/null 2>&1 || die "no local image $ref"
	docker save "$ref" | gzip -1 | remote "$dest" 'gunzip | sudo docker load >/dev/null' \
		|| die "loading $ref onto $dest failed"
	local id
	id=$(remote "$dest" "sudo docker image inspect --format '{{.Id}}' $(printf '%q' "$ref")") \
		|| die "$ref is not on $dest after the load"
	[[ $id =~ $ID_RE ]] || die "unexpected image ID from $dest: $id"
	echo "$id"
}

# The sync commands need a host that setup has installed the sync unit on.
require_sync_host() {
	remote "$1" 'sudo test -f /etc/font-preview/sync.env && systemctl cat fps-sync.service >/dev/null 2>&1' \
		|| die "$1 has no sync unit or sync.env; run setup first"
}

# Prints "Key=Value" lines of the sync unit; fails when SSH does.
sync_show() {
	remote "$1" 'systemctl show fps-sync.service -p ActiveState,Result,ExecMainStatus,InvocationID' </dev/null
}
prop() { sed -n "s/^$2=//p" <<<"$1"; }

# Starts the sync service as a fresh job and follows its journal until that invocation ends.
# Every SSH call is short, so a dropped connection is retried instead of reported as a failure.
cmd_sync() {
	local dest=$1
	require_sync_host "$dest"
	remote "$dest" 'sudo grep -q "^INDEX_COMMIT=" /etc/font-preview/sync.env' \
		|| die "no index pinned on $dest; run: deploy.sh index $dest <commit> <sources-sha256>"
	local st prev id="" cursor="" out c fails=0
	st=$(sync_show "$dest") || die "cannot reach $dest"
	# Starting a unit that is already activating joins that job and reports its (older) result.
	if [[ $(prop "$st" ActiveState) =~ ^(active|activating|deactivating)$ ]]; then
		echo "a sync is already running; waiting for it to finish, then starting a new one"
		while [[ $(prop "$st" ActiveState) =~ ^(active|activating|deactivating)$ ]]; do
			sleep 10
			st=$(sync_show "$dest") || true
		done
	fi
	prev=$(prop "$st" InvocationID)
	remote "$dest" 'sudo systemctl start --no-block fps-sync.service' </dev/null || die "could not start the sync on $dest"
	for _ in $(seq 1 30); do
		st=$(sync_show "$dest") || st=""
		id=$(prop "$st" InvocationID)
		[[ -n $id && $id != "$prev" ]] && break
		id=""
		sleep 2
	done
	[[ -n $id ]] || die "the sync did not start on $dest; see: journalctl -u fps-sync"
	echo "sync started (invocation $id)"
	flush_journal() {
		local args=""
		[[ -z $cursor ]] || args="--after-cursor='$cursor'"
		out=$(remote "$dest" "sudo journalctl -u fps-sync.service _SYSTEMD_INVOCATION_ID=$id --no-pager -o cat --show-cursor $args" </dev/null 2>/dev/null) || return 0
		c=$(printf '%s\n' "$out" | sed -n 's/^-- cursor: //p')
		[[ -z $c ]] || cursor=$c
		printf '%s\n' "$out" | grep -v '^-- ' || true
	}
	while :; do
		if st=$(sync_show "$dest"); then
			fails=0
			flush_journal
			[[ $(prop "$st" InvocationID) == "$id" ]] || break
			[[ $(prop "$st" ActiveState) =~ ^(inactive|failed)$ ]] && break
		else
			fails=$((fails + 1))
			((fails < 30)) || die "lost the connection to $dest; the sync may still be running, check: deploy.sh status $dest"
		fi
		sleep 10
	done
	flush_journal
	if [[ $(prop "$st" InvocationID) == "$id" && $(prop "$st" Result) == success ]]; then
		echo "sync finished: exit status $(prop "$st" ExecMainStatus)"
		remote "$dest" 'readlink /var/lib/font-preview/fonts/current | sed "s/^/generation: /"' || true
	else
		die "sync failed, current unchanged (see journal: journalctl -u fps-sync); exit status $(prop "$st" ExecMainStatus)"
	fi
}

# Pins the font index (public values) in sync.env, then syncs. A pin in versions.env is the
# default for new hosts; bump it there too.
cmd_index() {
	local dest=$1 commit=$2 sha=$3
	[[ $commit =~ ^[0-9a-f]{40}$ ]] || die "commit must be 40 hex digits, got $commit"
	[[ $sha =~ ^[0-9a-f]{64}$ ]] || die "sources-sha256 must be 64 hex digits, got $sha"
	require_sync_host "$dest"
	remote "$dest" "sudo sed -i '/^INDEX_COMMIT=/d;/^INDEX_SHA256=/d' /etc/font-preview/sync.env &&
		printf 'INDEX_COMMIT=%s\nINDEX_SHA256=%s\n' $commit $sha | sudo tee -a /etc/font-preview/sync.env >/dev/null"
	echo "pinned index $commit"
	cmd_sync "$dest"
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
setup) [[ $# -eq 4 ]] || die "usage: setup <ssh-dest> <domain> <image>"; cmd_setup "$2" "$3" "$4" ;;
deploy) [[ $# -eq 3 ]] || die "usage: deploy <ssh-dest> <image>"; cmd_deploy "$2" "$3" ;;
rollback) [[ $# -eq 2 ]] || die "usage: rollback <ssh-dest>"; cmd_rollback "$2" ;;
status) [[ $# -eq 2 ]] || die "usage: status <ssh-dest>"; cmd_status "$2" ;;
digest) [[ $# -eq 2 ]] || die "usage: digest <image:tag>"; cmd_digest "$2" ;;
load) [[ $# -eq 3 ]] || die "usage: load <ssh-dest> <local-image-ref>"; cmd_load "$2" "$3" ;;
sync) [[ $# -eq 2 ]] || die "usage: sync <ssh-dest>"; cmd_sync "$2" ;;
index) [[ $# -eq 4 ]] || die "usage: index <ssh-dest> <commit> <sources-sha256>"; cmd_index "$2" "$3" "$4" ;;
ignition) [[ $# -eq 4 ]] || die "usage: ignition <domain> <image@sha256:…> <out.ign>"; cmd_ignition "$2" "$3" "$4" ;;
*) sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
