#!/usr/bin/env bash
# One-time developer setup: toolchain components + a pre-push gate hook.
set -euo pipefail
cd "$(dirname "$0")/.."

if command -v rustup >/dev/null 2>&1 || [[ -f "$HOME/.cargo/env" ]]; then
  bash ./scripts/ensure-rust-toolchain.sh
fi

if [[ -d .git ]]; then
  hook=".git/hooks/pre-push"
  mkdir -p .git/hooks
  cat > "$hook" <<'EOF'
#!/usr/bin/env bash
exec ./scripts/gate.sh
EOF
  chmod +x "$hook"
  echo "installed pre-push hook -> scripts/gate.sh"
else
  echo "skip pre-push hook — no .git (Vagrant rsync / exported tree)"
fi
echo "run ./scripts/gate.sh anytime to gate locally"
