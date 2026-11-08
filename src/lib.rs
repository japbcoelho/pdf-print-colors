//! Print-ready colour for PDFs, shared by two commands:
//!
//! - `pdf-to-black-ink`: everything (colours, images, gradients, spot colours) to black and
//!   white, printed with black ink only: greys are tints of K and black is K 100.
//! - `pdf-to-cmyk`: colours kept, converted to CMYK for the FOGRA39 printing condition, with
//!   pure black and neutral greys on the black plate only.
//!
//! Ghostscript does the colour conversion; this crate prepares its input, finishes its output
//! (grey to K-only CMYK, output intent), audits the result and compares renders of the input and
//! the output before the file is written. See `SCRIPTS.md` for why Ghostscript and not the Craft
//! apps.

pub mod audit;
pub mod content;
pub mod gs;
pub mod images;
pub mod pipeline;
pub mod shading;
pub mod util;
