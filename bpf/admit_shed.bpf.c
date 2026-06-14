// SPDX-License-Identifier: Apache-2.0 OR MIT
//
// XDP token-bucket admission shed — kernel counterpart to userspace [`AdmitBucket`].
// Phase 5+ production path for [DEMI-XDP-SHED]. Build: ./scripts/build-bpf.sh

#include <linux/bpf.h>
#include <bpf/bpf_helpers.h>

enum admit_map_key {
	KEY_TOKENS = 0,
	KEY_CAPACITY = 1,
	KEY_SHED_TOTAL = 2,
};

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 3);
	__type(key, __u32);
	__type(value, __u64);
} admit_state SEC(".maps");

static __always_inline int admit_or_shed(void)
{
	__u32 tok_key = KEY_TOKENS;
	__u64 *tokens = bpf_map_lookup_elem(&admit_state, &tok_key);
	if (!tokens)
		return XDP_ABORTED;

	if (*tokens == 0) {
		__u32 shed_key = KEY_SHED_TOTAL;
		__u64 *shed = bpf_map_lookup_elem(&admit_state, &shed_key);
		if (shed)
			__sync_fetch_and_add(shed, 1);
		return XDP_DROP;
	}

	__sync_fetch_and_sub(tokens, 1);
	return XDP_PASS;
}

SEC("xdp")
int xdp_admit_shed(struct xdp_md *ctx)
{
	(void)ctx;
	return admit_or_shed();
}

char LICENSE[] SEC("license") = "Dual Apache-2.0 OR MIT";
