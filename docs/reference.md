# Reference

## Commands

```text
pdf-to-black-ink INPUT.pdf OUTPUT.pdf [--no-verify] [--profile ICC]
pdf-to-cmyk      INPUT.pdf OUTPUT.pdf [--keep-spots] [--jpeg] [--no-verify] [--profile ICC]
```

OUTPUT must be a different file from INPUT, in an existing folder.

## Options

- **`--profile ICC`**: the CMYK profile of the printing condition, instead of the default FOGRA39 profile.
- **`--no-verify`**: skip the render comparison (the audit still runs).
- **`--keep-spots`** (`pdf-to-cmyk`): keep spot colours instead of converting them to process colours.
- **`--jpeg`** (`pdf-to-cmyk`): write colour photos as maximum-quality JPEG. Without it, images keep their resolution losslessly, so photos that were JPEG get larger.

## The ICC profile

Without `--profile`, the commands read:

1. the file named by `PDF_PRINT_COLORS_PROFILE`, when set;
2. otherwise `FOGRA39L_coated.icc` in `$XDG_DATA_HOME/color/icc` (by default `~/.local/share/color/icc`).

The profile ships with TeX Live's `colorprofiles` package. To use it:

```bash
mkdir -p ~/.local/share/color/icc
cp "$(kpsewhich FOGRA39L_coated.icc)" ~/.local/share/color/icc/
```

A profile named `FOGRA39L_coated.icc` is recorded in the output intent as "Coated FOGRA39 (ISO 12647-2:2004)"; any other profile is recorded under its file name.

## What each command produces

`pdf-to-black-ink`:

- every colour, image, gradient and spot colour becomes a grey, written as CMYK with only K;
- greys are tints of K, and every black (grey 0, RGB 0 0 0, K 100, rich black) is K 100;
- printer's marks in the registration colour become K only.

`pdf-to-cmyk`:

- RGB, ICC, Lab and spot colours, images and gradients are converted through the profile, relative colorimetric with black point compensation;
- colours already in CMYK stay as they are;
- pure black and neutral greys from RGB or grey go on the black plate only; images keep their rich blacks;
- registration marks stay on all four process plates.

Both add the output intent when the PDF has none, and keep page boxes (trim, bleed), fonts, bookmarks and links. The result is not declared PDF/X.

## Checks

OUTPUT is written only when every check passes:

1. **Audit:** the colour spaces of the result are listed. Any RGB, Lab or unknown space fails, and so does a spot colour left without `--keep-spots`. For `pdf-to-black-ink`, any colour or image with cyan, magenta or yellow fails too.
2. **Ink check** (`pdf-to-black-ink`): Ghostscript `inkcov` must find no cyan, magenta or yellow ink on any page.
3. **Render comparison:** input and output are rendered at 72 dpi. A page fails when more than 0.05 % of its pixels differ by more than 48 grey levels from every pixel around them, which catches lost images, text or marks.

## Exit codes

- `0`: OUTPUT written.
- `1`: an error (unreadable input, missing profile, Ghostscript failure).
- `2`: the result was refused by a check; the reason is printed and nothing is written.
- `64`: wrong usage.
