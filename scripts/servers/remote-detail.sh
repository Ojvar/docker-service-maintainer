#!/usr/bin/env bash
# Read-only detail lookup for the Servers page, run on the remote host by collect-server.sh:
#   bash -s -- container <name or id>   docker inspect summary (no environment variables, no logs)
#   bash -s -- unit <name>.service      systemctl show properties + last 50 journal lines (if readable)
# Prints one JSON object. Never uses sudo and never changes anything.
set +e
export LC_ALL=C PATH="${PROBE_EXTRA_PATH:+$PROBE_EXTRA_PATH:}/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

js() {
  local s=$1
  s=${s//\\/\\\\}; s=${s//\"/\\\"}; s=${s//$'\t'/ }; s=${s//$'\r'/}; s=${s//$'\n'/\\n}
  printf '"%s"' "$s"
}
fail() { printf '{"kind":%s,"name":%s,"error":%s}\n' "$(js "$kind")" "$(js "$name")" "$(js "$1")"; exit 0; }

kind=${1:-} name=${2:-}
case $kind in
  container)
    [[ $name =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,254}$ ]] || fail "invalid container name"
    command -v docker >/dev/null || fail "docker is not installed"
    # Selected fields only: .Config.Env and the full labels can hold secrets, so they are never read.
    out=$(timeout 10 docker inspect --type container --format '{"id":{{json .Id}},"name":{{json .Name}},"image":{{json .Config.Image}},"created":{{json .Created}},"state":{"status":{{json .State.Status}},"running":{{json .State.Running}},"restarting":{{json .State.Restarting}},"oom_killed":{{json .State.OOMKilled}},"exit_code":{{json .State.ExitCode}},"error":{{json .State.Error}},"started_at":{{json .State.StartedAt}},"finished_at":{{json .State.FinishedAt}},"health":{{if .State.Health}}{{json .State.Health.Status}}{{else}}null{{end}}},"restart_count":{{json .RestartCount}},"restart_policy":{{json .HostConfig.RestartPolicy}},"memory_limit":{{json .HostConfig.Memory}},"nano_cpus":{{json .HostConfig.NanoCpus}},"mounts":{{json .Mounts}},"networks":{{json .NetworkSettings.Networks}},"ports":{{json .NetworkSettings.Ports}},"swarm_service":{{json (index .Config.Labels "com.docker.swarm.service.name")}},"stack":{{json (index .Config.Labels "com.docker.stack.namespace")}}}' -- "$name" 2>&1)
    rc=$?
    (( rc == 0 )) || fail "$(head -2 <<<"$out" | tr '\n' ' ')"
    printf '{"kind":"container","name":%s,"container":%s}\n' "$(js "$name")" "$(head -1 <<<"$out")"
    ;;
  unit)
    [[ $name =~ ^[A-Za-z0-9@_.:-]{1,200}\.service$ ]] || fail "invalid unit name"
    command -v systemctl >/dev/null || fail "systemctl is not available"
    props=$(timeout 6 systemctl show "$name" --no-pager -p Id -p Description -p LoadState -p ActiveState -p SubState -p Result \
      -p ExecMainStatus -p ExecMainStartTimestamp -p ActiveEnterTimestamp -p InactiveEnterTimestamp -p NRestarts -p MainPID \
      -p FragmentPath -p MemoryCurrent -p UnitFileState 2>/dev/null)
    [[ -n $props ]] || fail "systemctl show returned nothing"
    pj=""
    while IFS='=' read -r k v; do
      [[ -n $k ]] || continue
      pj+="${pj:+,}$(js "$k"):$(js "$v")"
    done <<<"$props"
    logs="" logs_ok=false logs_note=""
    if command -v journalctl >/dev/null; then
      logs=$(timeout 8 journalctl -u "$name" -n 50 --no-pager -o short-iso 2>&1); rc=$?
      if (( rc == 0 )) && ! grep -qiE 'insufficient permissions|No journal files were (found|opened)' <<<"$logs"; then
        logs_ok=true
      else
        logs_note="journal not readable by this user (add it to the systemd-journal or adm group)"; logs=""
      fi
    else
      logs_note="journalctl is not available"
    fi
    printf '{"kind":"unit","name":%s,"unit":{%s},"logs_available":%s,"logs_note":%s,"logs":%s}\n' \
      "$(js "$name")" "$pj" "$logs_ok" "$(js "$logs_note")" "$(js "$(tail -c 20000 <<<"$logs")")"
    ;;
  *) fail "usage: <container|unit> <name>" ;;
esac
