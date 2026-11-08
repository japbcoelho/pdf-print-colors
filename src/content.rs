//! Content-stream passes: grey to black ink (`g` -> `0 0 0 k`); registration marks as process
//! colours before Ghostscript; and, before a CMYK conversion, neutral RGB to grey so pure black
//! and greys end up on the black plate only.

#![allow(clippy::ptr_arg)] // every content pass has one signature; neutral_rgb_to_gray needs the Vec

use anyhow::Result;
use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, ObjectId};

use crate::util::{Family, Spaces, content_streams, k_of_gray, num, round4};

const NEUTRAL_EPS: f32 = 1e-4;

#[derive(Default, Debug)]
pub struct ContentStats {
    /// `g`/`G` rewritten as `k`/`K`.
    pub gray_ops: usize,
    /// Colours set in a grey colour space (`/DeviceGray cs` + `sc`), rewritten.
    pub gray_space_ops: usize,
    /// Neutral RGB colours (r = g = b) rewritten as grey before Ghostscript.
    pub neutral_rgb_ops: usize,
    /// Registration-colour (`/Separation /All`) colours set as process colours before Ghostscript.
    pub registration_ops: usize,
    /// Content streams that could not be parsed and were left as they are.
    pub unreadable: usize,
}

fn nums(ops: &[Object]) -> Option<Vec<f32>> {
    ops.iter().map(num).collect()
}

fn neutral(v: &[f32]) -> bool {
    v.len() == 3 && (v[0] - v[1]).abs() < NEUTRAL_EPS && (v[1] - v[2]).abs() < NEUTRAL_EPS
}

/// Greys to black ink: `v g` -> `0 0 0 (1-v) k` (and `G`/`K`); a grey colour space chosen with
/// `cs`/`CS` becomes `/DeviceCMYK` and its `sc`/`scn` colours follow.
pub fn gray_to_k(ops: &mut Vec<Operation>, spaces: &Spaces, st: &mut ContentStats) -> bool {
    to_k(ops, spaces, st, false)
}

/// As [`gray_to_k`], and the registration colour (`/Separation /All`, printer's marks on every
/// plate) too: printed with black ink only, its tint `t` becomes `0 0 0 t`.
pub fn gray_and_registration_to_k(
    ops: &mut Vec<Operation>,
    spaces: &Spaces,
    st: &mut ContentStats,
) -> bool {
    to_k(ops, spaces, st, true)
}

fn to_k(ops: &mut [Operation], spaces: &Spaces, st: &mut ContentStats, registration: bool) -> bool {
    /// What the colours of the current colour space become.
    #[derive(Clone, Copy, Default, PartialEq)]
    enum Conv {
        #[default]
        Keep,
        Gray,
        Tint,
    }
    let mut changed = false;
    let (mut fill, mut stroke) = (Conv::Keep, Conv::Keep);
    let mut stack: Vec<(Conv, Conv)> = Vec::new();
    for op in ops.iter_mut() {
        let o = op.operator.clone();
        let is_fill = o.chars().next().map(|c| c.is_lowercase()).unwrap_or(true);
        match o.as_str() {
            "q" => stack.push((fill, stroke)),
            "Q" => (fill, stroke) = stack.pop().unwrap_or_default(),
            "g" | "G" if op.operands.len() == 1 => {
                if let Some(v) = num(&op.operands[0]) {
                    *op = Operation::new(if is_fill { "k" } else { "K" }, k_of_gray(v));
                    st.gray_ops += 1;
                    changed = true;
                }
                if is_fill {
                    fill = Conv::Keep
                } else {
                    stroke = Conv::Keep
                }
            }
            "rg" | "RG" | "k" | "K" => {
                if is_fill {
                    fill = Conv::Keep
                } else {
                    stroke = Conv::Keep
                }
            }
            "cs" | "CS" if op.operands.len() == 1 => {
                let f = op.operands[0]
                    .as_name()
                    .map(|n| spaces.of(n))
                    .unwrap_or(Family::Unknown);
                let conv = match f {
                    Family::Gray => Conv::Gray,
                    Family::Registration if registration => Conv::Tint,
                    _ => Conv::Keep,
                };
                if conv != Conv::Keep {
                    op.operands[0] = Object::Name(b"DeviceCMYK".to_vec());
                    st.gray_space_ops += 1;
                    changed = true;
                }
                if is_fill { fill = conv } else { stroke = conv }
            }
            "sc" | "scn" | "SC" | "SCN" => {
                let conv = if is_fill { fill } else { stroke };
                if conv != Conv::Keep
                    && op.operands.len() == 1
                    && let Some(v) = num(&op.operands[0])
                {
                    op.operands = k_of_gray(if conv == Conv::Gray { v } else { 1.0 - v });
                    st.gray_space_ops += 1;
                    changed = true;
                }
            }
            _ => {}
        }
    }
    changed
}

/// Registration colour (`/Separation /All`, printer's marks) as DeviceCMYK before Ghostscript,
/// which rewrites `/All` marks at 100 % as 0 % (invisible) when it converts colours. On a CMYK
/// job `/All` prints on the four process plates, so tint t becomes `t t t t`.
pub fn registration_to_all_inks(
    ops: &mut Vec<Operation>,
    spaces: &Spaces,
    st: &mut ContentStats,
) -> bool {
    registration(ops, spaces, st, true)
}

/// As [`registration_to_all_inks`] for a job printed with black ink only: tint t becomes `0 0 0 t`.
pub fn registration_to_black(
    ops: &mut Vec<Operation>,
    spaces: &Spaces,
    st: &mut ContentStats,
) -> bool {
    registration(ops, spaces, st, false)
}

fn registration(
    ops: &mut [Operation],
    spaces: &Spaces,
    st: &mut ContentStats,
    all_inks: bool,
) -> bool {
    let mut changed = false;
    let (mut fill, mut stroke) = (false, false);
    let mut stack: Vec<(bool, bool)> = Vec::new();
    for op in ops.iter_mut() {
        let o = op.operator.clone();
        let is_fill = o.chars().next().map(|c| c.is_lowercase()).unwrap_or(true);
        match o.as_str() {
            "q" => stack.push((fill, stroke)),
            "Q" => (fill, stroke) = stack.pop().unwrap_or_default(),
            "g" | "G" | "rg" | "RG" | "k" | "K" => {
                if is_fill {
                    fill = false
                } else {
                    stroke = false
                }
            }
            "cs" | "CS" if op.operands.len() == 1 => {
                let reg = op.operands[0]
                    .as_name()
                    .map(|n| spaces.of(n) == Family::Registration)
                    .unwrap_or(false);
                if reg {
                    op.operands[0] = Object::Name(b"DeviceCMYK".to_vec());
                    changed = true;
                }
                if is_fill { fill = reg } else { stroke = reg }
            }
            "sc" | "scn" | "SC" | "SCN" => {
                let reg = if is_fill { fill } else { stroke };
                if reg
                    && op.operands.len() == 1
                    && let Some(t) = num(&op.operands[0])
                {
                    let t = Object::Real(round4(t.clamp(0.0, 1.0)));
                    op.operands = if all_inks {
                        vec![t.clone(), t.clone(), t.clone(), t]
                    } else {
                        vec![0.into(), 0.into(), 0.into(), t]
                    };
                    st.registration_ops += 1;
                    changed = true;
                }
            }
            _ => {}
        }
    }
    changed
}
/// Neutral RGB to grey, before Ghostscript converts RGB to CMYK through the ICC profile (which
/// would make pure black a four-ink rich black): `r r r rg` -> `r g`, and `r r r sc` in an RGB
/// colour space -> `r g`. When a later colour of that RGB space is not neutral, the space is set
/// again with `cs` first, so every other colour keeps its meaning.
pub fn neutral_rgb_to_gray(
    ops: &mut Vec<Operation>,
    spaces: &Spaces,
    st: &mut ContentStats,
) -> bool {
    #[derive(Clone, Default)]
    struct Side {
        name: Vec<u8>,
        rgb: bool,
        /// we replaced a colour of this RGB space with `g`, so the current space is DeviceGray
        now_gray: bool,
    }
    let mut changed = false;
    let mut fill = Side::default();
    let mut stroke = Side::default();
    let mut stack: Vec<(Side, Side)> = Vec::new();
    let mut out: Vec<Operation> = Vec::with_capacity(ops.len());
    for op in ops.drain(..) {
        let o = op.operator.as_str();
        let is_fill = o.chars().next().map(|c| c.is_lowercase()).unwrap_or(true);
        match o {
            "q" => stack.push((fill.clone(), stroke.clone())),
            "Q" => (fill, stroke) = stack.pop().unwrap_or_default(),
            _ => {}
        }
        let side = if is_fill { &mut fill } else { &mut stroke };
        match o {
            "rg" | "RG" => {
                let v = nums(&op.operands).unwrap_or_default();
                *side = Side {
                    name: b"DeviceRGB".to_vec(),
                    rgb: true,
                    now_gray: false,
                };
                if neutral(&v) {
                    out.push(Operation::new(
                        if is_fill { "g" } else { "G" },
                        vec![Object::Real(round4(v[0]))],
                    ));
                    side.now_gray = true;
                    st.neutral_rgb_ops += 1;
                    changed = true;
                    continue;
                }
            }
            "g" | "G" => {
                *side = Side {
                    name: b"DeviceGray".to_vec(),
                    rgb: false,
                    now_gray: false,
                }
            }
            "k" | "K" => {
                *side = Side {
                    name: b"DeviceCMYK".to_vec(),
                    rgb: false,
                    now_gray: false,
                }
            }
            "cs" | "CS" => {
                let name = op
                    .operands
                    .first()
                    .and_then(|n| n.as_name().ok())
                    .map(|n| n.to_vec())
                    .unwrap_or_default();
                let rgb = spaces.of(&name) == Family::Rgb;
                *side = Side {
                    name,
                    rgb,
                    now_gray: false,
                };
            }
            "sc" | "scn" | "SC" | "SCN" if side.rgb => {
                if let Some(v) = nums(&op.operands).filter(|v| v.len() == 3) {
                    if neutral(&v) {
                        out.push(Operation::new(
                            if is_fill { "g" } else { "G" },
                            vec![Object::Real(round4(v[0]))],
                        ));
                        side.now_gray = true;
                        st.neutral_rgb_ops += 1;
                        changed = true;
                        continue;
                    }
                    if side.now_gray {
                        out.push(Operation::new(
                            if is_fill { "cs" } else { "CS" },
                            vec![Object::Name(side.name.clone())],
                        ));
                        side.now_gray = false;
                    }
                }
            }
            _ => {}
        }
        out.push(op);
    }
    *ops = out;
    changed
}

/// Applies `pass` to every page and every stream with content of its own (forms, tiling
/// patterns, Type 3 glyphs).
pub fn rewrite_all(
    doc: &mut Document,
    pass: fn(&mut Vec<Operation>, &Spaces, &mut ContentStats) -> bool,
) -> Result<ContentStats> {
    let mut st = ContentStats::default();
    let pages: Vec<ObjectId> = doc.get_pages().values().copied().collect();
    for page in pages {
        let spaces = Spaces::of_page(doc, page);
        let data = doc.get_page_content(page);
        let Ok(mut content) = Content::decode(&data) else {
            st.unreadable += 1;
            continue;
        };
        if pass(&mut content.operations, &spaces, &mut st) {
            doc.change_page_content(page, content.encode()?)?;
        }
    }
    for (id, spaces) in content_streams(doc) {
        let Ok(stream) = doc.get_object_mut(id).and_then(Object::as_stream_mut) else {
            continue;
        };
        let Ok(data) = stream.decompressed_content().or_else(|e| {
            if stream.filters().map(|f| f.is_empty()).unwrap_or(true) {
                Ok(stream.content.clone())
            } else {
                Err(e)
            }
        }) else {
            st.unreadable += 1;
            continue;
        };
        let Ok(mut content) = Content::decode(&data) else {
            st.unreadable += 1;
            continue;
        };
        if pass(&mut content.operations, &spaces, &mut st) {
            stream.set_plain_content(content.encode()?);
            let _ = stream.compress();
        }
    }
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        src: &str,
        pass: fn(&mut Vec<Operation>, &Spaces, &mut ContentStats) -> bool,
        spaces: &Spaces,
    ) -> (String, ContentStats) {
        let mut content = Content::decode(src.as_bytes()).unwrap();
        let mut st = ContentStats::default();
        pass(&mut content.operations, spaces, &mut st);
        (
            String::from_utf8(content.encode().unwrap())
                .unwrap()
                .replace('\n', " "),
            st,
        )
    }

    #[test]
    fn grey_operators_become_k_only() {
        let (out, st) = run("0.25 g 0 0 10 10 re f 1 G", gray_to_k, &Spaces::default());
        assert_eq!(st.gray_ops, 2);
        assert!(
            out.contains("0 0 0 0.75 k") && out.contains("0 0 0 0 K"),
            "{out}"
        );
    }

    #[test]
    fn grey_space_by_resource_name_follows_save_and_restore() {
        let mut spaces = Spaces::default();
        spaces.0.insert(b"CS0".to_vec(), Family::Gray);
        let (out, st) = run("q /CS0 cs 0.5 sc Q 0.5 sc", gray_to_k, &spaces);
        assert_eq!(st.gray_space_ops, 2);
        assert!(
            out.contains("/DeviceCMYK cs") && out.contains("0 0 0 0.5 sc"),
            "{out}"
        );
        assert!(
            out.trim_end().ends_with("0.5 sc"),
            "after Q the space is no longer grey: {out}"
        );
    }

    #[test]
    fn registration_becomes_k_only_in_black_ink_and_stays_otherwise() {
        let mut spaces = Spaces::default();
        spaces.0.insert(b"CS1".to_vec(), Family::Registration);
        let (out, _) = run("/CS1 CS 1 SCN", gray_and_registration_to_k, &spaces);
        assert_eq!(out.trim(), "/DeviceCMYK CS 0 0 0 1 SCN");
        let (kept, _) = run("/CS1 CS 1 SCN", gray_to_k, &spaces);
        assert_eq!(kept.trim(), "/CS1 CS 1 SCN");
    }

    #[test]
    fn neutral_rgb_becomes_grey_and_colours_keep_their_space() {
        let mut spaces = Spaces::default();
        spaces.0.insert(b"CS0".to_vec(), Family::Rgb);
        let (out, st) = run(
            "0 0 0 rg 1 0 0 RG /CS0 cs 0.2 0.2 0.2 scn 1 0 0 scn",
            neutral_rgb_to_gray,
            &spaces,
        );
        assert_eq!(st.neutral_rgb_ops, 2);
        assert!(out.starts_with("0 g 1 0 0 RG"), "{out}");
        assert!(
            out.ends_with("/CS0 cs 0.2 g /CS0 cs 1 0 0 scn"),
            "the RGB space is set again before a colour: {out}"
        );
    }
}
