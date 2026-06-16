// SPDX-License-Identifier: Apache-2.0 OR MIT
//
// XDP token-bucket admission shed — kernel counterpart to userspace [`AdmitBucket`].
// Phase 5+ production path for [DEMI-XDP-SHED]. Build: ./scripts/build-bpf.sh

#include <linux/bpf.h>
#include <bpf/bpf_helpers.h>

struct demi_admit_state {
	__u64 tokens;
	__u64 capacity;
	__u64 shed_total;
};

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, __u32);
	__type(value, struct demi_admit_state);
} admit_state SEC(".maps");

static __always_inline int admit_or_shed(void)
{
	__u32 key = 0;
	struct demi_admit_state *st = bpf_map_lookup_elem(&admit_state, &key);
	if (!st)
		return XDP_ABORTED;

	/* Decrement-first (matches userspace AdmitBucket CAS): a plain
	 * load/compare then sub races on multi-CPU XDP — two CPUs can both
	 * observe the last token and over-admit, or sub from zero and wrap. */
	__u64 prev = __sync_fetch_and_sub(&st->tokens, 1);
	if (prev == 0) {
		__sync_fetch_and_add(&st->tokens, 1);
		__sync_fetch_and_add(&st->shed_total, 1);
		return XDP_DROP;
	}
	return XDP_PASS;
}

SEC("xdp")
int xdp_admit_shed(struct xdp_md *ctx)
{
	(void)ctx;
	return admit_or_shed();
}

char LICENSE[] SEC("license") = "Dual Apache-2.0 OR MIT";
