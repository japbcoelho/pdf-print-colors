//! The command-line flow shared by `pdf-to-black-ink` and `pdf-to-cmyk`: prepare, convert with
//! Ghostscript, finish, audit, verify, then write the output (never over the input).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use lopdf::{Dictionary, Document, Object, Stream, StringFormat};

use crate::util::{self, Family, masks, zlib};
use crate::{audit, content, gs, images, shading};

/// File name of the FOGRA39 profile looked up by default.
pub const DEFAULT_PROFILE_NAME: &str = "FOGRA39L_coated.icc";

/// The profile used without `--profile`: `$PDF_PRINT_COLORS_PROFILE`, else
/// `FOGRA39L_coated.icc` in `$XDG_DATA_HOME/color/icc` (default `~/.local/share/color/icc`).
pub fn default_profile() -> PathBuf {
    if let Some(path) = std::env::var_os("PDF_PRINT_COLORS_PROFILE").filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_default();
    data_home.join("color/icc").join(DEFAULT_PROFILE_NAME)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    BlackInk,
    Cmyk,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::BlackInk => "pdf-to-black-ink",
            Mode::Cmyk => "pdf-to-cmyk",
        }
    }

    fn usage(self) -> String {
        match self {
            Mode::BlackInk => "usage: pdf-to-black-ink INPUT.pdf OUTPUT.pdf [--no-verify] [--profile ICC]".into(),
            Mode::Cmyk => "usage: pdf-to-cmyk INPUT.pdf OUTPUT.pdf [--keep-spots] [--jpeg] [--no-verify] [--profile ICC]".into(),
        }
    }
}

pub struct Options {
    pub input: PathBuf,
    pub output: PathBuf,
    pub verify: bool,
    pub profile: PathBuf,
    pub keep_spots: bool,
    pub jpeg: bool,
}

/// Verification thresholds: a page fails when more than `max_share` of its pixels differ by more
/// than `level` grey levels (of 255) between the input and the output rendered at 72 dpi.
struct Tolerance {
    level: u8,
    max_share: f64,
}

/// Parses the arguments and runs; exits 64 on bad usage, 2 when the result is refused.
pub fn main(mode: Mode) {
    let mut files = Vec::new();
    let mut o = Options {
        input: PathBuf::new(),
        output: PathBuf::new(),
        verify: true,
        profile: default_profile(),
        keep_spots: false,
        jpeg: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match (a.as_str(), mode) {
            ("-h" | "--help", _) => {
                println!("{}", mode.usage());
                return;
            }
            ("--no-verify", _) => o.verify = false,
            ("--profile", _) => match args.next() {
                Some(p) => o.profile = PathBuf::from(p),
                None => usage_error(mode),
            },
            ("--keep-spots", Mode::Cmyk) => o.keep_spots = true,
            ("--jpeg", Mode::Cmyk) => o.jpeg = true,
            (s, _) if s.starts_with("--") => usage_error(mode),
            _ => files.push(PathBuf::from(a)),
        }
    }
    if files.len() != 2 {
        usage_error(mode);
    }
    o.output = files.pop().unwrap();
    o.input = files.pop().unwrap();
    match run(mode, &o) {
        Ok(true) => {}
        Ok(false) => std::process::exit(2),
        Err(e) => {
            eprintln!("{}: error: {e:#}", mode.name());
            std::process::exit(1);
        }
    }
}

fn usage_error(mode: Mode) -> ! {
    eprintln!("{}", mode.usage());
    std::process::exit(64);
}

fn mb(p: &Path) -> String {
    std::fs::metadata(p)
        .map(|m| format!("{:.1} MB", m.len() as f64 / 1e6))
        .unwrap_or_default()
}

/// Returns Ok(false) when the output was refused (and not written).
pub fn run(mode: Mode, o: &Options) -> Result<bool> {
    let input = o
        .input
        .canonicalize()
        .with_context(|| format!("cannot read {}", o.input.display()))?;
    if o.output.canonicalize().ok().as_deref() == Some(input.as_path()) {
        bail!("OUTPUT must be a different file from INPUT");
    }
    let out_dir = o
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !out_dir.is_dir() {
        bail!("the folder of OUTPUT does not exist: {}", out_dir.display());
    }
    let profile = o
        .profile
        .canonicalize()
        .with_context(|| format!("cannot read the ICC profile {}", o.profile.display()))?;
    // Private work folder (random name, mode 0700, created atomically), removed however the run
    // ends.
    let tmp = tempfile::Builder::new()
        .prefix("pdf-print-colors-")
        .tempdir()
        .context("cannot create the work folder")?;
    let name = mode.name();

    // 1. Prepare: 16-bit images to 8 bits; registration marks as process colours; for CMYK,
    //    neutral RGB to grey.
    let mut doc =
        Document::load(&input).with_context(|| format!("cannot read {}", input.display()))?;
    let pages = doc.get_pages().len();
    println!(
        "{name}: {} ({pages} pages, {})",
        input.display(),
        mb(&input)
    );
    let sixteen = images::sixteen_to_eight(&mut doc)?;
    if sixteen > 0 {
        println!("  prepared: {sixteen} 16-bit images reduced to 8 bits");
    }
    let registration = if util::count_registration(&doc) > 0 {
        let pass = if mode == Mode::Cmyk {
            content::registration_to_all_inks
        } else {
            content::registration_to_black
        };
        content::rewrite_all(&mut doc, pass)?.registration_ops
    } else {
        0
    };
    if registration > 0 {
        let how = if mode == Mode::Cmyk {
            "C, M, Y and K 100 %"
        } else {
            "K only"
        };
        println!(
            "  prepared: {registration} registration-colour (printer's marks) colours set as {how}"
        );
    }
    let neutral = match mode {
        Mode::Cmyk => content::rewrite_all(&mut doc, content::neutral_rgb_to_gray)?.neutral_rgb_ops,
        Mode::BlackInk => 0,
    };
    if neutral > 0 {
        println!(
            "  prepared: {neutral} neutral RGB colours set as grey, so they print on the black plate only"
        );
    }
    let spots = mode == Mode::BlackInk && util::has_spots(&doc);
    let mut gs_input = if sixteen + neutral + registration > 0 {
        let p = tmp.path().join("prepared.pdf");
        doc.save(&p).context("cannot write the prepared copy")?;
        p
    } else {
        input.clone()
    };
    drop(doc);
    if spots {
        // Converting to grey, Ghostscript 10.06 keeps spot colours (Separation and DeviceN) even
        // with PreserveSeparation=false; converting to CMYK it does not. So spots go to process
        // colours first, with the same plain formulas as the grey pass.
        let p = tmp.path().join("process.pdf");
        let mut a: Vec<String> = gs::LOSSLESS_IMAGES.iter().map(|s| s.to_string()).collect();
        a.extend(
            [
                "-dPreserveSeparation=false",
                "-sColorConversionStrategy=CMYK",
                "-dProcessColorModel=/DeviceCMYK",
                "-dUseFastColor",
            ]
            .map(String::from),
        );
        let (took, _) = gs::pdfwrite(&gs_input, &p, &a, &[])?;
        println!(
            "  prepared: spot colours turned into process colours first ({:.1} s)",
            took.as_secs_f32()
        );
        gs_input = p;
    }

    // 2. Ghostscript.
    let icc = profile.display().to_string();
    let mut args: Vec<String> = gs::LOSSLESS_IMAGES.iter().map(|s| s.to_string()).collect();
    let mut post: Vec<String> = Vec::new();
    if !(mode == Mode::Cmyk && o.keep_spots) {
        args.push("-dPreserveSeparation=false".into());
    }
    match mode {
        Mode::BlackInk => args.extend(
            // UseFastColor: grey = 1 - min(1, 0.3c + 0.59m + 0.11y + k) and 0.3r + 0.59g + 0.11b,
            // so K-only colours keep their exact tint and every black becomes K 100.
            [
                "-sColorConversionStrategy=Gray",
                "-dProcessColorModel=/DeviceGray",
                "-dUseFastColor",
            ]
            .map(String::from),
        ),
        Mode::Cmyk => {
            args.extend(
                [
                    "-sColorConversionStrategy=CMYK",
                    "-dProcessColorModel=/DeviceCMYK",
                    "-dRenderIntent=1",
                    "-dBlackPtComp=1",
                ]
                .map(String::from),
            );
            args.push(format!("--permit-file-read={icc}"));
            args.push(format!("-sOutputICCProfile={icc}"));
            if o.jpeg {
                args.retain(|a| a != "-dColorImageFilter=/FlateEncode");
                args.push("-dColorImageFilter=/DCTEncode".into());
                post.extend(
                    ["-c", "<< /ColorImageDict << /QFactor 0.15 /Blend 1 /HSamples [1 1 1 1] /VSamples [1 1 1 1] >> >> setdistillerparams", "-f"]
                        .map(String::from),
                );
            }
        }
    }
    let gs_out = tmp.path().join("gs.pdf");
    let (took, msg) = gs::pdfwrite(&gs_input, &gs_out, &args, &post)?;
    println!("  Ghostscript: {:.1} s", took.as_secs_f32());
    for line in msg.lines().filter(|l| !l.trim().is_empty()).take(12) {
        println!("  Ghostscript says: {}", line.trim());
    }

    // 3. Finish: grey to K-only CMYK everywhere, output intent.
    let mut doc = Document::load(&gs_out).context("cannot read Ghostscript's output")?;
    let m = masks(&doc);
    let cs = content::rewrite_all(
        &mut doc,
        if mode == Mode::BlackInk {
            content::gray_and_registration_to_k
        } else {
            content::gray_to_k
        },
    )?;
    let is = images::gray_images_to_k(&mut doc, &m)?;
    let ss = shading::gray_shadings_to_k(&mut doc, &m)?;
    util::normalize_registration(&mut doc);
    let intent = add_output_intent(&mut doc, &profile)?;
    println!(
        "  black plate: {} grey colours, {} grey colour-space colours, {} images, {} indexed images, {} gradients, {} transparency groups -> K only",
        cs.gray_ops, cs.gray_space_ops, is.converted, is.indexed, ss.converted, ss.groups
    );
    if intent {
        println!(
            "  output intent: {}",
            profile.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    let mut notes = Vec::new();
    if is.left_gray > 0 {
        notes.push(format!(
            "{} greyscale images kept in DeviceGray (they print on the black plate only)",
            is.left_gray
        ));
    }
    if ss.left_gray > 0 {
        notes.push(format!(
            "{} gradients kept in DeviceGray (they print on the black plate only)",
            ss.left_gray
        ));
    }
    if cs.unreadable > 0 {
        notes.push(format!(
            "{} content streams could not be read and were left as they are",
            cs.unreadable
        ));
    }
    doc.compress();
    let file_name = o
        .output
        .file_name()
        .context("OUTPUT has no file name")?
        .to_string_lossy()
        .to_string();
    let partial = out_dir.join(format!(".{file_name}.partial"));
    doc.save(&partial)
        .with_context(|| format!("cannot write {}", partial.display()))?;
    drop(doc);

    // 4. Audit and 5. verify; the output is written only when both pass.
    let verdict = check(mode, o, &input, &partial, &icc, tmp.path(), &mut notes);
    for n in &notes {
        println!("  note: {n}");
    }
    match verdict {
        Ok(()) => {
            std::fs::rename(&partial, &o.output)
                .with_context(|| format!("cannot write {}", o.output.display()))?;
            println!("  written: {} ({})", o.output.display(), mb(&o.output));
            Ok(true)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            eprintln!("{name}: refused, nothing written: {e:#}");
            Ok(false)
        }
    }
}

fn check(
    mode: Mode,
    o: &Options,
    input: &Path,
    out: &Path,
    icc: &str,
    tmp: &Path,
    notes: &mut Vec<String>,
) -> Result<()> {
    let doc = Document::load(out).context("cannot read the result back")?;
    let c = audit::census(&doc, mode == Mode::BlackInk);
    let out_pages = doc.get_pages().len();
    drop(doc);
    println!("  audit: {}", c.summary());
    let mut bad: Vec<String> = Vec::new();
    for f in [Family::Rgb, Family::Lab, Family::Unknown] {
        if c.count(f) > 0 {
            bad.push(format!("{} {} colour uses left", c.count(f), f.label()));
        }
    }
    let registration = c.count(Family::Registration);
    if registration > 0 {
        if mode == Mode::Cmyk {
            notes.push(format!("{registration} uses of the registration colour kept (printer's marks, on every plate)"));
        } else {
            bad.push(format!(
                "{registration} uses of the registration colour left"
            ));
        }
    }
    let spots = c.count(Family::Separation) + c.count(Family::DeviceN);
    if spots > 0 {
        if mode == Mode::Cmyk && o.keep_spots {
            notes.push(format!("{spots} spot-colour uses kept (--keep-spots)"));
        } else {
            bad.push(format!(
                "{spots} spot-colour (Separation/DeviceN) uses left"
            ));
        }
    }
    if mode == Mode::BlackInk && c.cmy_ops + c.cmy_images > 0 {
        bad.push(format!(
            "{} colours and {} images still use cyan, magenta or yellow",
            c.cmy_ops, c.cmy_images
        ));
    }
    if !bad.is_empty() {
        bail!("the audit found {}", bad.join("; "));
    }
    if !o.verify {
        notes.push("not verified (--no-verify)".into());
        return Ok(());
    }
    if mode == Mode::BlackInk {
        let cov = gs::inkcov(out, &[])?;
        if cov.len() != out_pages {
            bail!("ink coverage covered {} of {out_pages} pages", cov.len());
        }
        if let Some((i, v)) = cov
            .iter()
            .enumerate()
            .find(|(_, v)| v[0] + v[1] + v[2] > 0.0)
        {
            bail!(
                "page {} uses cyan, magenta or yellow ink (C {:.5}, M {:.5}, Y {:.5})",
                i + 1,
                v[0],
                v[1],
                v[2]
            );
        }
        println!("  ink check: every page prints with black ink only (Ghostscript inkcov)");
    }
    // Faithful conversions measured at most 0.004 % (line and glyph edges); a lost 2 cm image 0.3 %.
    let tol = Tolerance {
        level: 48,
        max_share: 0.0005,
    };
    // Each view renders the input and the output alike; a pixel is explained when any view finds
    // it (or a neighbour) within `tol.level`. Missing or changed content fails every view.
    let views: Vec<(bool, Vec<String>)> = match mode {
        // The same plain formulas Ghostscript used, so a faithful result renders alike.
        Mode::BlackInk => vec![(false, vec!["-dUseFastColor".into()])],
        Mode::Cmyk => vec![
            // Lightness: holds when the input blends in RGB (its transparency groups) and the
            // output in CMYK.
            (
                false,
                vec![
                    format!("--permit-file-read={icc}"),
                    format!("-sDefaultCMYKProfile={icc}"),
                ],
            ),
            // The input rendered in CMYK through the same profile and intent as the conversion:
            // holds for saturated colours that lose lightness in the FOGRA39 gamut.
            (
                true,
                vec![
                    format!("--permit-file-read={icc}"),
                    format!("-sDefaultCMYKProfile={icc}"),
                    format!("-sOutputICCProfile={icc}"),
                    "-dRenderIntent=1".into(),
                    "-dBlackPtComp=1".into(),
                ],
            ),
        ],
    };
    let mut rendered = Vec::new();
    for (k, (cmyk, extra)) in views.iter().enumerate() {
        let a = gs::render(input, &tmp.join(format!("a{k}")), 72, *cmyk, extra)?;
        let b = gs::render(out, &tmp.join(format!("b{k}")), 72, *cmyk, extra)?;
        if a.len() != b.len() {
            bail!("page count changed: {} -> {}", a.len(), b.len());
        }
        rendered.push((a, b, *cmyk));
    }
    let pages = rendered[0].0.len();
    let mut worst = (0usize, 0.0f64);
    for i in 0..pages {
        let (mut ab, mut ba): (Vec<bool>, Vec<bool>) = (Vec::new(), Vec::new());
        for (a, b, cmyk) in &rendered {
            let (pa, pb) = (gs::load(&a[i], *cmyk)?, gs::load(&b[i], *cmyk)?);
            if (pa.width, pa.height) != (pb.width, pb.height) {
                bail!("page {} changed size", i + 1);
            }
            merge(&mut ab, explained(&pa, &pb, tol.level));
            merge(&mut ba, explained(&pb, &pa, tol.level));
        }
        let off = ab
            .iter()
            .filter(|e| !**e)
            .count()
            .max(ba.iter().filter(|e| !**e).count());
        let share = off as f64 / ab.len().max(1) as f64;
        if share > worst.1 {
            worst = (i + 1, share);
        }
        if share > tol.max_share {
            bail!(
                "page {} looks different: {:.2} % of it changed by more than {} grey levels (limit {:.2} %)",
                i + 1,
                share * 100.0,
                tol.level,
                tol.max_share * 100.0
            );
        }
    }
    println!(
        "  verified: {} pages render alike (most changed: page {}, {:.3} % of its pixels)",
        pages,
        worst.0.max(1),
        worst.1 * 100.0
    );
    Ok(())
}

/// Adds the printing condition's output intent when the PDF has none. Returns whether it added it.
fn add_output_intent(doc: &mut Document, profile: &Path) -> Result<bool> {
    if doc.catalog()?.has(b"OutputIntents") {
        return Ok(false);
    }
    let data =
        std::fs::read(profile).with_context(|| format!("cannot read {}", profile.display()))?;
    let mut sd = Dictionary::new();
    sd.set("N", 4);
    sd.set("Filter", "FlateDecode");
    let sid = doc.add_object(Stream::new(sd, zlib(&data)?));
    let default = profile
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case(DEFAULT_PROFILE_NAME));
    let (id, info) = if default {
        ("FOGRA39", "Coated FOGRA39 (ISO 12647-2:2004)".to_string())
    } else {
        (
            "Custom",
            profile
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
        )
    };
    let s = |t: &str| Object::String(t.as_bytes().to_vec(), StringFormat::Literal);
    let mut oi = Dictionary::new();
    oi.set("Type", Object::Name(b"OutputIntent".to_vec()));
    oi.set("S", Object::Name(b"GTS_PDFX".to_vec()));
    oi.set("OutputConditionIdentifier", s(id));
    oi.set("OutputCondition", s(&info));
    oi.set("Info", s(&info));
    oi.set("RegistryName", s("http://www.color.org"));
    oi.set("DestOutputProfile", Object::Reference(sid));
    doc.catalog_mut()?
        .set("OutputIntents", Object::Array(vec![Object::Dictionary(oi)]));
    Ok(true)
}

/// For each pixel of `a`: whether some pixel of `b` within one pixel around it is closer than
/// `level` grey levels. A line or a glyph edge rasterised one pixel off is explained by its
/// neighbour; missing or changed content is not.
fn explained(a: &gs::Page, b: &gs::Page, level: u8) -> Vec<bool> {
    let (w, h) = (a.width, a.height);
    let mut out = vec![true; w * h];
    for y in 0..h {
        for x in 0..w {
            let v = a.pixels[y * w + x];
            let near = |yy: usize, xx: usize| b.pixels[yy * w + xx].abs_diff(v) <= level;
            if near(y, x) {
                continue;
            }
            out[y * w + x] = (y.saturating_sub(1)..=(y + 1).min(h - 1))
                .any(|yy| (x.saturating_sub(1)..=(x + 1).min(w - 1)).any(|xx| near(yy, xx)));
        }
    }
    out
}

/// Pixels explained by any view so far.
fn merge(acc: &mut Vec<bool>, view: Vec<bool>) {
    if acc.is_empty() {
        *acc = view;
    } else {
        acc.iter_mut().zip(view).for_each(|(a, v)| *a |= v);
    }
}

#[cfg(test)]
fn unexplained(a: &gs::Page, b: &gs::Page, level: u8) -> usize {
    explained(a, b, level).iter().filter(|e| !**e).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(w: usize, h: usize, dark: &[(usize, usize)]) -> gs::Page {
        let mut pixels = vec![255u8; w * h];
        for (x, y) in dark {
            pixels[y * w + x] = 0;
        }
        gs::Page {
            width: w,
            height: h,
            pixels,
        }
    }

    #[test]
    fn a_line_one_pixel_off_is_explained_but_missing_content_is_not() {
        let line: Vec<(usize, usize)> = (0..8).map(|y| (3, y)).collect();
        let moved: Vec<(usize, usize)> = (0..8).map(|y| (4, y)).collect();
        let (a, b) = (page(8, 8, &line), page(8, 8, &moved));
        assert_eq!(unexplained(&a, &b, 48).max(unexplained(&b, &a, 48)), 0);
        let empty = page(8, 8, &[]);
        assert_eq!(unexplained(&a, &empty, 48), 8);
    }
}
