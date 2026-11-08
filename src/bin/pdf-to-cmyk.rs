//! pdf-to-cmyk: a PDF with its colours kept, converted to CMYK for printing (FOGRA39).
//!
//! RGB, ICC, Lab and spot colours, images and gradients are converted by Ghostscript to the
//! FOGRA39 CMYK profile (relative colorimetric with black point compensation); colours already in
//! CMYK stay as they are. Pure black and neutral greys in RGB or grey go on the black plate only
//! (K 100 and K tints), not as four-ink blacks. Images stay at full resolution, lossless (or as
//! maximum-quality JPEG with --jpeg). The FOGRA39 output intent is added; the result is audited
//! and its render compared with the input; when a check fails nothing is written (exit code 2).
//!
//! Usage: pdf-to-cmyk INPUT.pdf OUTPUT.pdf [--keep-spots] [--jpeg] [--no-verify] [--profile ICC]

fn main() {
    pdf_print_colors::pipeline::main(pdf_print_colors::pipeline::Mode::Cmyk);
}
