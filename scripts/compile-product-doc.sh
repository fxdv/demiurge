#!/usr/bin/env bash
# Compile stamped PRODUCT-AND-DESIGN.md to PDF via pandoc + xelatex.
#
# Usage: ./scripts/compile-product-doc.sh input.md output.pdf
set -euo pipefail

MD="${1:?usage: compile-product-doc.sh <input.md> <output.pdf>}"
PDF="${2:?usage: compile-product-doc.sh <input.md> <output.pdf>}"

if ! command -v pandoc >/dev/null 2>&1; then
  echo "ERROR: pandoc not found — install pandoc + texlive-xetex" >&2
  echo "  macOS:  brew install pandoc basictex && sudo tlmgr update --self && sudo tlmgr install xetex" >&2
  echo "  Ubuntu: sudo apt-get install -y pandoc texlive-xetex" >&2
  exit 1
fi

ENGINE="xelatex"
if ! command -v xelatex >/dev/null 2>&1; then
  if command -v pdflatex >/dev/null 2>&1; then
    ENGINE="pdflatex"
  else
    echo "ERROR: no xelatex/pdflatex — install texlive-xetex or texlive-latex-base" >&2
    exit 1
  fi
fi

mkdir -p "$(dirname "$PDF")"
pandoc "$MD" -o "$PDF" \
  --pdf-engine="$ENGINE" \
  -V geometry:margin=1in \
  -V fontsize=11pt \
  -V documentclass=article \
  -V colorlinks=true \
  -V linkcolor=NavyBlue \
  -V urlcolor=NavyBlue \
  --toc \
  --toc-depth=2 \
  -V toc-title="Contents"

echo "compile-product-doc: wrote $PDF ($(wc -c < "$PDF" | tr -d ' ') bytes)"
