# pdf-print-colors

Make a finished PDF print-ready: black ink only, or CMYK for FOGRA39, checked before it is written.

Print shops reject PDFs with RGB images, four-ink blacks in a black-and-white interior, or leftover spot colours. Two commands fix that for any PDF (LaTeX, Word or browser output, covers):

- **`pdf-to-black-ink`** turns every colour, image and gradient into a grey on the black plate only: a 30 % grey becomes K 30, and every black becomes K 100.
- **`pdf-to-cmyk`** keeps the colours and converts them to CMYK through the FOGRA39 profile; neutral greys and pure black stay on the black plate.

Both keep fonts, page boxes, bookmarks and links, add the FOGRA39 output intent, and write the output only after an audit of its colour spaces and a render comparison with the input pass. Ghostscript does the colour conversion.

## Install

Requires Rust 1.89 or later, Ghostscript (`gs`) and the `FOGRA39L_coated.icc` profile (see [docs/reference.md](docs/reference.md#the-icc-profile)).

```bash
cargo install --locked --git https://github.com/japbcoelho/pdf-print-colors
```

## Usage

```bash
pdf-to-black-ink book.pdf book-black.pdf
pdf-to-cmyk cover.pdf cover-cmyk.pdf
```

Options, the checks and the exit codes are in [docs/reference.md](docs/reference.md); how it works and the Ghostscript pitfalls it handles are in [docs/explanation.md](docs/explanation.md).

## Development

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

## License

[MIT](LICENSE). The FOGRA39 profile is not included.
