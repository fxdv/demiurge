#!/usr/bin/env bash
# One-time developer setup: toolchain components + a pre-push gate hook.
set -euo pipefail
cd "$(dirname "$0")/.."

if command -v rustup >/dev/null 2>&1; then
  rustup component add rustfmt clippy >/dev/null 2>&1 || true
fi

hook=".git/hooks/pre-push"
cat > "$hook" <<'EOF'
#!/usr/bin/env bash
exec ./scripts/gate.sh
EOF
chmod +x "$hook"

echo "installed pre-push hook -> scripts/gate.sh"
echo "run ./scripts/gate.sh anytime to gate locally"
