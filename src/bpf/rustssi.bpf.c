// SPDX-License-Identifier: GPL-2.0
// Phase 1 data-plane redirector.
//
// Redirects a packet into the AF_XDP socket bound at its RX queue index when
// either endpoint (src or dst) is an (IPv4, port) tuple present in config_map;
// everything else falls through to the kernel stack (XDP_PASS).

#include <linux/bpf.h>
#include <linux/if_ether.h>
#include <linux/ip.h>
#include <linux/in.h>
#include <linux/tcp.h>
#include <linux/udp.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_endian.h>

char LICENSE[] SEC("license") = "GPL";

// Endpoint key. All fields in network byte order, matching how the bytes sit in
// the packet, so userspace inserts the raw octets/port with no host conversion.
// Packed so the 6-byte key has no padding bytes to zero for consistent hashing.
struct endpoint {
	__u32 addr; // __be32
	__u16 port; // __be16
} __attribute__((packed));

struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__type(key, struct endpoint);
	__type(value, __u8);
	__uint(max_entries, 16);
} config_map SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_XSKMAP);
	__type(key, __u32);
	__type(value, __u32);
	__uint(max_entries, 64);
} xsks_map SEC(".maps");

static __always_inline int endpoint_tracked(__be32 addr, __be16 port)
{
	struct endpoint key = {
		.addr = addr,
		.port = port,
	};
	return bpf_map_lookup_elem(&config_map, &key) != NULL;
}

SEC("xdp")
int xdp_redirect_irc(struct xdp_md *ctx)
{
	void *data = (void *)(long)ctx->data;
	void *data_end = (void *)(long)ctx->data_end;

	struct ethhdr *eth = data;
	if (data + sizeof(*eth) > data_end)
  {
		return XDP_PASS;
  }

	if (eth->h_proto != bpf_htons(ETH_P_IP))
  {
		return XDP_PASS;
  }

	struct iphdr *iph = data + sizeof(*eth);
	if ((void *)iph + sizeof(*iph) > data_end)
  {
		return XDP_PASS;
  }


	// Masked ihl keeps the L4 offset bounded so the verifier accepts the
	// variable-offset access below.
	__u32 ihl = (iph->ihl & 0x0f) * 4;
	if (ihl < sizeof(struct iphdr))
  {
		return XDP_PASS;
  }

	void *l4 = (void *)iph + ihl;
	__be16 sport, dport;

	if (iph->protocol == IPPROTO_TCP)
  {
		struct tcphdr *th = l4;
		if (l4 + sizeof(*th) > data_end)
    {
			return XDP_PASS;
    }

		sport = th->source;
		dport = th->dest;
	} else if (iph->protocol == IPPROTO_UDP) {
		struct udphdr *uh = l4;

		if (l4 + sizeof(*uh) > data_end)
    {
			return XDP_PASS;
    }

		sport = uh->source;
		dport = uh->dest;
	} else {
		return XDP_PASS;
	}

	if (endpoint_tracked(iph->saddr, sport) || endpoint_tracked(iph->daddr, dport))
  {
		return bpf_redirect_map(&xsks_map, ctx->rx_queue_index, XDP_PASS);
  }

	return XDP_PASS;
}
