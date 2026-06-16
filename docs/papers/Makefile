.PHONY: all clean

all: cost-algebra.pdf

cost-algebra.pdf: cost-algebra.tex
	latexmk -pdf -interaction=nonstopmode -halt-on-error cost-algebra.tex

clean:
	latexmk -C cost-algebra.tex 2>/dev/null || true
