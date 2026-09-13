#!/bin/bash
# QEMU guest for AF_XDP zero-copy benchmarking. No libvirt, no virt-install,
# no cloud-localds: qemu-system-x86_64 + a hand-built NoCloud seed.
#
#     ./scripts/vm/vm.sh image      # download + verify base qcow2, make ssh key
#     ./scripts/vm/vm.sh up         # boot, pin vCPUs, block until ssh answers
#     ./scripts/vm/vm.sh ssh [cmd]
#     ./scripts/vm/vm.sh status
#     ./scripts/vm/vm.sh down
#
#     host virbr0 10.99.1.1/24   <->  guest eth1 10.99.1.2/24   (device under test)
#     host localhost:2222        <->  guest eth0 (user-mode NAT), ssh only
#
# virbr0 is an isolated libvirt network this script defines and starts over
# qemu:///system, so no root is needed for it. The multiqueue tap plugged into
# it is the one thing root has to create, once per host boot; `up` prints the
# command when it is missing.
set -u

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
CACHE=${VM_CACHE:-$HOME/.cache/rustssi-vm}
REL=44
BUILD=1.7
IMG=Fedora-Cloud-Base-Generic-$REL-$BUILD.x86_64.qcow2
CHECKSUM=Fedora-Cloud-$REL-$BUILD-x86_64-CHECKSUM
MIRROR=https://download.fedoraproject.org/pub/fedora/linux/releases/$REL/Cloud/x86_64/images

TAP=sp-tap0
HOST_IP=10.99.1.1
GUEST_IP=10.99.1.2
GUEST_MAC=52:54:00:99:01:02
BRIDGE=virbr0
LIBVIRT_NET=rustssi
HELPER=/usr/libexec/qemu-bridge-helper
VIRSH="-c qemu:///system"
QUEUES=${VM_QUEUES:-2}
HOST_CPUS=${VM_HOST_CPUS:-8-11}
MEM=4096
SSH_PORT=${VM_SSH_PORT:-2222}
# Guest cores 2,3 are isolated for the pinned busy-poll benchmark thread.
ISOL="isolcpus=2,3 nohz_full=2,3 rcu_nocbs=2,3 net.ifnames=0"

BASE=$CACHE/$IMG
KEY=$CACHE/id_ed25519
OVERLAY=$CACHE/overlay.qcow2
SEED=$CACHE/seed.img
PIDFILE=$CACHE/qemu.pid
QMP=$CACHE/qmp.sock
SERIAL=$CACHE/serial.log
SSH_OPTS=(-i "$KEY" -p "$SSH_PORT" -o StrictHostKeyChecking=no \
	-o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=4)

die() { echo "vm: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null; }
qemu_pid() { [[ -s $PIDFILE ]] && read -r p <"$PIDFILE" && kill -0 "$p" 2>/dev/null && echo "$p"; }

cpu_list() {
	local part
	for part in ${HOST_CPUS//,/ }; do
		if [[ $part == *-* ]]; then seq "${part%-*}" "${part#*-}"; else echo "$part"; fi
	done
}

cmd_image() {
	mkdir -p "$CACHE" || die "cannot create $CACHE"
	[[ -f $KEY ]] || ssh-keygen -q -t ed25519 -N '' -C rustssi-vm -f "$KEY" || die "ssh-keygen failed"
	if [[ ! -f $BASE ]]; then
		echo "vm: downloading $IMG"
		curl -fL# -C - -o "$BASE.part" "$MIRROR/$IMG" || die "download failed"
		mv "$BASE.part" "$BASE"
	fi
	curl -fsSL -o "$CACHE/$CHECKSUM" "$MIRROR/$CHECKSUM" || die "checksum download failed"
	local want
	want=$(sed -n "s/^SHA256 ($IMG) = //p" "$CACHE/$CHECKSUM")
	[[ -n $want ]] || die "no SHA256 line for $IMG in $CHECKSUM"
	echo "vm: verifying $IMG"
	echo "$want  $BASE" | sha256sum -c --quiet - || die "checksum mismatch; rm $BASE and retry"
	qemu-img info "$BASE" | sed -n '1,4p'
	echo "vm: image ready: $BASE"
}

# NoCloud seed. cloud-localds is not installed, so build the FAT/ISO by hand;
# either works, cloud-init only needs the CIDATA volume label.
make_seed() {
	local d
	d=$(mktemp -d) || die "mktemp failed"
	printf 'instance-id: rustssi-1\nlocal-hostname: rustssi-vm\n' >"$d/meta-data"
	cat >"$d/user-data" <<EOF
#cloud-config
users:
  - name: fedora
    groups: [wheel]
    sudo: ["ALL=(ALL) NOPASSWD:ALL"]
    shell: /bin/bash
    ssh_authorized_keys: ["$(cat "$KEY.pub")"]
write_files:
  - path: /etc/NetworkManager/system-connections/eth0.nmconnection
    permissions: "0600"
    content: |
      [connection]
      id=eth0
      type=ethernet
      interface-name=eth0
      [ipv4]
      method=auto
      [ipv6]
      method=disabled
  - path: /etc/NetworkManager/system-connections/eth1.nmconnection
    permissions: "0600"
    content: |
      [connection]
      id=eth1
      type=ethernet
      interface-name=eth1
      [ipv4]
      method=manual
      address1=$GUEST_IP/24
      [ipv6]
      method=disabled
  - path: /usr/local/bin/zc-check
    permissions: "0755"
    content: |
      #!/usr/bin/python3
      # Root only. Proves virtio-net AF_XDP zero-copy end to end: register a UMEM,
      # size the rings, then bind XDP_ZEROCOPY. EINVAL here means the driver
      # refused zero-copy (usually missing VIRTIO_F_ACCESS_PLATFORM).
      import ctypes, mmap, os, socket, struct, sys
      SOL_XDP, AF_XDP, XDP_ZEROCOPY = 283, 44, 4
      XDP_RX_RING, XDP_UMEM_REG, XDP_FILL_RING, XDP_COMP_RING = 2, 4, 5, 6
      ifname = sys.argv[1] if len(sys.argv) > 1 else "eth1"
      queue = int(sys.argv[2]) if len(sys.argv) > 2 else 0
      frames, frame_size = 4096, 2048
      mem = mmap.mmap(-1, frames * frame_size)
      addr = ctypes.addressof(ctypes.c_char.from_buffer(mem))
      s = socket.socket(AF_XDP, socket.SOCK_RAW, 0)
      s.setsockopt(SOL_XDP, XDP_UMEM_REG,
                   struct.pack("QQIIII", addr, frames * frame_size, frame_size, 0, 0, 0))
      for opt in (XDP_FILL_RING, XDP_COMP_RING, XDP_RX_RING):
          s.setsockopt(SOL_XDP, opt, struct.pack("I", 2048))
      sa = struct.pack("HHIII", AF_XDP, XDP_ZEROCOPY, socket.if_nametoindex(ifname), queue, 0)
      libc = ctypes.CDLL("libc.so.6", use_errno=True)
      if libc.bind(s.fileno(), sa, len(sa)):
          err = ctypes.get_errno()
          sys.exit("%s queue %d: XDP_ZEROCOPY bind FAILED: %s" % (ifname, queue, os.strerror(err)))
      print("%s queue %d: XDP_ZEROCOPY bind ok" % (ifname, queue))
# ethtool for the channel count the suite must set, perf for profile mode.
# Both come from the NAT'd eth0 on first boot, before any benchmark runs.
packages: [ethtool, perf]
runcmd:
  # Quoted: an unquoted YAML flow item would split the arg string on its commas.
  - ["grubby", "--update-kernel=ALL", "--args=$ISOL"]
  - [reboot]
EOF
	rm -f "$SEED"
	if have genisoimage; then
		genisoimage -quiet -output "$SEED" -volid CIDATA -joliet -rock "$d/user-data" "$d/meta-data"
	elif have xorriso; then
		xorriso -as mkisofs -quiet -o "$SEED" -V CIDATA -J -r "$d/user-data" "$d/meta-data"
	elif have mkisofs; then
		mkisofs -quiet -o "$SEED" -V CIDATA -J -r "$d/user-data" "$d/meta-data"
	elif have mkfs.vfat && have mcopy; then
		truncate -s 1M "$SEED" && mkfs.vfat -n CIDATA "$SEED" >/dev/null &&
			mcopy -oi "$SEED" "$d/user-data" "$d/meta-data" ::
	else
		die "no seed image builder: install one of mtools, genisoimage, xorriso"
	fi || die "seed image build failed"
	rm -rf "$d"
}

# The addressed bridge comes from an isolated libvirt network: libvirtd runs as
# root and autostarts it, so membership in the libvirt group is enough. No
# <forward>, so it is plain L2 plus the host IP.
ensure_bridge() {
	[[ -d /sys/class/net/$BRIDGE ]] && return
	have virsh || die "no $BRIDGE and no virsh; install libvirt-client"
	cat >"$CACHE/net.xml" <<EOF
<network>
  <name>$LIBVIRT_NET</name>
  <bridge name='$BRIDGE' stp='off' delay='0'/>
  <ip address='$HOST_IP' netmask='255.255.255.0'/>
</network>
EOF
	virsh $VIRSH net-info "$LIBVIRT_NET" >/dev/null 2>&1 ||
		virsh $VIRSH net-define "$CACHE/net.xml" >/dev/null ||
		die "net-define failed; is your user in the libvirt group?"
	virsh $VIRSH net-start "$LIBVIRT_NET" >/dev/null || die "net-start $LIBVIRT_NET failed"
	virsh $VIRSH net-autostart "$LIBVIRT_NET" >/dev/null
	echo "vm: libvirt network $LIBVIRT_NET started, bridge $BRIDGE $HOST_IP/24"
}

# qemu rejects queues= together with helper=, and -netdev bridge has no queues
# parameter at all, so the setuid bridge helper can only ever give one queue.
# Multiqueue therefore needs a tap, and creating one needs root exactly once
# per host boot; after that qemu opens it as the owning user.
dut_netdev() {
	local master flags=0
	master=$(basename "$(readlink -f "/sys/class/net/$TAP/master" 2>/dev/null)")
	[[ -r /sys/class/net/$TAP/flags ]] && flags=$(<"/sys/class/net/$TAP/flags")
	if [[ $master == "$BRIDGE" && $((flags & 1)) -eq 1 ]]; then
		echo "tap,id=n1,ifname=$TAP,script=no,downscript=no,queues=$QUEUES,vhost=${VM_VHOST:-on}"
	elif [[ $QUEUES -eq 1 ]]; then
		echo "bridge,id=n1,br=$BRIDGE,helper=$HELPER"
	else
		die "no usable $TAP on $BRIDGE, and the bridge helper cannot do queues=$QUEUES.
  Run once per host boot:
    sudo ip tuntap add dev $TAP mode tap multi_queue user $(id -un)
    sudo ip link set $TAP master $BRIDGE up
  Or stay root-free with one queue: VM_QUEUES=1 $0 up"
	fi
}

# QMP query-cpus-fast: the only way to map vCPU index -> host thread id.
vcpu_tids() {
	python3 "$HERE/qmp.py" "$QMP"
}

pin_vcpus() {
	local tids cpus i=0 pair
	tids=$(vcpu_tids) || { echo "vm: QMP failed, vCPUs unpinned" >&2; return; }
	mapfile -t cpus < <(cpu_list)
	for pair in $tids; do
		if [[ $i -ge ${#cpus[@]} ]]; then
			echo "vm: only ${#cpus[@]} host cpus in \$VM_HOST_CPUS, vCPUs past $i unpinned" >&2
			break
		fi
		taskset -pc "${cpus[i]}" "${pair#*:}" >/dev/null ||
			echo "vm: pin vcpu ${pair%:*} -> cpu ${cpus[i]} failed" >&2
		i=$((i + 1))
	done
	echo "vm: pinned $i vCPUs to host cpus ${cpus[*]:0:$i}"
}

cmd_up() {
	[[ -z $(qemu_pid) ]] || die "already running (pid $(qemu_pid)); ./vm.sh down first"
	[[ -f $BASE && -f $KEY ]] || die "run ./vm.sh image first"
	[[ -e /dev/kvm ]] || die "/dev/kvm missing"
	make_seed
	rm -f "$OVERLAY" "$QMP" "$SERIAL"
	qemu-img create -q -f qcow2 -F qcow2 -b "$BASE" "$OVERLAY" >/dev/null ||
		die "overlay create failed"
	ensure_bridge
	local netdev
	netdev=$(dut_netdev) || exit 1

	local mem_args=(-m "$MEM")
	local free_kb pagesize_kb
	pagesize_kb=$(awk '/Hugepagesize/{print $2}' /proc/meminfo)
	free_kb=$(( $(awk '/HugePages_Free/{print $2}' /proc/meminfo) * pagesize_kb ))
	if [[ -d /dev/hugepages && $free_kb -ge $((MEM * 1024)) ]]; then
		mem_args+=(-mem-path /dev/hugepages -mem-prealloc)
		echo "vm: hugepage backing enabled"
	fi

	# nic1 iommu_platform=on (which needs disable-legacy=on) gives the guest
	# VIRTIO_F_ACCESS_PLATFORM; without it virtio_net cannot put the rings in
	# premapped DMA mode and an XDP_ZEROCOPY bind fails EINVAL.
	qemu-system-x86_64 \
		-enable-kvm -cpu host -smp 4 "${mem_args[@]}" \
		-display none -serial "file:$SERIAL" \
		-drive "file=$OVERLAY,if=virtio,format=qcow2" \
		-drive "file=$SEED,if=virtio,format=raw,readonly=on" \
		-netdev "user,id=n0,hostfwd=tcp::$SSH_PORT-:22" \
		-device virtio-net-pci,netdev=n0,id=nic0 \
		-netdev "$netdev" \
		-device "virtio-net-pci,netdev=n1,id=nic1,mac=$GUEST_MAC,mq=on,rss=on,vectors=$((2 * QUEUES + 2)),disable-legacy=on,iommu_platform=on" \
		-qmp "unix:$QMP,server=on,wait=off" \
		-pidfile "$PIDFILE" -daemonize || die "qemu failed to start, see $SERIAL"

	pin_vcpus
	echo -n "vm: waiting for ssh and the isolcpus reboot"
	local i
	for ((i = 0; i < 180; i++)); do
		# cloud-init reboots once to pick up isolcpus, so ssh answering is not
		# enough: wait for the cmdline the benchmark needs.
		if ssh "${SSH_OPTS[@]}" fedora@localhost 'grep -q isolcpus /proc/cmdline' 2>/dev/null; then
			echo " ok"
			break
		fi
		[[ -n $(qemu_pid) ]] || die "qemu died, see $SERIAL"
		echo -n .
		sleep 2
	done
	[[ $i -lt 180 ]] || die "guest never came up, see $SERIAL"

	ssh "${SSH_OPTS[@]}" fedora@localhost '
		echo "guest kernel: $(uname -r)"
		echo -n "BTF: "; [[ -r /sys/kernel/btf/vmlinux ]] && echo "/sys/kernel/btf/vmlinux present" || echo MISSING
		grep -h CONFIG_DEBUG_INFO_BTF= "/boot/config-$(uname -r)" 2>/dev/null || true
		ip -br addr
		ip -d link show eth1 | grep -o "num[tr]xqueues [0-9]*" | paste -sd" "
		sudo zc-check eth1 0' 2>&1

	cat <<EOF
vm: READY
  ssh:      ssh ${SSH_OPTS[*]} fedora@localhost
  host:     $BRIDGE $HOST_IP/24 via ${netdev%%,*} mac $(cat "/sys/class/net/$BRIDGE/address")
  guest:    eth1 $GUEST_IP/24 mac $GUEST_MAC, $QUEUES queues, guest cpus 2,3 isolated
EOF
}

cmd_ssh() {
	[[ -n $(qemu_pid) ]] || die "not running"
	exec ssh "${SSH_OPTS[@]}" fedora@localhost "$@"
}

cmd_status() {
	local p
	p=$(qemu_pid)
	if [[ -z $p ]]; then
		echo "qemu: not running"
		return
	fi
	echo "qemu: running, pid $p"
	local pair
	for pair in $(vcpu_tids 2>/dev/null); do
		echo "  vcpu ${pair%:*} tid ${pair#*:} $(taskset -pc "${pair#*:}" 2>/dev/null | sed 's/.*list: /affinity /')"
	done
	ip -br addr show "$BRIDGE" 2>/dev/null || echo "bridge: $BRIDGE missing"
	ip -br link show "$TAP" 2>/dev/null
	ssh "${SSH_OPTS[@]}" fedora@localhost 'ip -br addr' 2>/dev/null || echo "guest: unreachable"
}

cmd_down() {
	local p
	p=$(qemu_pid)
	if [[ -n $p ]]; then
		kill "$p" 2>/dev/null
		for _ in {1..20}; do kill -0 "$p" 2>/dev/null || break; sleep 0.5; done
		kill -9 "$p" 2>/dev/null
	fi
	# The libvirt network autostarts and the tap is reusable, so both stay;
	# deleting the tap would need root again.
	rm -f "$OVERLAY" "$PIDFILE" "$QMP" "$SEED"
	echo "vm: down"
}

case ${1:-} in
image) cmd_image ;;
up) cmd_up ;;
ssh) shift; cmd_ssh "$@" ;;
status) cmd_status ;;
down) cmd_down ;;
*) die "usage: $0 {image|up|ssh|status|down}" ;;
esac
