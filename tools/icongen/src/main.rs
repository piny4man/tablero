//! icongen — flatten `icons/*.svg` into `crates/tablero/src/builtin_icon_paths.rs`.
//!
//! This is a development tool, **never** linked into the tablero binary. It reads
//! every SVG under `icons/`, applies any `<g transform>` matrices, flattens the
//! path data to absolute cubic Béziers (via `svgtypes`), preserves each
//! `<path>`'s winding rule, and emits a single `#[rustfmt::skip]` data module of
//! static `Cmd` tables. tablero then renders those tables with `tiny-skia` at
//! runtime — no SVG parser or bundled asset ships in the bar.
//!
//! Run from anywhere in the workspace:
//!
//! ```text
//! cargo run -p icongen
//! ```
//!
//! Then review and commit the regenerated file.

use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use roxmltree::{Document, Node};
use svgtypes::{SimplePathSegment, SimplifyingPathParser};

/// A 2×3 affine matrix `[a c e; b d f]` in the SVG convention.
#[derive(Clone, Copy)]
struct Matrix {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl Matrix {
    const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    /// `self ∘ other`: the composed transform that applies `other` first.
    fn compose(self, o: Matrix) -> Matrix {
        Matrix {
            a: self.a * o.a + self.c * o.b,
            b: self.b * o.a + self.d * o.b,
            c: self.a * o.c + self.c * o.d,
            d: self.b * o.c + self.d * o.d,
            e: self.a * o.e + self.c * o.f + self.e,
            f: self.b * o.e + self.d * o.f + self.f,
        }
    }

    fn apply(self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }
}

/// Parse an SVG `transform` attribute into a single collapsed matrix.
///
/// Supports the `matrix`, `translate`, and `scale` functions actually used by
/// the icon set (with a general `rotate(angle)` fallback); unknown functions
/// collapse to identity so an unexpected transform degrades to a no-op rather
/// than a panic.
fn parse_transform(spec: &str) -> Matrix {
    let mut matrix = Matrix::IDENTITY;
    let mut rest = spec;
    while let Some(open) = rest.find('(') {
        let name = rest[..open]
            .rsplit(|c: char| c.is_whitespace() || c == ',' || c == ')')
            .find(|token| !token.is_empty())
            .unwrap_or("");
        let Some(close_rel) = rest[open..].find(')') else {
            break;
        };
        let close = open + close_rel;
        let args: Vec<f64> = rest[open + 1..close]
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|token| !token.is_empty())
            .filter_map(|token| token.parse::<f64>().ok())
            .collect();
        let arg = |i: usize, default: f64| args.get(i).copied().unwrap_or(default);
        let step = match name {
            "matrix" if args.len() == 6 => Matrix {
                a: args[0],
                b: args[1],
                c: args[2],
                d: args[3],
                e: args[4],
                f: args[5],
            },
            "translate" => Matrix {
                e: arg(0, 0.0),
                f: arg(1, 0.0),
                ..Matrix::IDENTITY
            },
            "scale" => {
                let sx = arg(0, 1.0);
                Matrix {
                    a: sx,
                    d: arg(1, sx),
                    ..Matrix::IDENTITY
                }
            }
            "rotate" => {
                let r = arg(0, 0.0).to_radians();
                Matrix {
                    a: r.cos(),
                    b: r.sin(),
                    c: -r.sin(),
                    d: r.cos(),
                    ..Matrix::IDENTITY
                }
            }
            _ => Matrix::IDENTITY,
        };
        matrix = matrix.compose(step);
        rest = &rest[close + 1..];
    }
    matrix
}

/// One flattened drawing command, mirroring the runtime `Cmd` enum.
#[derive(Clone, Copy)]
enum Cmd {
    Move(f64, f64),
    Line(f64, f64),
    Cubic(f64, f64, f64, f64, f64, f64),
    Close,
}

/// One `<path>` element: its winding rule and flattened command stream.
struct SubPath {
    even_odd: bool,
    cmds: Vec<Cmd>,
}

/// One icon: source stem, viewBox extent, and ordered sub-paths.
struct Icon {
    name: String,
    view_w: f64,
    view_h: f64,
    paths: Vec<SubPath>,
}

/// Flatten one `<path>` `d` string (already-resolved `transform`) to cubics.
fn flatten_path(d: &str, transform: Matrix) -> Vec<Cmd> {
    let mut cmds = Vec::new();
    let mut cur = (0.0, 0.0);
    let mut start = (0.0, 0.0);
    for segment in SimplifyingPathParser::from(d) {
        let Ok(segment) = segment else { continue };
        match segment {
            SimplePathSegment::MoveTo { x, y } => {
                let p = transform.apply(x, y);
                cmds.push(Cmd::Move(p.0, p.1));
                cur = p;
                start = p;
            }
            SimplePathSegment::LineTo { x, y } => {
                let p = transform.apply(x, y);
                cmds.push(Cmd::Line(p.0, p.1));
                cur = p;
            }
            SimplePathSegment::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => {
                let c1 = transform.apply(x1, y1);
                let c2 = transform.apply(x2, y2);
                let p = transform.apply(x, y);
                cmds.push(Cmd::Cubic(c1.0, c1.1, c2.0, c2.1, p.0, p.1));
                cur = p;
            }
            // Elevate a quadratic to a cubic. The conversion is affine-covariant,
            // so transforming the (already transformed) endpoints and control
            // point yields the same curve as converting in user space first.
            SimplePathSegment::Quadratic { x1, y1, x, y } => {
                let q = transform.apply(x1, y1);
                let e = transform.apply(x, y);
                let c1 = (
                    cur.0 + 2.0 / 3.0 * (q.0 - cur.0),
                    cur.1 + 2.0 / 3.0 * (q.1 - cur.1),
                );
                let c2 = (e.0 + 2.0 / 3.0 * (q.0 - e.0), e.1 + 2.0 / 3.0 * (q.1 - e.1));
                cmds.push(Cmd::Cubic(c1.0, c1.1, c2.0, c2.1, e.0, e.1));
                cur = e;
            }
            SimplePathSegment::ClosePath => {
                cmds.push(Cmd::Close);
                cur = start;
            }
        }
    }
    cmds
}

/// Collect drawable `<path>` elements in document order, skipping `<defs>` and
/// `<clipPath>` (clip geometry is not painted), accumulating ancestor transforms.
fn collect_paths(node: Node, transform: Matrix, out: &mut Vec<SubPath>) {
    for child in node.children() {
        if !child.is_element() {
            continue;
        }
        let tag = child.tag_name().name();
        if tag == "defs" || tag == "clipPath" {
            continue;
        }
        let local = child
            .attribute("transform")
            .map_or(Matrix::IDENTITY, parse_transform);
        let combined = transform.compose(local);
        if tag == "path" {
            if let Some(d) = child.attribute("d") {
                let even_odd = child.attribute("fill-rule") == Some("evenodd");
                let cmds = flatten_path(d, combined);
                if !cmds.is_empty() {
                    out.push(SubPath { even_odd, cmds });
                }
            }
        }
        collect_paths(child, combined, out);
    }
}

/// Parse the viewBox width/height, falling back to `width`/`height` attributes.
fn view_extent(svg: Node) -> (f64, f64) {
    if let Some(view_box) = svg.attribute("viewBox") {
        let nums: Vec<f64> = view_box
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|token| !token.is_empty())
            .filter_map(|token| token.parse::<f64>().ok())
            .collect();
        if nums.len() == 4 && nums[2] > 0.0 && nums[3] > 0.0 {
            return (nums[2], nums[3]);
        }
    }
    let dim = |name| svg.attribute(name).and_then(|v| v.parse::<f64>().ok());
    (dim("width").unwrap_or(24.0), dim("height").unwrap_or(24.0))
}

fn parse_icon(path: &Path) -> Result<Icon, Box<dyn Error>> {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("icon file has no stem")?
        .to_string();
    let text = fs::read_to_string(path)?;
    let doc = Document::parse(&text)?;
    let svg = doc.root_element();
    let (view_w, view_h) = view_extent(svg);
    let mut paths = Vec::new();
    collect_paths(svg, Matrix::IDENTITY, &mut paths);
    Ok(Icon {
        name,
        view_w,
        view_h,
        paths,
    })
}

/// Format an `f64` as a compact `f32` literal, rounded to four decimals.
fn num(v: f64) -> String {
    let rounded = (v * 10_000.0).round() / 10_000.0;
    let rounded = if rounded == 0.0 { 0.0 } else { rounded };
    format!("{rounded}f32")
}

fn emit(icons: &[Icon]) -> String {
    let mut out = String::new();
    out.push_str(
        "// @generated by `cargo run -p icongen` from icons/*.svg — do not edit by hand.\n\
         //\n\
         // Every SVG is flattened to absolute cubic Béziers at generation time, so the\n\
         // bar renders these static tables with tiny-skia and ships no SVG parser,\n\
         // bundled icon font, or other icon-rendering dependency.\n\n\
         // Coordinates are literal path data; a stray value near a math constant\n\
         // (e.g. 6.28 ≈ TAU) is a coincidence, not an approximation.\n\
         #![allow(clippy::approx_constant)]\n\n",
    );
    out.push_str("/// One flattened drawing command in the icon's own viewBox space.\n");
    out.push_str("#[derive(Clone, Copy)]\n");
    out.push_str("pub(super) enum Cmd {\n");
    out.push_str("    Move(f32, f32),\n");
    out.push_str("    Line(f32, f32),\n");
    out.push_str("    Cubic(f32, f32, f32, f32, f32, f32),\n");
    out.push_str("    Close,\n");
    out.push_str("}\n\n");
    out.push_str("/// One `<path>` element: its winding rule and command stream.\n");
    out.push_str("pub(super) struct SubPath {\n");
    out.push_str("    pub even_odd: bool,\n");
    out.push_str("    pub cmds: &'static [Cmd],\n");
    out.push_str("}\n\n");
    out.push_str("/// One icon: source stem, viewBox extent, and ordered sub-paths.\n");
    out.push_str("pub(super) struct RawIcon {\n");
    out.push_str("    pub name: &'static str,\n");
    out.push_str("    pub view_w: f32,\n");
    out.push_str("    pub view_h: f32,\n");
    out.push_str("    pub paths: &'static [SubPath],\n");
    out.push_str("}\n\n");

    out.push_str("#[rustfmt::skip]\n");
    out.push_str("pub(super) const ICONS: &[RawIcon] = &[\n");
    for icon in icons {
        let _ = writeln!(
            out,
            "    RawIcon {{ name: {:?}, view_w: {}, view_h: {}, paths: &[",
            icon.name,
            num(icon.view_w),
            num(icon.view_h),
        );
        for sub in &icon.paths {
            let _ = writeln!(
                out,
                "        SubPath {{ even_odd: {}, cmds: &[",
                sub.even_odd
            );
            for cmd in &sub.cmds {
                let line = match *cmd {
                    Cmd::Move(x, y) => format!("Cmd::Move({}, {})", num(x), num(y)),
                    Cmd::Line(x, y) => format!("Cmd::Line({}, {})", num(x), num(y)),
                    Cmd::Cubic(x1, y1, x2, y2, x, y) => format!(
                        "Cmd::Cubic({}, {}, {}, {}, {}, {})",
                        num(x1),
                        num(y1),
                        num(x2),
                        num(y2),
                        num(x),
                        num(y),
                    ),
                    Cmd::Close => "Cmd::Close".to_string(),
                };
                let _ = writeln!(out, "            {line},");
            }
            out.push_str("        ] },\n");
        }
        out.push_str("    ] },\n");
    }
    out.push_str("];\n");
    out
}

fn main() -> Result<(), Box<dyn Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let icons_dir = root.join("icons");
    let out_path = root.join("crates/tablero/src/builtin_icon_paths.rs");

    let mut svg_paths: Vec<PathBuf> = fs::read_dir(&icons_dir)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "svg"))
        .collect();
    svg_paths.sort();

    let mut icons = Vec::with_capacity(svg_paths.len());
    for path in &svg_paths {
        let icon = parse_icon(path)?;
        if icon.paths.is_empty() {
            eprintln!("warning: {} produced no paths", path.display());
        }
        icons.push(icon);
    }

    fs::write(&out_path, emit(&icons))?;
    println!("wrote {} icons to {}", icons.len(), out_path.display());
    Ok(())
}
