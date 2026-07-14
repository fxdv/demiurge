// SPDX-License-Identifier: Apache-2.0 OR MIT
//
// XDP token-bucket admission shed — kernel counterpart to userspace [`AdmitBucket`].
// Phase 5+ production path for [DEMI-XDP-SHED]. Build: ./scripts/build-bpf.sh
//
// Scope: the bucket gates *new work only* — TCP SYN (and not SYN+ACK) packets,
// optionally narrowed to one listen port. Everything else (established flows,
// ACKs, ICMP, ARP, non-TCP) always passes: shedding must reject new requests,
// never sever in-flight connections or management traffic.
//
// Refill: classic token bucket. Tokens accrue lazily at `refill_per_sec`,
// applied on the SYN path before the decrement, so the bucket recovers without
// any userspace liveness dependency.
//
// Kernel floor: BPF_ATOMIC fetch/cmpxchg instructions (clang -mcpu=v3) require
// Linux >= 5.12.

#include <linux/bpf.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_endian.h>

// Self-contained header views (avoid uapi bitfield/byteorder portability):
// only the fields this program reads, all fixed-width.

#define DEMI_ETH_P_IP 0x0800
#define DEMI_ETH_P_IPV6 0x86DD
#define DEMI_IPPROTO_TCP 6

#define DEMI_TCP_FLAG_SYN 0x02
#define DEMI_TCP_FLAG_ACK 0x10

struct demi_eth {
	__u8 dst[6];
	__u8 src[6];
	__be16 proto;
};

struct demi_ip4 {
	__u8 ver_ihl; /* version:4 | ihl:4 (words) */
	__u8 tos;
	__be16 tot_len;
	__be16 id;
	__be16 frag_off;
	__u8 ttl;
	__u8 protocol;
	__be16 check;
	__be32 saddr;
	__be32 daddr;
};

struct demi_ip6 {
	__u8 ver_tc;
	__u8 tc_fl[3];
	__be16 payload_len;
	__u8 nexthdr;
	__u8 hop_limit;
	__u8 saddr[16];
	__u8 daddr[16];
};

struct demi_tcp {
	__be16 source;
	__be16 dest;
	__be32 seq;
	__be32 ack_seq;
	__u8 doff_res;
	__u8 flags;
	__be16 window;
};

struct demi_admit_state {
	/* Signed: an empty bucket dips below zero transiently under concurrent
	 * shed and every observer of prev <= 0 compensates. Unsigned would wrap
	 * to 2^64-1 here and fail the bucket *open* during overload. */
	__s64 tokens;
	__u64 capacity;
	/* Tokens accrued per second; 0 disables in-kernel refill. */
	__u64 refill_per_sec;
	__u64 last_refill_ns;
	/* Host byte order; 0 gates every TCP SYN on the interface. */
	__u64 listen_port;
};

struct demi_admit_stats {
	__u64 shed_total;
	__u64 pass_total;
};

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, __u32);
	__type(value, struct demi_admit_state);
} admit_state SEC(".maps");

/* Per-CPU: XDP disables preemption, so plain increments are race-free and
 * the hot path never bounces a shared stats cache line. Userspace sums. */
struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, __u32);
	__type(value, struct demi_admit_stats);
} admit_stats SEC(".maps");

/* 1 if this packet is a new-connection TCP SYN subject to the bucket.
 * Unparseable or exotic packets (IPv6 extension headers, fragments) pass:
 * the bucket is an overload valve, not a firewall. */
static __always_inline int demi_gated_syn(struct xdp_md *ctx, __u64 listen_port)
{
	void *data = (void *)(long)ctx->data;
	void *data_end = (void *)(long)ctx->data_end;

	struct demi_eth *eth = data;
	if ((void *)(eth + 1) > data_end)
		return 0;

	struct demi_tcp *tcp;
	if (eth->proto == bpf_htons(DEMI_ETH_P_IP)) {
		struct demi_ip4 *ip = (void *)(eth + 1);
		if ((void *)(ip + 1) > data_end)
			return 0;
		if (ip->protocol != DEMI_IPPROTO_TCP)
			return 0;
		__u32 ihl_bytes = (__u32)(ip->ver_ihl & 0x0f) * 4;
		if (ihl_bytes < sizeof(*ip))
			return 0;
		tcp = (void *)ip + ihl_bytes;
	} else if (eth->proto == bpf_htons(DEMI_ETH_P_IPV6)) {
		struct demi_ip6 *ip6 = (void *)(eth + 1);
		if ((void *)(ip6 + 1) > data_end)
			return 0;
		if (ip6->nexthdr != DEMI_IPPROTO_TCP)
			return 0;
		tcp = (void *)(ip6 + 1);
	} else {
		return 0;
	}

	if ((void *)(tcp + 1) > data_end)
		return 0;
	/* New connection attempt only: SYN set, ACK clear (SYN+ACK is the
	 * *reply* leg of a handshake someone else initiated). */
	if (!(tcp->flags & DEMI_TCP_FLAG_SYN) || (tcp->flags & DEMI_TCP_FLAG_ACK))
		return 0;
	if (listen_port && tcp->dest != bpf_htons((__u16)listen_port))
		return 0;
	return 1;
}

/* Lazy token accrual. One CPU wins the last_refill_ns CAS per quantum and
 * applies the whole-token credit; losers see no elapsed quantum and move on.
 * The capacity clamp reads tokens racily — worst case a concurrent admit
 * makes us under-fill by the raced amount, never over-fill past capacity
 * plus one in-flight quantum. */
static __always_inline void demi_maybe_refill(struct demi_admit_state *st)
{
	__u64 rate = st->refill_per_sec;
	if (!rate)
		return;
	__u64 ns_per_token = 1000000000ULL / rate;
	if (!ns_per_token)
		ns_per_token = 1;

	__u64 last = st->last_refill_ns;
	__u64 now = bpf_ktime_get_ns();
	if (now <= last)
		return;
	__u64 accrued = (now - last) / ns_per_token;
	if (!accrued)
		return;
	/* Advance by whole tokens only, keeping the fractional remainder. */
	__u64 new_last = last + accrued * ns_per_token;
	if (__sync_val_compare_and_swap(&st->last_refill_ns, last, new_last) != last)
		return;

	__s64 room = (__s64)st->capacity - st->tokens;
	if (room <= 0)
		return;
	__s64 add = (__s64)accrued;
	if (add > room)
		add = room;
	__sync_fetch_and_add(&st->tokens, add);
}

static __always_inline int admit_or_shed(struct xdp_md *ctx)
{
	__u32 key = 0;
	struct demi_admit_state *st = bpf_map_lookup_elem(&admit_state, &key);
	if (!st)
		return XDP_ABORTED;

	if (!demi_gated_syn(ctx, st->listen_port))
		return XDP_PASS;

	demi_maybe_refill(st);

	struct demi_admit_stats *stats = bpf_map_lookup_elem(&admit_stats, &key);

	/* Decrement-first (matches userspace AdmitBucket CAS): a plain
	 * load/compare then sub races on multi-CPU XDP — two CPUs can both
	 * observe the last token and over-admit. Signed compare handles the
	 * transient dip below zero; see demi_admit_state.tokens. */
	__s64 prev = __sync_fetch_and_sub(&st->tokens, 1);
	if (prev <= 0) {
		__sync_fetch_and_add(&st->tokens, 1);
		if (stats)
			stats->shed_total++;
		return XDP_DROP;
	}
	if (stats)
		stats->pass_total++;
	return XDP_PASS;
}

SEC("xdp")
int xdp_admit_shed(struct xdp_md *ctx)
{
	return admit_or_shed(ctx);
}

char LICENSE[] SEC("license") = "Dual Apache-2.0 OR MIT";
