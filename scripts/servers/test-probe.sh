#!/usr/bin/env bash
# Runs remote-probe.sh on this machine against fake tools to cover branches the live servers
# don't hit: Docker without permission / daemon down / not installed / running, and hosts
# without ip, lsblk, systemctl or resolvectl. Needs bash and jq; changes nothing on the host.
#   scripts/servers/test-probe.sh
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
probe="$here/remote-probe.sh"
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
pass=0 fail=0

check() { # name, json, jq expression that must be true
  if jq -e "$3" >/dev/null 2>&1 <<<"$2"; then pass=$((pass + 1)); echo "ok   $1"
  else fail=$((fail + 1)); echo "FAIL $1: $3"; jq -c '.docker // .' <<<"$2" 2>/dev/null | head -c 400; echo; fi
}

fake_docker() { # dir, body of the `docker info` branch
  mkdir -p "$1"
  cat >"$1/docker" <<DOCKER
#!/usr/bin/env bash
case "\$1" in
  --version) echo "Docker version 24.0.7, build afdd53b" ;;
  info) $2 ;;
  stats) printf '%s\n' 'abcdef123456|12.5%|100MiB / 1GiB|9.77%' ;;
  ps) printf '%s\n' 'abcdef123456|web.1|nginx:1.25|running|Up 2 hours (healthy)|2 hours ago|0.0.0.0:80->80/tcp' ;;
  *) exit 1 ;;
esac
DOCKER
  chmod +x "$1/docker"
}

run() { PROBE_EXTRA_PATH="${1:-}" PROBE_DISABLE="${2:-}" bash "$probe" 2>/dev/null; }

fake_docker "$tmp/noperm" 'echo "permission denied while trying to connect to the Docker daemon socket at unix:///var/run/docker.sock" >&2; exit 1'
out=$(run "$tmp/noperm")
check "valid json (no_permission)" "$out" 'type == "object"'
check "docker no_permission" "$out" '.docker.status == "no_permission" and .docker.installed == true and .docker.client_version == "24.0.7"'
check "no_permission marks docker unavailable" "$out" '.unavailable | index("docker") != null'

fake_docker "$tmp/down" 'echo "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?" >&2; exit 1'
out=$(run "$tmp/down")
check "docker daemon_down" "$out" '.docker.status == "daemon_down" and (.docker.error | test("Cannot connect"))'
check "daemon_down is not a collection error" "$out" '.unavailable | index("docker") == null'

fake_docker "$tmp/up" "echo '24.0.7|3|1|0|2|5|overlay2|inactive|false|4|8000000000'"
out=$(run "$tmp/up")
check "docker running" "$out" '.docker.status == "running" and .docker.containers_running == 1 and .docker.images == 5'
check "container row and stats" "$out" '.docker.containers[0] | .name == "web.1" and .cpu_percent == 12.5 and .mem_percent == 9.77'

out=$(run "" "docker")
check "docker not_installed" "$out" '.docker.status == "not_installed" and (.unavailable | index("docker") == null)'

out=$(run "" "ip lsblk systemctl resolvectl")
check "fallbacks: valid json" "$out" 'type == "object"'
check "fallbacks: interfaces from /sys/class/net" "$out" '(.network.interfaces | length) > 0 and any(.network.interfaces[]; .name == "lo")'
check "fallbacks: gateway from /proc/net/route" "$out" \
  "$( [[ -n $(awk '$2=="00000000"' /proc/net/route) ]] && echo '.network.default_gateway != null' || echo 'true')"
check "fallbacks: primary ip" "$out" '.system.primary_ip != null'
check "fallbacks: block devices from /sys/block" "$out" '(.block_devices | length) > 0'
check "fallbacks: DNS from resolv.conf" "$out" '.network.dns_servers | type == "array"'
check "fallbacks: services reported unavailable" "$out" '.services == null and (.unavailable | index("services") != null)'

echo "$pass passed, $fail failed"
(( fail == 0 ))
