//! pdf-to-black-ink: a PDF in black and white, printed with black ink only.
//!
//! Every colour, image, gradient and spot colour becomes a grey, written as CMYK with only K:
//! greys are tints of K (a 30 % grey is K 30) and every black (grey 0, RGB 0 0 0, K 100, rich
//! black) is K 100. Ghostscript converts the colours; this tool then moves the greys to the black
//! plate, adds the FOGRA39 output intent, audits the result, checks with Ghostscript inkcov that no
//! page uses cyan, magenta or yellow, and compares renders of the input and the output. When a
//! check fails nothing is written and the exit code is 2.
//!
//! Usage: pdf-to-black-ink INPUT.pdf OUTPUT.pdf [--no-verify] [--profile ICC]

fn main() {
    pdf_print_colors::pipeline::main(pdf_print_colors::pipeline::Mode::BlackInk);
}
