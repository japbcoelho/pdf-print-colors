# How it works

Each run works in a private temporary folder, removed however the run ends:

1. **Prepare** the input for Ghostscript (see the pitfalls below).
2. **Convert** with Ghostscript's `pdfwrite`: to grey for `pdf-to-black-ink`, to CMYK through the ICC profile for `pdf-to-cmyk`.
3. **Finish** the result: greys move to the black plate as K tints, neutral RGB greys stay on K, transparency groups are set to CMYK, and the output intent is added.
4. **Audit** the colour spaces, **check the inks** and **compare renders** with the input ([checks](reference.md#checks)).
5. **Write** OUTPUT only when every check passed.

## Why Ghostscript

Ghostscript keeps text, fonts and the document structure. Prepress flatteners that were tried instead turned Type 1 text into outlines, dropped tiling patterns or replaced the fonts.

## Ghostscript pitfalls handled here

Found with Ghostscript 10.06:

- it drops 16-bit images that have a soft mask, so they are reduced to 8 bits first;
- with the Gray strategy it keeps spot colours, so spots go through CMYK first;
- it writes registration marks at tint 0, so they become process colours first;
- it leaves grey or RGB transparency groups, so they are set to CMYK after it.

## Speed

A 388-page LaTeX book took 24 s with every check; a 25-page Word document 2 to 4 s; a cover 1 to 2 s.
