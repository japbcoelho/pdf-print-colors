//! Ghostscript runs: the colour conversion (pdfwrite), grey renders for the visual check and the
//! ink coverage per page (inkcov).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const BASE: [&str; 4] = ["-q", "-dNOPAUSE", "-dBATCH", "-dSAFER"];

/// Image settings for pdfwrite: every image Ghostscript writes is lossless Flate, at its own
/// resolution, and never an inline image (so the passes after it see every image).
pub const LOSSLESS_IMAGES: [&str; 13] = [
    "-dAutoFilterColorImages=false",
    "-dAutoFilterGrayImages=false",
    "-dColorImageFilter=/FlateEncode",
    "-dGrayImageFilter=/FlateEncode",
    "-dMonoImageFilter=/FlateEncode",
    "-dDownsampleColorImages=false",
    "-dDownsampleGrayImages=false",
    "-dDownsampleMonoImages=false",
    "-dPassThroughJPEGImages=false",
    "-dPassThroughJPXImages=false",
    "-dMaxInlineImageSize=0",
    "-dDetectDuplicateImages=true",
    "-dEmbedAllFonts=true",
];

fn gs() -> Command {
    let mut c = Command::new("gs");
    c.args(BASE);
    c
}

/// `gs -sDEVICE=pdfwrite ARGS -o OUTPUT [POST...] INPUT`; returns the time taken and what
/// Ghostscript printed (warnings about repaired input, for example).
pub fn pdfwrite(
    input: &Path,
    output: &Path,
    args: &[String],
    post: &[String],
) -> Result<(Duration, String)> {
    let t = Instant::now();
    let out = gs()
        .arg("-sDEVICE=pdfwrite")
        .args(args)
        .arg("-o")
        .arg(output)
        .args(post)
        .arg(input)
        .output()
        .context("cannot run gs (Ghostscript)")?;
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        bail!("Ghostscript failed ({}):\n{}", out.status, msg.trim());
    }
    Ok((t.elapsed(), msg.trim().to_string()))
}

pub struct Page {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

/// Renders every page at `dpi` into `dir`, one file per page, in page order; `load` reads one
/// back as grey. With `cmyk`, pages are rendered in CMYK (`pamcmyk32`, through the profiles in
/// `extra`); otherwise in 8-bit grey (`pgmraw`).
pub fn render(
    pdf: &Path,
    dir: &Path,
    dpi: u32,
    cmyk: bool,
    extra: &[String],
) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let out = gs()
        .arg(if cmyk {
            "-sDEVICE=pamcmyk32"
        } else {
            "-sDEVICE=pgmraw"
        })
        .arg(format!("-r{dpi}"))
        .args(extra)
        .arg(format!("-sOutputFile={}/%06d.img", dir.display()))
        .arg(pdf)
        .output()
        .context("cannot run gs (Ghostscript)")?;
    if !out.status.success() {
        bail!(
            "Ghostscript could not render {}: {}",
            pdf.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    files.sort();
    Ok(files)
}

/// One rendered page as grey; a CMYK page through 1 - min(1, 0.3c + 0.59m + 0.11y + k).
pub fn load(file: &Path, cmyk: bool) -> Result<Page> {
    let b = std::fs::read(file)?;
    if cmyk { pam_cmyk_gray(&b) } else { pgm(&b) }
}

/// A binary PGM (P5) file.
fn pgm(b: &[u8]) -> Result<Page> {
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < b.len() && b[i] == b'#' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        if start == i {
            bail!("bad PGM file");
        }
        fields.push(String::from_utf8_lossy(&b[start..i]).to_string());
    }
    let width: usize = fields[1].parse()?;
    let height: usize = fields[2].parse()?;
    let pixels = b
        .get(i + 1..i + 1 + width * height)
        .context("short PGM file")?
        .to_vec();
    Ok(Page {
        width,
        height,
        pixels,
    })
}

/// Ink coverage of every page, `[C, M, Y, K]` as fractions of the page area.
pub fn inkcov(pdf: &Path, extra: &[String]) -> Result<Vec<[f64; 4]>> {
    let out = gs()
        .args(["-sDEVICE=inkcov", "-r36", "-o", "-"])
        .args(extra)
        .arg(pdf)
        .output()
        .context("cannot run gs (Ghostscript)")?;
    if !out.status.success() {
        bail!(
            "Ghostscript inkcov failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("CMYK"))
        .filter_map(|l| {
            let v: Vec<f64> = l
                .split_whitespace()
                .take(4)
                .filter_map(|x| x.parse().ok())
                .collect();
            (v.len() == 4).then(|| [v[0], v[1], v[2], v[3]])
        })
        .collect())
}

/// A CMYK PAM file (P7, DEPTH 4) as grey: 255 - min(255, 0.3c + 0.59m + 0.11y + k).
fn pam_cmyk_gray(b: &[u8]) -> Result<Page> {
    let end = b
        .windows(7)
        .position(|w| w == b"ENDHDR\n")
        .context("bad PAM file")?;
    let header = String::from_utf8_lossy(&b[..end]);
    let field = |key: &str| -> Result<usize> {
        header
            .lines()
            .find_map(|l| l.strip_prefix(key).map(|v| v.trim().parse::<usize>()))
            .context("bad PAM header")?
            .map_err(Into::into)
    };
    let (width, height) = (field("WIDTH ")?, field("HEIGHT ")?);
    if field("DEPTH ")? != 4 {
        bail!("PAM file is not CMYK");
    }
    let data = b
        .get(end + 7..end + 7 + width * height * 4)
        .context("short PAM file")?;
    let pixels = data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            let ink = 0.3 * p[0] as f32 + 0.59 * p[1] as f32 + 0.11 * p[2] as f32 + p[3] as f32;
            (255.0 - ink.min(255.0)).round() as u8
        })
        .collect();
    Ok(Page {
        width,
        height,
        pixels,
    })
}
