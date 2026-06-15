# Track B — local Linux environment

Track B (XDP attach, `io_uring` forwarder, BPF runtime) needs **Linux**. macOS cannot load XDP or compile the BPF object.

## Recommended on macOS: Docker (works when Multipass does not)

If Multipass has no internet, mount failures, or snap errors — **use Docker instead**:

```bash
cd /path/to/demiurge
./scripts/linux-vm/docker-track-b.sh bootstrap   # first time (~5–10 min)
./scripts/linux-vm/docker-track-b.sh shell     # Linux shell, repo at /work
./scripts/linux-vm/docker-track-b.sh gate        # re-run CI gate
./scripts/linux-vm/docker-track-b.sh down        # remove container
```

Docker uses your Mac’s network stack (reliable). The repo is bind-mounted at `/work` — edits on the Mac appear instantly in the container.

**Limits:** XDP *attach* to a real NIC still needs a real Linux VM or bare metal; Docker is enough for BPF compile, `io_uring` dev, and `gate.sh`.

## Quick pick

| Host | When | Command |
|------|------|---------|
| **macOS (any)** | Fast CI mirror, BPF compile | `./docker-track-b.sh bootstrap` |
| **macOS + VirtualBox** | **Headless VM, full Linux (XDP veth)** | `./vagrant-up.sh` |
| Apple Silicon Mac | Manual ISO install (GUI) | `./create-vbox.sh --start` |
| Multipass | Often broken NAT on Mac | skip unless networking works |

**Architecture note:** CI release artifacts are **linux x86_64**. An ARM64 Ubuntu VM (default on M-series Macs) is enough for `build-bpf.sh`, `gate.sh`, and `io_uring` development. Use an x86_64 Linux box later for production NIC / perf gates.

## Vagrant + VirtualBox (headless, recommended VM flow)

One command brings up Ubuntu, syncs the repo, and runs bootstrap (first run downloads ~1 GB box + ~10 min provision).

```bash
brew install vagrant          # VirtualBox must already be installed
cd /path/to/demiurge/scripts/linux-vm
chmod +x vagrant-up.sh vagrant-provision.sh
./vagrant-up.sh               # vagrant up --provider=virtualbox (headless)
./vagrant-up.sh ssh           # shell in; repo at /demiurge
./vagrant-up.sh provision     # re-run bootstrap after pull
./vagrant-up.sh destroy       # tear down
```

Uses `bento/ubuntu-24.04` (VirtualBox **arm64** on Apple Silicon, **amd64** on Intel). Repo sync is **rsync** to `/demiurge` on guest local disk (not vboxsf). Builds go to `/demiurge/target/` (host `target/` is excluded from rsync).

If the guest has no internet (macOS firewall / NAT), on the **host**:

```bash
sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add /usr/libexec/bootpd
sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblock /usr/libexec/bootpd
vagrant reload --provision
```

## VirtualBox (GUI install)

1. Install VirtualBox (one-time, needs password + system extension approval):

   ```bash
   brew install --cask virtualbox
   ```

   Open **System Settings → Privacy & Security** and allow Oracle VirtualBox; reboot if macOS asks.

2. Create the VM (downloads Ubuntu 24.04 Server ISO ~2.5 GB on first run):

   ```bash
   cd /path/to/demiurge
   chmod +x scripts/linux-vm/*.sh
   ./scripts/linux-vm/create-vbox.sh --start
   ```

3. In the installer: pick defaults, create a user, enable **OpenSSH server** if offered.

4. In the guest:

   ```bash
   git clone https://github.com/fxdv/demiurge.git ~/demiurge
   ~/demiurge/scripts/linux-vm/bootstrap-guest.sh
   ```

   Or mount the host repo shared folder (`/media/sf_demiurge`) after Guest Additions.

## Multipass (headless, no GUI)

```bash
brew install multipass
./scripts/linux-vm/create-multipass.sh
multipass shell demiurge-track-b
./scripts/linux-vm/bootstrap-guest.sh /mnt/demiurge   # after mount, or clone inside
```

### Mount failed (`multipass-sshfs` / snap store)

Host mount needs the `multipass-sshfs` snap **inside** the guest. If auto-install fails:

```bash
# Option A — install snap in guest, then mount (live edit of host files)
./scripts/linux-vm/fix-multipass-mount.sh
multipass mount /path/to/demiurge demiurge-track-b:/mnt/demiurge

# Option B — clone in guest (simplest; GitHub main only)
multipass shell demiurge-track-b
git clone https://github.com/fxdv/demiurge.git ~/demiurge
~/demiurge/scripts/linux-vm/bootstrap-guest.sh ~/demiurge

# Option C — copy local tree including uncommitted work (no mount)
./scripts/linux-vm/push-repo.sh
multipass shell demiurge-track-b
~/demiurge/scripts/linux-vm/bootstrap-guest.sh ~/demiurge
```

### No internet in the guest

The VM may have an IP (`192.168.252.x`) but still fail DNS or outbound NAT — common on macOS with VPN/firewall.

**Quick test inside `multipass shell`:**

```bash
ping -c2 1.1.1.1          # routing OK?
ping -c2 github.com       # DNS OK?
```

**Fix on the Mac host:**

```bash
./scripts/linux-vm/fix-multipass-network.sh
```

That whitelists `bootpd` in the macOS firewall, restarts the VM, and sets public DNS (`1.1.1.1`) in the guest.

**If still broken — bridged mode** (uses your Wi‑Fi directly; bypasses NAT):

```bash
multipass delete demiurge-track-b --purge
multipass set local.bridged-network=en0   # or en1 if on Wi‑Fi only
./scripts/linux-vm/create-multipass.sh
```

**Fully offline** (no guest internet; uses Docker on the Mac to build debs):

```bash
./scripts/linux-vm/prepare-offline-bundle.sh
./scripts/linux-vm/push-repo.sh
multipass shell demiurge-track-b
~/demiurge/scripts/linux-vm/bootstrap-offline.sh ~/demiurge
```

## What bootstrap runs

- apt: `clang`, `llvm`, `libbpf-dev`, `iputils-ping`, kernel headers, build tools
- `./scripts/ensure-rust-toolchain.sh` — Rust **stable ≥ 1.83** (Cargo.lock v4)
- `./scripts/build-bpf.sh` → `target/bpf/admit_shed.o`
- `./scripts/gate.sh` — full CI mirror **+ required Track B gate** (XDP veth smoke as root)

## Env overrides

```bash
VM_NAME=demiurge-track-b VM_CPUS=4 VM_MEM_MB=8192 DEMIURGE_REPO=/path/to/demiurge ./create-vbox.sh
```

### `Cargo.lock` version 4 / `-Znext-lockfile-bump`

The repo uses **lockfile v4** (Rust **1.83+**). Provisioning runs `./scripts/ensure-rust-toolchain.sh` automatically.

If `cargo` fails after an old VM snapshot:

```bash
source "$HOME/.cargo/env"
bash /demiurge/scripts/ensure-rust-toolchain.sh
```
