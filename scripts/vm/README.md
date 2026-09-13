# Bench guest (QEMU, AF_XDP zero-copy + RSS)

    ./scripts/vm/vm.sh image && ./scripts/vm/vm.sh up      # then ssh / status / down
    sudo ip tuntap add dev sp-tap0 mode tap multi_queue user $USER   # once per host boot,
    sudo ip link set sp-tap0 master virbr0 up                        # the only root step
`virbr0` is an isolated libvirt network `vm.sh` defines over `qemu:///system` itself (libvirt group, no root). `-netdev bridge`/`helper=` cannot do `queues=N`, so multiqueue needs that tap; `VM_QUEUES=1 ./scripts/vm/vm.sh up` is fully root-free at one queue.
Kernel: Fedora 44 Cloud, 6.19.10-300.fc44. virtio_net AF_XDP zero-copy is RX since 6.11 and TX since 6.13, so 6.13 is the floor. `iommu_platform=on` (VIRTIO_F_ACCESS_PLATFORM) is required - without premapped DMA the zero-copy bind fails EINVAL.
Zero-copy proof: `./scripts/vm/vm.sh ssh sudo zc-check eth1 0` -> `eth1 queue 0: XDP_ZEROCOPY bind ok` (registers a UMEM and binds `XDP_ZEROCOPY`; the kernel exposes xdp-features only over netdev netlink, which `ip`/`ethtool` do not read).
RSS worked with `vhost=on` (the default) and with `vhost=off`; only `vhost=off` makes qemu warn that it cannot load its eBPF steering program unless run as root. Verified in the guest with `ethtool -l eth1` (Combined: 2), `ethtool -x eth1` (2-ring indirection table), and both `virtio1-input.0/.1` in `/proc/interrupts` climbing under a multi-flow UDP blast. `ethtool` is not in the cloud image: `sudo dnf install ethtool`.
BTF for the bench binary's own BPF load: `/sys/kernel/btf/vmlinux` present, `CONFIG_DEBUG_INFO_BTF=y`. Guest CPUs 2,3 are `isolcpus`/`nohz_full`/`rcu_nocbs`, vCPU threads pinned to host CPUs `$VM_HOST_CPUS` (default 8-11), 4G RAM, hugepage-backed when the host pool has 4G free.
