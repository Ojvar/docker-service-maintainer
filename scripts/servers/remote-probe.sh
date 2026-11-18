#!/usr/bin/env bash
# Read-only system probe for the Servers dashboard.
# collect-server.sh sends this file over ssh stdin and runs it with `bash -s` on the remote host.
# It only reads /proc, /sys, /etc and runs listing commands (df, ip, ps, systemctl, docker ps/info/stats);
# it never writes, never uses sudo and never prints command lines or environment variables.
# Output: one JSON object. A section that can't be read is null and its name is listed in "unavailable".
set +e
# PROBE_EXTRA_PATH / PROBE_DISABLE are test hooks (scripts/servers/test-probe.sh); they are never set over ssh.
export LC_ALL=C PATH="${PROBE_EXTRA_PATH:+$PROBE_EXTRA_PATH:}/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

unavailable=()
has() {
  [[ " ${PROBE_DISABLE:-} " == *" $1 "* ]] && return 1
  command -v "$1" >/dev/null 2>&1
}

# JSON string (exact escaping in bash), or null for an empty value.
js() {
  local s=$1
  s=${s//\\/\\\\}; s=${s//\"/\\\"}; s=${s//$'\t'/ }; s=${s//$'\r'/}; s=${s//$'\n'/\\n}
  printf '"%s"' "$s"
}
jsn() { if [[ -n $1 ]]; then js "$1"; else printf null; fi; }
num() { if [[ $1 =~ ^-?[0-9]+(\.[0-9]+)?$ ]]; then printf '%s' "$1"; else printf null; fi; }

# awk helper shared by all sections: e() makes a JSON string (backslashes and quotes are
# replaced, which is fine for display values), n() prints a number or null.
AWK_LIB='function e(s){gsub(/\\/,"/",s);gsub(/"/,"\047",s);gsub(/\t/," ",s);gsub(/\r/,"",s);return "\"" s "\""}
function n(v){return (v ~ /^-?[0-9]+(\.[0-9]+)?$/) ? v : "null"}
function r1(v){return sprintf("%.1f", v)}'

# Two snapshots of /proc counters, 1 second apart, for CPU usage, disk I/O and network rates.
# /proc/<pid>/stat is sampled too, so top processes show current CPU use (like top), not a lifetime average.
snap_t1=$(date +%s%N); cpu1=$(head -1 /proc/stat); disk1=$(cat /proc/diskstats 2>/dev/null); net1=$(cat /proc/net/dev 2>/dev/null)
procs1=$(cat /proc/[0-9]*/stat 2>/dev/null)
sleep 1
snap_t2=$(date +%s%N); cpu2=$(head -1 /proc/stat); disk2=$(cat /proc/diskstats 2>/dev/null); net2=$(cat /proc/net/dev 2>/dev/null)
procs2=$(cat /proc/[0-9]*/stat 2>/dev/null)
clk_tck=$(getconf CLK_TCK 2>/dev/null); [[ $clk_tck =~ ^[0-9]+$ ]] || clk_tck=100
elapsed_ms=$(( (snap_t2 - snap_t1) / 1000000 )); (( elapsed_ms > 0 )) || elapsed_ms=1000

sys_info() {
  local short fqdn os os_id os_ver ip virt
  short=$(cat /proc/sys/kernel/hostname 2>/dev/null || hostname)
  fqdn=$(hostname -f 2>/dev/null); [[ -n $fqdn ]] || fqdn=$short
  if [[ -r /etc/os-release ]]; then
    os=$(sed -n 's/^PRETTY_NAME=//p' /etc/os-release | tr -d '"')
    os_id=$(sed -n 's/^ID=//p' /etc/os-release | tr -d '"')
    os_ver=$(sed -n 's/^VERSION_ID=//p' /etc/os-release | tr -d '"')
  fi
  has ip && ip=$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{for(i=1;i<NF;i++) if($i=="src"){print $(i+1); exit}}')
  [[ -n $ip ]] || ip=$(hostname -I 2>/dev/null | awk '{print $1}')
  # Last resort: first non-loopback local IPv4 address from the kernel's FIB.
  [[ -n $ip ]] || ip=$(awk '/\|-- /{a=$2} /\/32 host LOCAL/{ if(a !~ /^127\./){print a; exit} }' /proc/net/fib_trie 2>/dev/null)
  has systemd-detect-virt && virt=$(systemd-detect-virt 2>/dev/null)
  printf '{"hostname":%s,"fqdn":%s,"primary_ip":%s,"os":%s,"os_id":%s,"os_version":%s,"kernel":%s,"architecture":%s,"uptime_seconds":%s,"boot_time":%s,"virtualization":%s,"timezone":%s,"remote_time":%s,"logged_in_users":%s}' \
    "$(jsn "$short")" "$(jsn "$fqdn")" "$(jsn "$ip")" "$(jsn "$os")" "$(jsn "$os_id")" "$(jsn "$os_ver")" \
    "$(jsn "$(uname -r)")" "$(jsn "$(uname -m)")" "$(num "$(awk '{print int($1)}' /proc/uptime)")" \
    "$(num "$(awk '/^btime/{print $2}' /proc/stat)")" "$(jsn "$virt")" "$(jsn "$(date +%Z)")" "$(num "$(date +%s)")" \
    "$(num "$(who 2>/dev/null | wc -l)")"
}

cpu_info() {
  local model logical sockets cores threads mhz usage
  model=$(awk -F: '/^model name/{sub(/^[ \t]+/,"",$2); print $2; exit}' /proc/cpuinfo)
  [[ -n $model ]] || model=$(lscpu 2>/dev/null | awk -F: '/^Model name/{sub(/^[ \t]+/,"",$2); print $2; exit}')
  logical=$(nproc 2>/dev/null || grep -c '^processor' /proc/cpuinfo)
  if has lscpu; then
    sockets=$(lscpu | awk -F: '/^Socket\(s\)/{gsub(/ /,"",$2); print $2; exit}')
    cores=$(lscpu | awk -F: '/^Core\(s\) per socket/{gsub(/ /,"",$2); print $2; exit}')
    threads=$(lscpu | awk -F: '/^Thread\(s\) per core/{gsub(/ /,"",$2); print $2; exit}')
  fi
  mhz=$(awk -F: '/^cpu MHz/{s+=$2; c++} END{if(c) printf "%.0f", s/c}' /proc/cpuinfo)
  # fields: cpu user nice system idle iowait irq softirq steal
  usage=$(printf '%s\n%s\n' "$cpu1" "$cpu2" | awk '
    {t=0; for(i=2;i<=9;i++) t+=$i; tot[NR]=t; idle[NR]=$5+$6; io[NR]=$6; st[NR]=$9; us[NR]=$2+$3; sy[NR]=$4+$7+$8}
    END{d=tot[2]-tot[1]; if(d<=0){print "null null null null null"; exit}
      printf "%.1f %.1f %.1f %.1f %.1f", 100*(d-(idle[2]-idle[1]))/d, 100*(io[2]-io[1])/d, 100*(st[2]-st[1])/d, 100*(us[2]-us[1])/d, 100*(sy[2]-sy[1])/d}')
  read -r u_all u_io u_st u_us u_sy <<<"$usage"
  local cores_total=""
  [[ $sockets =~ ^[0-9]+$ && $cores =~ ^[0-9]+$ ]] && cores_total=$((sockets * cores))
  printf '{"model":%s,"logical_cores":%s,"physical_cores":%s,"sockets":%s,"threads_per_core":%s,"mhz":%s,"usage_percent":%s,"user_percent":%s,"system_percent":%s,"iowait_percent":%s,"steal_percent":%s}' \
    "$(jsn "$model")" "$(num "$logical")" "$(num "$cores_total")" "$(num "$sockets")" "$(num "$threads")" "$(num "$mhz")" \
    "$(num "$u_all")" "$(num "$u_us")" "$(num "$u_sy")" "$(num "$u_io")" "$(num "$u_st")"
}

mem_info() {
  awk "$AWK_LIB"'
    {v[$1]=$2*1024}
    END{t=v["MemTotal:"]; a=v["MemAvailable:"]; if(a=="") a=v["MemFree:"]+v["Buffers:"]+v["Cached:"]
      if(!t) exit
      st=v["SwapTotal:"]; sf=v["SwapFree:"]
      printf "{\"total_bytes\":%.0f,\"available_bytes\":%.0f,\"used_bytes\":%.0f,\"free_bytes\":%.0f,\"buffers_bytes\":%.0f,\"cached_bytes\":%.0f,\"used_percent\":%s,\"swap_total_bytes\":%.0f,\"swap_used_bytes\":%.0f,\"swap_used_percent\":%s}",
        t, a, t-a, v["MemFree:"], v["Buffers:"], v["Cached:"], r1(100*(t-a)/t), st, st-sf, (st>0 ? r1(100*(st-sf)/st) : "0")}' /proc/meminfo
}

disks() {
  has df || return
  local out
  out=$(awk "$AWK_LIB"'
    FNR==NR { if(FNR>1){ip[$6]=$5; gsub(/%/,"",ip[$6])} next }
    FNR>1 { mp=$7; for(i=8;i<=NF;i++) mp=mp " " $i
      if(seen[$1 SUBSEP mp]++) next
      pct=($3>0) ? r1(100*$4/$3) : "0"
      net=($2 ~ /^(cifs|smb3|smbfs|nfs|nfs4|ceph|glusterfs|9p|davfs|afs|lustre|beegfs|gpfs)$/ || $2 ~ /^fuse\./) ? "true" : "false"
      printf "%s{\"filesystem\":%s,\"type\":%s,\"mount\":%s,\"total_bytes\":%s,\"used_bytes\":%s,\"available_bytes\":%s,\"used_percent\":%s,\"inodes_used_percent\":%s,\"network\":%s}", (c++?",":""), e($1), e($2), e(mp), n($3), n($4), n($5), pct, n(ip[mp]), net }' \
    <(timeout 5 df -P -i -x tmpfs -x devtmpfs -x squashfs -x overlay -x efivarfs -x nsfs -x fuse.lxcfs 2>/dev/null) \
    <(timeout 5 df -P -T -B1 -x tmpfs -x devtmpfs -x squashfs -x overlay -x efivarfs -x nsfs -x fuse.lxcfs 2>/dev/null))
  [[ -n $out ]] && printf '[%s]' "$out"
}

block_devices() {
  local out="" d name size rota
  if has lsblk; then
    out=$(lsblk -dnb -o NAME,SIZE,ROTA,TYPE 2>/dev/null | awk "$AWK_LIB"'$4=="disk"{printf "%s{\"name\":%s,\"size_bytes\":%s,\"rotational\":%s}", (c++?",":""), e($1), n($2), ($3=="1"?"true":"false")}')
  fi
  if [[ -z $out && -d /sys/block ]]; then
    # Fallback without lsblk: /sys/block/<dev>/size is in 512-byte sectors.
    for d in /sys/block/*; do
      name=${d##*/}
      [[ $name =~ ^(loop|ram|zram|sr|fd|dm-|md) ]] && continue
      size=$(cat "$d/size" 2>/dev/null); rota=$(cat "$d/queue/rotational" 2>/dev/null)
      [[ $size =~ ^[0-9]+$ ]] || continue
      out+="${out:+,}{\"name\":$(js "$name"),\"size_bytes\":$((size * 512)),\"rotational\":$([[ $rota == 1 ]] && echo true || echo false)}"
    done
  fi
  [[ -n $out ]] && printf '[%s]' "$out"
}

disk_io() {
  [[ -n $disk1 && -d /sys/block ]] || return
  local whole out
  whole=$(ls /sys/block 2>/dev/null | grep -Ev '^(loop|ram|zram|sr|fd)' | tr '\n' ' ')
  out=$( { echo "$disk1"; echo "@@"; echo "$disk2"; } | awk -v whole=" $whole" -v ms="$elapsed_ms" "$AWK_LIB"'
    $1=="@@" {second=1; next}
    { name=$3; if(index(whole, " " name " ")==0) next
      if(!second){ r1b[name]=$6*512; w1b[name]=$10*512; io1[name]=$13; order[++k]=name; next }
      rb=$6*512; wb=$10*512
      rd[name]=$4; wr[name]=$8; R[name]=rb; W[name]=wb
      rr[name]=(rb-r1b[name])*1000/ms; wr_s[name]=(wb-w1b[name])*1000/ms; ut[name]=($13-io1[name])*100/ms }
    END{ for(i=1;i<=k;i++){ d=order[i]; if(!(d in R)) continue; if(ut[d]>100) ut[d]=100
      printf "%s{\"device\":%s,\"reads\":%s,\"writes\":%s,\"read_bytes\":%.0f,\"written_bytes\":%.0f,\"read_bytes_per_sec\":%.0f,\"write_bytes_per_sec\":%.0f,\"util_percent\":%s}", (c++?",":""), e(d), n(rd[d]), n(wr[d]), R[d], W[d], rr[d], wr_s[d], r1(ut[d]) } }')
  [[ -n $out ]] && printf '[%s]' "$out"
}

# Same line format as `ip -o link`, built from /sys/class/net when `ip` is missing.
links_fallback() {
  local d n st mtu mac
  for d in /sys/class/net/*; do
    n=${d##*/}
    st=$(tr '[:lower:]' '[:upper:]' < "$d/operstate" 2>/dev/null); mtu=$(cat "$d/mtu" 2>/dev/null); mac=$(cat "$d/address" 2>/dev/null)
    printf '0: %s: <> mtu %s state %s link/ether %s\n' "$n" "${mtu:-0}" "${st:-UNKNOWN}" "${mac:-00:00:00:00:00:00}"
  done
}
# Same line format as `ip -o addr`, from ifconfig (both net-tools output styles) when `ip` is missing.
addrs_fallback() {
  has ifconfig || return
  ifconfig -a 2>/dev/null | awk '
    /^[^ \t]/ { nm=$1; sub(/:$/,"",nm) }
    $1=="inet" || $1=="inet6" { a=$2; sub(/^addr:/,"",a); if(a!="") print "0: " nm " " $1 " " a }'
}
network() {
  local ifaces gw gw_dev dns search upstream veth links addrs
  if has ip; then links=$(ip -o link show 2>/dev/null); addrs=$(ip -o addr show 2>/dev/null)
  else links=$(links_fallback); addrs=$(addrs_fallback)
  fi
  ifaces=$( { echo "$links"; echo "@@L"; echo "$addrs"; echo "@@A"; echo "$net1"; echo "@@D"; echo "$net2"; } | awk -v ms="$elapsed_ms" "$AWK_LIB"'
    $1=="@@L"{sec=1; next} $1=="@@A"{sec=2; next} $1=="@@D"{sec=3; next}
    sec==0 { nm=$2; sub(/:$/,"",nm); sub(/@.*/,"",nm); if(nm ~ /^veth/) next
      if(!(nm in seen)){seen[nm]=1; order[++k]=nm}
      for(i=3;i<NF;i++){ if($i=="state") st[nm]=$(i+1); if($i=="mtu") mtu[nm]=$(i+1); if($i ~ /^link\//) mac[nm]=$(i+1) }
      next }
    sec==1 { nm=$2; if(nm ~ /^veth/) next; if(!(nm in seen)){seen[nm]=1; order[++k]=nm}
      a=e($4); fam=($3=="inet6")?"ipv6":"ipv4"; addrs[nm]=addrs[nm] (addrs[nm]==""?"":",") "{\"family\":\"" fam "\",\"address\":" a "}"; next }
    { line=$0; if(index(line,":")==0) next; split(line, p, ":"); nm=p[1]; gsub(/ /,"",nm); if(nm ~ /^veth/) next
      rest=substr(line, index(line,":")+1); split(rest, f, " ")
      if(sec==2){ rx1[nm]=f[1]; tx1[nm]=f[9]; next }
      if(!(nm in seen)){seen[nm]=1; order[++k]=nm}
      rx[nm]=f[1]; rxp[nm]=f[2]; rxe[nm]=f[3]; rxd[nm]=f[4]; tx[nm]=f[9]; txp[nm]=f[10]; txe[nm]=f[11]; txd[nm]=f[12]
      rxr[nm]=(f[1]-rx1[nm])*1000/ms; txr[nm]=(f[9]-tx1[nm])*1000/ms }
    END{ for(i=1;i<=k;i++){ d=order[i]
      printf "%s{\"name\":%s,\"state\":%s,\"mtu\":%s,\"mac\":%s,\"addresses\":[%s],\"rx_bytes\":%s,\"tx_bytes\":%s,\"rx_packets\":%s,\"tx_packets\":%s,\"rx_errors\":%s,\"tx_errors\":%s,\"rx_dropped\":%s,\"tx_dropped\":%s,\"rx_bytes_per_sec\":%s,\"tx_bytes_per_sec\":%s}",
        (c++?",":""), e(d), (st[d]==""?"null":e(st[d])), n(mtu[d]), (mac[d]==""?"null":e(mac[d])), addrs[d], n(rx[d]), n(tx[d]), n(rxp[d]), n(txp[d]), n(rxe[d]), n(txe[d]), n(rxd[d]), n(txd[d]),
        (d in rx ? sprintf("%.0f", rxr[d]) : "null"), (d in tx ? sprintf("%.0f", txr[d]) : "null") } }')
  veth=$(awk -F: 'NR>2{gsub(/ /,"",$1); if($1 ~ /^veth/) c++} END{print c+0}' <<<"$net2")
  if has ip; then
    read -r gw gw_dev < <(ip route show default 2>/dev/null | awk '{g="";d=""; for(i=1;i<NF;i++){if($i=="via")g=$(i+1); if($i=="dev")d=$(i+1)} print g, d; exit}')
  elif [[ -r /proc/net/route ]]; then
    # Default route from /proc/net/route: gateway is little-endian hex.
    read -r gw gw_dev < <(awk 'function hx(s,  i,v){v=0; for(i=1;i<=length(s);i++) v=v*16+index("0123456789ABCDEF", toupper(substr(s,i,1)))-1; return v}
      NR>1 && $2=="00000000" { h=$3; printf "%d.%d.%d.%d %s\n", hx(substr(h,7,2)), hx(substr(h,5,2)), hx(substr(h,3,2)), hx(substr(h,1,2)), $1; exit }' /proc/net/route 2>/dev/null)
  fi
  if [[ -r /etc/resolv.conf ]]; then
    dns=$(awk '$1=="nameserver"{printf "%s\"%s\"", (c++?",":""), $2}' /etc/resolv.conf)
    search=$(awk '$1=="search"||$1=="domain"{for(i=2;i<=NF;i++) printf "%s\"%s\"", (c++?",":""), $i}' /etc/resolv.conf)
  fi
  if grep -q '^nameserver 127\.0\.0\.53' /etc/resolv.conf 2>/dev/null && has resolvectl; then
    upstream=$(timeout 3 resolvectl dns 2>/dev/null | awk -F: '{split($NF,a," "); for(i in a) if(a[i]!="" && !seen[a[i]]++) printf "%s\"%s\"", (c++?",":""), a[i]}')
  fi
  [[ -z $ifaces && -z $gw ]] && return
  printf '{"interfaces":[%s],"veth_count":%s,"default_gateway":%s,"default_interface":%s,"dns_servers":[%s],"dns_search":[%s],"dns_upstream":[%s]}' \
    "$ifaces" "$(num "$veth")" "$(jsn "$gw")" "$(jsn "$gw_dev")" "$dns" "$search" "$upstream"
}

# Top 10 processes by CPU over the 1 s sample (percent of one core, like top), as JSON objects.
top_cpu_sampled() {
  [[ -n $procs1 && -n $procs2 ]] || return
  local rows pids
  # /proc/<pid>/stat: pid (comm) state ...; utime/stime are fields 14/15 (12/13 after the last ")").
  rows=$( { echo "$procs1"; echo "@@"; echo "$procs2"; } | awk -v hz="$clk_tck" -v ms="$elapsed_ms" '
    $1=="@@" { second=1; next }
    { line=$0; i=index(line, "("); n=split(line, a, ")"); rest=a[n]
      name=substr(line, i+1, length(line)-length(rest)-i-1); split(rest, f, " "); t=f[12]+f[13]; pid=$1
      if(!second){ t1[pid]=t; next }
      if(pid in t1){ d=t-t1[pid]; if(d>0){ gsub(/\t/," ",name); printf "%s\t%.1f\t%s\n", pid, d*100000/(hz*ms), name } } }' | sort -t$'\t' -k2,2 -rn | head -10)
  [[ -n $rows ]] || return
  pids=$(cut -f1 <<<"$rows" | paste -sd, -)
  { ps -o pid=,user:20=,pmem=,rss=,etime= -p "$pids" 2>/dev/null; echo "@@"; echo "$rows"; } | awk -F'\t' "$AWK_LIB"'
    $0=="@@" { second=1; next }
    !second { split($0, p, " "); u[p[1]]=p[2]; m[p[1]]=p[3]; r[p[1]]=p[4]; el[p[1]]=p[5]; next }
    { pid=$1; printf "%s{\"pid\":%s,\"user\":%s,\"cpu_percent\":%s,\"mem_percent\":%s,\"rss_bytes\":%s,\"elapsed\":%s,\"name\":%s}", (c++?",":""), n(pid), (pid in u ? e(u[pid]) : "null"), n($2), n(m[pid]), (pid in r ? n(r[pid]*1024) : "null"), (pid in el ? e(el[pid]) : "null"), e($3) }'
}

health() {
  local la procs threads zombies top_cpu top_mem
  read -r l1 l5 l15 rt _ < /proc/loadavg
  procs=$(ls -d /proc/[0-9]* 2>/dev/null | wc -l)
  threads=${rt#*/}; running=${rt%/*}
  zombies=$(ps -eo stat= 2>/dev/null | grep -c '^Z')
  # comm (name only), never full command lines: those can carry secrets.
  local ps_awk="$AWK_LIB"'NR>1 && NR<=11 {printf "%s{\"pid\":%s,\"user\":%s,\"cpu_percent\":%s,\"mem_percent\":%s,\"rss_bytes\":%s,\"elapsed\":%s,\"name\":%s}", (c++?",":""), n($1), e($2), n($3), n($4), n($5*1024), e($6), e($7)}'
  local mode=sampled
  top_cpu=$(top_cpu_sampled)
  if [[ -z $top_cpu ]]; then
    mode=lifetime
    top_cpu=$(ps -eo pid,user:20,pcpu,pmem,rss,etime,comm --sort=-pcpu 2>/dev/null | awk "$ps_awk")
  fi
  top_mem=$(ps -eo pid,user:20,pcpu,pmem,rss,etime,comm --sort=-rss 2>/dev/null | awk "$ps_awk")
  printf '{"load_1":%s,"load_5":%s,"load_15":%s,"processes":%s,"running_processes":%s,"threads":%s,"zombie_processes":%s,"top_cpu_mode":"%s","top_cpu":[%s],"top_memory":[%s]}' \
    "$(num "$l1")" "$(num "$l5")" "$(num "$l15")" "$(num "$procs")" "$(num "$running")" "$(num "$threads")" "$(num "$zombies")" "$mode" "$top_cpu" "$top_mem"
}

temperatures() {
  local out="" z t label h f name
  for z in /sys/class/thermal/thermal_zone*; do
    [[ -r $z/temp ]] || continue
    t=$(cat "$z/temp" 2>/dev/null); label=$(cat "$z/type" 2>/dev/null)
    [[ $t =~ ^-?[0-9]+$ ]] && (( t > -50000 && t < 150000 )) || continue
    out+="${out:+,}{\"sensor\":$(jsn "${label:-${z##*/}}"),\"celsius\":$(awk -v t="$t" 'BEGIN{printf "%.1f", t/1000}'),\"source\":\"thermal\"}"
  done
  for h in /sys/class/hwmon/hwmon*; do
    name=$(cat "$h/name" 2>/dev/null)
    for f in "$h"/temp*_input; do
      [[ -r $f ]] || continue
      t=$(cat "$f" 2>/dev/null); label=$(cat "${f%_input}_label" 2>/dev/null)
      [[ $t =~ ^-?[0-9]+$ ]] && (( t > -50000 && t < 150000 )) || continue
      out+="${out:+,}{\"sensor\":$(jsn "${name:-hwmon}${label:+ $label}"),\"celsius\":$(awk -v t="$t" 'BEGIN{printf "%.1f", t/1000}'),\"source\":\"hwmon\"}"
    done
  done
  if [[ -z $out ]] && has sensors; then
    # lm-sensors fallback: chip lines, "Label:" lines, then "  tempN_input: 45.000".
    out=$(timeout 5 sensors -u 2>/dev/null | awk "$AWK_LIB"'
      /^[^ \t].*:$/ { label=substr($0, 1, length($0)-1); next }
      /^[^ \t]/ && $0 !~ /:/ { chip=$0; label=""; next }
      /^[ \t]+temp[0-9]+_input:/ { v=$2+0; if(v>-50 && v<150) printf "%s{\"sensor\":%s,\"celsius\":%.1f,\"source\":\"sensors\"}", (c++?",":""), e(chip (label!="" ? " " label : "")), v }')
  fi
  [[ -n $out ]] && printf '[%s]' "$out"
}

services() {
  has systemctl || return
  local running failed rc
  running=$(timeout 6 systemctl list-units --type=service --state=running --no-legend --no-pager --plain 2>/dev/null)
  rc=$?; (( rc == 0 )) || return
  failed=$(timeout 6 systemctl list-units --type=service --state=failed --no-legend --no-pager --plain 2>/dev/null)
  local svc_awk="$AWK_LIB"'NF>=4{d=""; for(i=5;i<=NF;i++) d=d (d==""?"":" ") $i; printf "%s{\"unit\":%s,\"active\":%s,\"sub\":%s,\"description\":%s}", (c++?",":""), e($1), e($3), e($4), e(d)}'
  local r f
  r=$(awk "$svc_awk" <<<"$running"); f=$(awk "$svc_awk" <<<"$failed")
  printf '{"running_count":%s,"failed_count":%s,"running":[%s],"failed":[%s]}' \
    "$(grep -c . <<<"$running")" "$(grep -c . <<<"$failed")" "$r" "$f"
}

docker_info() {
  if ! has docker; then printf '{"installed":false,"status":"not_installed"}'; return; fi
  local client info rc status err
  client=$(docker --version 2>/dev/null | sed -n 's/^Docker version \([^,]*\).*/\1/p')
  info=$(timeout 8 docker info --format '{{.ServerVersion}}|{{.Containers}}|{{.ContainersRunning}}|{{.ContainersPaused}}|{{.ContainersStopped}}|{{.Images}}|{{.Driver}}|{{.Swarm.LocalNodeState}}|{{.Swarm.ControlAvailable}}|{{.NCPU}}|{{.MemTotal}}' 2>&1)
  rc=$?
  if (( rc != 0 )); then
    err=$(head -3 <<<"$info" | tr '\n' ' ')
    case $err in
      *"permission denied"*|*"Permission denied"*) status=no_permission ;;
      *"Cannot connect"*|*"Is the docker daemon running"*) status=daemon_down ;;
      *) status=error ;;
    esac
    [[ $rc == 124 ]] && { status=error; err="docker info timed out"; }
    if [[ $status == daemon_down ]]; then
      printf '{"installed":true,"status":"daemon_down","client_version":%s,"error":%s}' "$(jsn "$client")" "$(jsn "$err")"
    else
      printf '{"installed":true,"status":"%s","client_version":%s,"error":%s,"collect_error":true}' "$status" "$(jsn "$client")" "$(jsn "$err")"
    fi
    return
  fi
  IFS='|' read -r ver total running paused stopped images driver swarm manager _ _ <<<"$info"
  local containers stats
  stats=$(timeout 15 docker stats --no-stream --format '{{.ID}}|{{.CPUPerc}}|{{.MemUsage}}|{{.MemPerc}}' 2>/dev/null)
  containers=$(timeout 8 docker ps --no-trunc=false --format '{{.ID}}|{{.Names}}|{{.Image}}|{{.State}}|{{.Status}}|{{.RunningFor}}|{{.Ports}}' 2>/dev/null |
    awk -F'|' -v stats="$stats" "$AWK_LIB"'
      BEGIN{ m=split(stats, L, "\n"); for(i=1;i<=m;i++){ split(L[i], s, "|"); id=substr(s[1],1,12); gsub(/%/,"",s[2]); gsub(/%/,"",s[4]); cpu[id]=s[2]; mu[id]=s[3]; mp[id]=s[4] } }
      { id=substr($1,1,12); ports=$7; if(length(ports)>300) ports=substr(ports,1,300) "…"
        printf "%s{\"id\":%s,\"name\":%s,\"image\":%s,\"state\":%s,\"status\":%s,\"running_for\":%s,\"ports\":%s,\"cpu_percent\":%s,\"mem_usage\":%s,\"mem_percent\":%s}",
          (c++?",":""), e(id), e($2), e($3), e($4), e($5), e($6), e(ports), n(cpu[id]), (id in mu ? e(mu[id]) : "null"), n(mp[id]) }')
  printf '{"installed":true,"status":"running","client_version":%s,"server_version":%s,"storage_driver":%s,"containers_total":%s,"containers_running":%s,"containers_paused":%s,"containers_stopped":%s,"images":%s,"swarm_state":%s,"swarm_manager":%s,"stats_available":%s,"containers":[%s]}' \
    "$(jsn "$client")" "$(jsn "$ver")" "$(jsn "$driver")" "$(num "$total")" "$(num "$running")" "$(num "$paused")" "$(num "$stopped")" "$(num "$images")" \
    "$(jsn "$swarm")" "$([[ $manager == true ]] && echo true || echo false)" "$([[ -n $stats ]] && echo true || echo false)" "$containers"
}

# Prints `"name":<json>`; an empty or failed section becomes null and is listed in "unavailable".
section() {
  local out
  out=$("$2" 2>/dev/null)
  if [[ -z $out ]]; then unavailable+=("$1"); out=null
  elif [[ $out == *'"collect_error":true'* ]]; then unavailable+=("$1")
  fi
  printf '"%s":%s' "$1" "$out"
}

printf '{"probe_version":1,"sample_ms":%s,' "$elapsed_ms"
section system sys_info; printf ','
section cpu cpu_info; printf ','
section memory mem_info; printf ','
section disks disks; printf ','
section block_devices block_devices; printf ','
section disk_io disk_io; printf ','
section network network; printf ','
section health health; printf ','
section temperatures temperatures; printf ','
section services services; printf ','
section docker docker_info; printf ','
list=""; for u in "${unavailable[@]}"; do list+="${list:+,}\"$u\""; done
printf '"unavailable":[%s]}\n' "$list"
