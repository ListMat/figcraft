//! Import of `.pen` (`pen.dev`) JSON documents and HTML clipboard payloads
//! (`data-pen-node-clipboard`), written from the published `.pen` schema specification only.
//!
//! Layers (`frame`, `group`, `rectangle`, `ellipse`, `polygon`, `path`, `line`, `text`, `ref`,
//! `note`, `icon`, `browser`, `script`), flexbox layout (`horizontal` / `vertical` / `none`,
//! `fit_content`, `fill_container`, `padding`, `gap`, `justifyContent`, `alignItems`,
//! `layoutPosition: "absolute"`), paints (solid colours, linear/radial/angular gradients, images,
//! mesh-gradient approximations), uniform and per-side strokes, blur and shadow effects, reusable
//! components, slots, `ref` instances with `descendants` overrides/replacements, and variables
//! with multi-axis themes are mapped into a [`Document`]. Unknown or runtime-only features (WebGL
//! shaders, scripts) degrade cleanly with warnings; malformed input never panics.

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use figcraft_doc::color::{BlendMode, Color, ColorStop, Gradient, GradientKind, ImagePaint, ImageScaleMode, Paint, PaintKind};
use figcraft_doc::geom::{Affine, BezPath, Point, Shape, Vec2};
use figcraft_doc::{
    AutoLayout, Blur, Collection, Corners, CrossAlign, DevResource, Document, Effect, EffectKind, LayoutMode, LetterSpacing, LineHeight, MainAlign,
    Mode, Node, NodeId, NodeKind, PropDef, Shadow, Sizing, StrokeAlign, StrokeCap, StrokeJoin, StrokeProps, TextAlignH, TextAlignV, TextAutoResize,
    TextData, TextDecoration, TextStyle, Variable, VariableType, VariableValue, VectorNetwork, WindingRule,
};
use serde_json::{Map, Value};

use crate::FormatError;
use crate::rest::ImageResolver;

/// Largest HTML clipboard string inspected (64 MiB).
const MAX_CLIPBOARD_HTML: usize = 64 * 1024 * 1024;
/// Most layers imported from one payload.
const MAX_NODES: usize = 200_000;
/// Deepest layer nesting followed.
const MAX_DEPTH: usize = 256;
/// Deepest `ref` component expansion chain followed.
const MAX_REF_DEPTH: usize = 16;
/// Longest SVG path string read from one `path` layer.
const MAX_PATH_BYTES: usize = 4 << 20;
/// Most path elements in one vector.
const MAX_PATH_ELEMENTS: usize = 200_000;
/// Largest inline `data:image/...` payload decoded (64 MiB).
const MAX_DATA_IMAGE_BYTES: usize = 64 * 1024 * 1024;
/// Largest coordinate magnitude kept.
const MAX_COORD: f64 = 1.0e7;
/// Largest width/height kept.
const MAX_SIZE: f64 = 1.0e6;
/// Most distinct warnings reported.
const MAX_WARNINGS: usize = 200;
/// Longest string kept for names and labels.
const MAX_NAME_CHARS: usize = 512;
/// Longest text content kept on a single text layer.
const MAX_TEXT_CHARS: usize = 200_000;
/// Placeholder key used when an image fill URL cannot be resolved.
const MISSING_IMAGE: &str = "pen-image-missing";

const PEN_NODE_TYPES: &[&str] =
    &["frame", "group", "rectangle", "ellipse", "polygon", "path", "line", "text", "note", "icon", "icon_font", "script", "browser", "ref"];

/// Extract the decoded JSON bytes from a Pen HTML clipboard payload
/// (`<span data-pen-node-clipboard="<base64>">…</span>`).
pub fn extract_clipboard_html(html: &str) -> Option<Vec<u8>> {
    const ATTR: &str = "data-pen-node-clipboard";
    if html.len() > MAX_CLIPBOARD_HTML {
        return None;
    }
    let pos = html.find(ATTR)?;
    let after = html.get(pos + ATTR.len()..)?.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let after_eq = after.strip_prefix('=')?.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let quote = after_eq.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let body = after_eq.get(quote.len_utf8()..)?;
    let end = body.find(quote)?;
    let raw_attr = body.get(..end)?;
    let unescaped = raw_attr.replace("&quot;", "\"").replace("&#43;", "+").replace("&#61;", "=").replace("&amp;", "&");
    let cleaned: String = unescaped.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if cleaned.is_empty() {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&cleaned)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&cleaned))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(&cleaned))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&cleaned))
        .ok()?;
    let v: Value = serde_json::from_slice(&decoded).ok()?;
    is_pen_json(&v).then_some(decoded)
}

/// Whether `text` is a plain-JSON `.pen` document, node, or clipboard payload.
pub fn looks_like_pen_json(text: &str) -> bool {
    if text.len() > MAX_CLIPBOARD_HTML {
        return false;
    }
    let trimmed = text.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if !trimmed.starts_with('{') {
        return false;
    }
    serde_json::from_str::<Value>(trimmed).is_ok_and(|v| is_pen_json(&v))
}

/// Whether `v` is a `.pen` JSON document, clipboard payload, or single `.pen` node.
pub fn is_pen_json(v: &Value) -> bool {
    let Some(obj) = v.as_object() else {
        return false;
    };
    if obj.contains_key("format") || obj.get("document").and_then(|d| d.get("type")).and_then(Value::as_str) == Some("DOCUMENT") {
        return false;
    }
    if obj.get("remoteData").and_then(|r| r.get("nodes")).is_some_and(Value::is_array) {
        return true;
    }
    if obj.get("nodes").and_then(Value::as_array).is_some_and(|a| a.iter().any(is_pen_node)) {
        return true;
    }
    if obj.get("version").is_some_and(Value::is_string) && obj.get("children").is_some_and(Value::is_array) {
        return true;
    }
    if obj.get("children").and_then(Value::as_array).is_some_and(|a| !a.is_empty() && a.iter().all(is_pen_node)) && !obj.contains_key("pages") {
        return true;
    }
    is_pen_node(v)
}

fn is_pen_node(v: &Value) -> bool {
    v.get("type").and_then(Value::as_str).is_some_and(|t| PEN_NODE_TYPES.contains(&t))
}

/// Import `bytes` when they are `.pen` JSON; `None` when they are another format.
pub fn try_import(bytes: &[u8], name: &str, images: ImageResolver) -> Option<Result<(Document, Vec<String>), FormatError>> {
    if bytes.len() > MAX_CLIPBOARD_HTML * 2 {
        return None;
    }
    let start = bytes.iter().position(|b| !b.is_ascii_whitespace())?;
    if bytes.get(start) != Some(&b'{') {
        return None;
    }
    let src = std::str::from_utf8(bytes).ok()?;
    let v: Value = serde_json::from_str(src).ok()?;
    is_pen_json(&v).then(|| import(&v, name, images))
}

/// Build a [`Document`] from a parsed `.pen` JSON value.
pub fn import(v: &Value, name: &str, images: ImageResolver) -> Result<(Document, Vec<String>), FormatError> {
    if !is_pen_json(v) {
        return Err(FormatError::Corrupt("not a valid .pen JSON payload".into()));
    }
    let doc_name = if name.trim().is_empty() { "Untitled".to_string() } else { clip_chars(name.trim(), MAX_NAME_CHARS) };
    let doc = Document::new(doc_name);
    let Some(page) = doc.first_page() else {
        return Err(FormatError::Corrupt("missing page".into()));
    };
    let mut cx = Ctx::new(doc, images);

    // Locate the root container (`remoteData` for clipboard payloads, else `v` itself).
    let root = v.get("remoteData").filter(|r| r.is_object()).unwrap_or(v);
    cx.load_variables_and_themes(root, v);

    let nodes_slice: Vec<Value> = if let Some(arr) = root.get("nodes").and_then(Value::as_array) {
        arr.clone()
    } else if let Some(arr) = root.get("children").and_then(Value::as_array) {
        arr.clone()
    } else if is_pen_node(root) {
        vec![root.clone()]
    } else {
        Vec::new()
    };

    // Index all node definitions (for `ref` expansion and `slot` recommendations).
    for extra_key in ["components", "definitions"] {
        if let Some(arr) = root.get(extra_key).or_else(|| v.get(extra_key)).and_then(Value::as_array) {
            for item in arr {
                cx.collect_defs(item, 0);
            }
        }
        if let Some(map) = root.get(extra_key).or_else(|| v.get(extra_key)).and_then(Value::as_object) {
            for (k, item) in map {
                if !k.is_empty() && item.is_object() {
                    cx.defs.entry(k.clone()).or_insert_with(|| item.clone());
                }
                cx.collect_defs(item, 0);
            }
        }
    }
    for item in &nodes_slice {
        cx.collect_defs(item, 0);
    }

    let default_theme = cx.default_theme.clone();
    let mut ref_stack = BTreeSet::new();
    for item in &nodes_slice {
        cx.build_node(item, page, None, false, &default_theme, 0, &mut ref_stack);
    }
    cx.resolve_slot_preferences();

    Ok((cx.doc, cx.warnings.finish()))
}

#[derive(Default)]
struct Warnings {
    list: Vec<(String, usize)>,
    index: BTreeMap<String, usize>,
}

impl Warnings {
    fn add(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if let Some(i) = self.index.get(&msg).copied() {
            if let Some(entry) = self.list.get_mut(i) {
                entry.1 = entry.1.saturating_add(1);
            }
        } else if self.list.len() < MAX_WARNINGS {
            self.index.insert(msg.clone(), self.list.len());
            self.list.push((msg, 1));
        }
    }

    fn finish(self) -> Vec<String> {
        self.list.into_iter().map(|(m, n)| if n > 1 { format!("{m} (×{n})") } else { m }).collect()
    }
}

#[derive(Clone)]
struct VarEntry {
    id: String,
    kind: VariableType,
    raw_entries: Vec<(BTreeMap<String, String>, Value)>,
}

#[derive(Clone, Copy)]
enum ParsedSize {
    Fixed(f64),
    Hug(Option<f64>),
    Fill(Option<f64>),
}

struct Ctx<'a> {
    doc: Document,
    images: ImageResolver<'a>,
    image_keys: BTreeMap<String, String>,
    missing_images: usize,
    warnings: Warnings,
    count: usize,
    /// `.pen` node `id` → JSON definition, for `ref` resolution.
    defs: BTreeMap<String, Value>,
    /// `.pen` node `id` → first imported `NodeId`.
    imported_ids: BTreeMap<String, NodeId>,
    /// `(component_node_id, slot_prop_name, preferred_pen_ids)` resolved after all nodes exist.
    pending_slots: Vec<(NodeId, String, Vec<String>)>,
    /// Collection id created for `.pen` variables/themes, if any.
    collection_id: Option<String>,
    /// Theme modes `(mode_id, axis_map)` in order (default first).
    theme_modes: Vec<(String, BTreeMap<String, String>)>,
    /// Default theme axis values (`axis -> first_value`).
    default_theme: BTreeMap<String, String>,
    /// Variable bare name (without leading `$`) → `VarEntry`.
    vars: BTreeMap<String, VarEntry>,
}

impl<'a> Ctx<'a> {
    fn new(doc: Document, images: ImageResolver<'a>) -> Self {
        Self {
            doc,
            images,
            image_keys: BTreeMap::new(),
            missing_images: 0,
            warnings: Warnings::default(),
            count: 0,
            defs: BTreeMap::new(),
            imported_ids: BTreeMap::new(),
            pending_slots: Vec::new(),
            collection_id: None,
            theme_modes: Vec::new(),
            default_theme: BTreeMap::new(),
            vars: BTreeMap::new(),
        }
    }

    fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.add(msg);
    }

    fn collect_defs(&mut self, v: &Value, depth: usize) {
        if depth > MAX_DEPTH || self.defs.len() >= MAX_NODES {
            return;
        }
        if let Some(id) = str_field(v, "id").filter(|s| !s.is_empty())
            && v.get("type").and_then(Value::as_str) != Some("ref")
        {
            self.defs.entry(id.to_string()).or_insert_with(|| v.clone());
        }
        if let Some(kids) = v.get("children").and_then(Value::as_array) {
            for k in kids {
                self.collect_defs(k, depth + 1);
            }
        }
    }

    fn load_variables_and_themes(&mut self, root: &Value, outer: &Value) {
        let themes_obj = root.get("themes").or_else(|| outer.get("themes")).and_then(Value::as_object);
        let vars_obj = root.get("variables").or_else(|| outer.get("variables")).and_then(Value::as_object);
        if themes_obj.is_none_or(Map::is_empty) && vars_obj.is_none_or(Map::is_empty) {
            return;
        }

        // Extract theme axes and build up to 32 mode combinations.
        let mut axes: Vec<(String, Vec<String>)> = Vec::new();
        if let Some(tmap) = themes_obj {
            for (axis, vals) in tmap.iter().take(8) {
                let list: Vec<String> = vals
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .take(8)
                    .map(|s| clip_chars(s.trim(), 64))
                    .collect();
                if let Some(first) = list.first() {
                    self.default_theme.insert(axis.clone(), first.clone());
                    axes.push((axis.clone(), list));
                }
            }
        }

        let mut combos: Vec<BTreeMap<String, String>> = vec![BTreeMap::new()];
        for (axis, vals) in &axes {
            let mut next = Vec::new();
            for base in &combos {
                for val in vals {
                    if next.len() >= 32 {
                        break;
                    }
                    let mut m = base.clone();
                    m.insert(axis.clone(), val.clone());
                    next.push(m);
                }
            }
            if !next.is_empty() {
                combos = next;
            }
        }

        // Also discover any theme combinations mentioned directly on variable value entries.
        if let Some(vmap) = vars_obj {
            for (_, def) in vmap.iter().take(10_000) {
                if let Some(arr) = def.get("value").and_then(Value::as_array) {
                    for item in arr.iter().take(32) {
                        if let Some(th) = item.get("theme").and_then(Value::as_object) {
                            let mut merged = self.default_theme.clone();
                            for (k, val) in th {
                                if let Some(s) = val.as_str() {
                                    merged.insert(k.clone(), clip_chars(s, 64));
                                    self.default_theme.entry(k.clone()).or_insert_with(|| clip_chars(s, 64));
                                }
                            }
                            if !merged.is_empty() && combos.len() < 32 && !combos.iter().any(|c| c == &merged) {
                                combos.push(merged);
                            }
                        }
                    }
                }
            }
        }

        let mut modes: Vec<Mode> = Vec::new();
        for (idx, combo) in combos.into_iter().enumerate() {
            let name = if combo.is_empty() {
                "Default".to_string()
            } else if combo.len() == 1 {
                combo.values().next().cloned().unwrap_or_else(|| format!("Mode {}", idx + 1))
            } else {
                combo.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join(", ")
            };
            let id = format!("pen-mode-{idx}");
            modes.push(Mode { id: id.clone(), name });
            self.theme_modes.push((id, combo));
        }

        let Some(vmap) = vars_obj.filter(|m| !m.is_empty()) else {
            return;
        };

        let coll_id = "pen-tokens".to_string();
        let coll_name =
            if axes.len() == 1 { axes.first().map(|(a, _)| clip_chars(a, 64)).unwrap_or_else(|| "Tokens".into()) } else { "Tokens".into() };
        self.doc.variables.collections.push(Collection { id: coll_id.clone(), name: coll_name, modes, extends: None });
        self.collection_id = Some(coll_id.clone());

        // First pass: register variable ids, types, and raw themed value lists.
        let mut order: Vec<String> = Vec::new();
        for (raw_key, def) in vmap.iter().take(10_000) {
            let bare = normalize_var_name(raw_key);
            if bare.is_empty() || self.vars.contains_key(&bare) {
                continue;
            }
            let Some(kind) = str_field(def, "type").and_then(|t| match t {
                "color" => Some(VariableType::Color),
                "number" => Some(VariableType::Number),
                "string" => Some(VariableType::String),
                "boolean" => Some(VariableType::Boolean),
                _ => None,
            }) else {
                continue;
            };
            let mut raw_entries: Vec<(BTreeMap<String, String>, Value)> = Vec::new();
            match def.get("value") {
                Some(Value::Array(arr)) => {
                    for entry in arr.iter().take(64) {
                        if let Some(val) = entry.get("value") {
                            let mut th = BTreeMap::new();
                            if let Some(tobj) = entry.get("theme").and_then(Value::as_object) {
                                for (k, tv) in tobj {
                                    if let Some(s) = tv.as_str() {
                                        th.insert(k.clone(), s.to_string());
                                    }
                                }
                            }
                            raw_entries.push((th, val.clone()));
                        }
                    }
                }
                Some(single) => {
                    raw_entries.push((BTreeMap::new(), single.clone()));
                }
                None => continue,
            }
            if raw_entries.is_empty() {
                continue;
            }
            let id = format!("pen-var-{}", self.vars.len() + 1);
            self.vars.insert(bare.clone(), VarEntry { id, kind, raw_entries });
            order.push(bare);
        }

        // Second pass: populate `figcraft_doc::Variable` entries per mode.
        let theme_modes = self.theme_modes.clone();
        for bare in order {
            let Some(entry) = self.vars.get(&bare).cloned() else { continue };
            let mut values = BTreeMap::new();
            for (mode_id, mode_theme) in &theme_modes {
                if let Some(raw_val) = pick_themed_raw(&entry.raw_entries, mode_theme)
                    && let Some(vv) = self.to_variable_value(entry.kind, raw_val, mode_theme)
                {
                    values.insert(mode_id.clone(), vv);
                }
            }
            if values.is_empty() {
                values.insert(
                    theme_modes.first().map(|(m, _)| m.clone()).unwrap_or_else(|| "pen-mode-0".into()),
                    VariableValue::default_for(entry.kind),
                );
            }
            // In FigCraft, variable names use `/` for groups and must not be empty.
            let clean_name = clip_chars(bare.trim_start_matches('-'), 128);
            let display_name = if clean_name.is_empty() { bare.clone() } else { clean_name };
            let mut code_syntax = BTreeMap::new();
            let css_name = if bare.starts_with("--") { format!("var({bare})") } else { format!("var(--{bare})") };
            code_syntax.insert("WEB".into(), css_name);
            self.doc.variables.variables.push(Variable {
                id: entry.id,
                name: display_name,
                collection: coll_id.clone(),
                kind: entry.kind,
                values,
                description: String::new(),
                scopes: None,
                hidden_from_publishing: false,
                extends: None,
                overrides: BTreeSet::new(),
                code_syntax,
            });
        }
    }

    fn to_variable_value(&self, kind: VariableType, raw: &Value, theme: &BTreeMap<String, String>) -> Option<VariableValue> {
        if let Some(var_ref) = raw.as_str().and_then(|s| s.strip_prefix('$')) {
            let target = normalize_var_name(var_ref);
            if let Some(other) = self.vars.get(&target)
                && other.kind == kind
            {
                return Some(VariableValue::Alias(other.id.clone()));
            }
            // Resolve concrete fallback if types differ or alias chain is needed.
            return self.resolve_var_concrete(&target, kind, theme, 0);
        }
        parse_literal_var_value(kind, raw)
    }

    fn resolve_var_concrete(&self, bare: &str, expected: VariableType, theme: &BTreeMap<String, String>, depth: usize) -> Option<VariableValue> {
        if depth > 16 {
            return None;
        }
        let entry = self.vars.get(bare)?;
        if entry.kind != expected {
            return None;
        }
        let raw = pick_themed_raw(&entry.raw_entries, theme)?;
        if let Some(next_ref) = raw.as_str().and_then(|s| s.strip_prefix('$')) {
            let next = normalize_var_name(next_ref);
            return self.resolve_var_concrete(&next, expected, theme, depth + 1);
        }
        parse_literal_var_value(expected, raw)
    }

    fn merged_theme(&self, j: &Value, inherited: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut out = inherited.clone();
        if let Some(th) = j.get("theme").and_then(Value::as_object) {
            for (k, v) in th {
                if let Some(s) = v.as_str() {
                    out.insert(k.clone(), s.to_string());
                }
            }
        }
        out
    }

    fn mode_for_theme(&self, theme: &BTreeMap<String, String>) -> Option<(String, String)> {
        let coll = self.collection_id.as_ref()?;
        let (mode_id, _) = self.theme_modes.iter().filter(|(_, m)| m.iter().all(|(k, v)| theme.get(k) == Some(v))).max_by_key(|(_, m)| m.len())?;
        Some((coll.clone(), mode_id.clone()))
    }

    /// Resolve a number or `$variable` in `theme`, optionally binding `field` on `node`.
    fn resolve_num(&mut self, v: Option<&Value>, theme: &BTreeMap<String, String>, mut bind: Option<(&mut Node, &str)>) -> Option<f64> {
        let v = v?;
        if let Some(n) = v.as_f64().filter(|x| x.is_finite()) {
            return Some(n);
        }
        let raw_str = v.as_str()?.trim();
        if let Some(var_ref) = raw_str.strip_prefix('$') {
            let bare = normalize_var_name(var_ref);
            if let Some(entry_id) = self.vars.get(&bare).filter(|e| e.kind == VariableType::Number).map(|e| e.id.clone()) {
                if let Some((node, field)) = bind.as_mut() {
                    node.bound.insert((*field).to_string(), entry_id);
                }
                if let Some(VariableValue::Number(n)) = self.resolve_var_concrete(&bare, VariableType::Number, theme, 0) {
                    return Some(n);
                }
            } else {
                if let Some((node, field)) = bind.as_mut() {
                    node.plugin_data.insert(format!("pen:var:{field}"), bare.clone());
                }
                self.warn(format!("unresolved number variable `${bare}`"));
            }
            return None;
        }
        raw_str.parse::<f64>().ok().filter(|x| x.is_finite())
    }

    /// Resolve a boolean or `$variable` in `theme`, optionally binding `field` on `node`.
    fn resolve_bool(&mut self, v: Option<&Value>, theme: &BTreeMap<String, String>, mut bind: Option<(&mut Node, &str)>) -> Option<bool> {
        let v = v?;
        if let Some(b) = v.as_bool() {
            return Some(b);
        }
        let raw_str = v.as_str()?.trim();
        if let Some(var_ref) = raw_str.strip_prefix('$') {
            let bare = normalize_var_name(var_ref);
            if let Some(entry_id) = self.vars.get(&bare).filter(|e| e.kind == VariableType::Boolean).map(|e| e.id.clone()) {
                if let Some((node, field)) = bind.as_mut() {
                    node.bound.insert((*field).to_string(), entry_id);
                }
                if let Some(VariableValue::Boolean(b)) = self.resolve_var_concrete(&bare, VariableType::Boolean, theme, 0) {
                    return Some(b);
                }
            } else {
                if let Some((node, field)) = bind.as_mut() {
                    node.plugin_data.insert(format!("pen:var:{field}"), bare.clone());
                }
                self.warn(format!("unresolved boolean variable `${bare}`"));
            }
        }
        None
    }

    /// Resolve a string or `$variable` in `theme`, optionally binding `field` on `node`.
    fn resolve_string(&mut self, v: Option<&Value>, theme: &BTreeMap<String, String>, mut bind: Option<(&mut Node, &str)>) -> Option<String> {
        let v = v?;
        if let Some(n) = v.as_f64().filter(|x| x.is_finite()) {
            return Some(if n.fract() == 0.0 { format!("{}", n as i64) } else { format!("{n}") });
        }
        let s = v.as_str()?;
        if let Some(var_ref) = s.trim().strip_prefix('$') {
            let bare = normalize_var_name(var_ref);
            if let Some(entry_id) = self.vars.get(&bare).filter(|e| e.kind == VariableType::String).map(|e| e.id.clone()) {
                if let Some((node, field)) = bind.as_mut() {
                    node.bound.insert((*field).to_string(), entry_id);
                }
                if let Some(VariableValue::String(resolved)) = self.resolve_var_concrete(&bare, VariableType::String, theme, 0) {
                    return Some(resolved);
                }
            } else if let Some(VariableValue::Number(num_val)) = self.resolve_var_concrete(&bare, VariableType::Number, theme, 0) {
                return Some(format!("{num_val}"));
            } else {
                if let Some((node, field)) = bind.as_mut() {
                    node.plugin_data.insert(format!("pen:var:{field}"), bare.clone());
                }
                self.warn(format!("unresolved string variable `${bare}`"));
                // Keep readable placeholder text rather than blanking out headings/labels.
                return Some(bare);
            }
        }
        Some(s.to_string())
    }

    /// Resolve a colour or `$variable` in `theme`. Returns `(Color, Option<VariableId>, Option<unresolved_bare_name>)`.
    fn resolve_color(
        &mut self,
        v: Option<&Value>,
        theme: &BTreeMap<String, String>,
        fallback_unresolved: Option<Color>,
    ) -> Option<(Color, Option<String>, Option<String>)> {
        let v = v?;
        if let Some(s) = v.as_str() {
            let trimmed = s.trim();
            if let Some(var_ref) = trimmed.strip_prefix('$') {
                let bare = normalize_var_name(var_ref);
                if let Some(entry_id) = self.vars.get(&bare).filter(|e| e.kind == VariableType::Color).map(|e| e.id.clone())
                    && let Some(VariableValue::Color(c)) = self.resolve_var_concrete(&bare, VariableType::Color, theme, 0)
                {
                    return Some((c, Some(entry_id), None));
                }
                self.warn(format!("unresolved color variable `${bare}`"));
                return fallback_unresolved.map(|c| (c, None, Some(bare)));
            }
            return Color::parse_css(trimmed).map(|c| (c, None, None));
        }
        serde_json::from_value::<Color>(v.clone()).ok().map(|c| (c, None, None))
    }

    fn resolve_slot_preferences(&mut self) {
        let pending = std::mem::take(&mut self.pending_slots);
        for (comp_id, prop_name, pen_ids) in pending {
            let preferred: Vec<NodeId> = pen_ids
                .iter()
                .filter_map(|pid| self.imported_ids.get(pid).copied())
                .filter(|nid| self.doc.node(*nid).is_some_and(|n| matches!(n.kind, NodeKind::Component { .. })))
                .collect();
            if preferred.is_empty() {
                continue;
            }
            if let Some(NodeKind::Component { props, .. }) = self.doc.node_mut(comp_id).map(|n| &mut n.kind)
                && let Some(PropDef::Slot { preferred: slot_pref }) = props.get_mut(&prop_name)
            {
                *slot_pref = preferred;
            }
        }
    }

    /// Expand a `type: "ref"` node into a concrete `.pen` node JSON tree, applying root property
    /// overrides and `/`-separated `descendants` overrides or subtree replacements.
    fn expand_ref(&mut self, j: &Value, depth: usize, ref_stack: &mut BTreeSet<String>) -> Option<Value> {
        if depth > MAX_REF_DEPTH {
            self.warn("deeply nested component `ref` chains were truncated");
            return None;
        }
        let target_id = str_field(j, "ref")?.trim().to_string();
        if target_id.is_empty() {
            return None;
        }
        if !ref_stack.insert(target_id.clone()) {
            self.warn(format!("cyclic component `ref` `{target_id}` was skipped"));
            return None;
        }
        let Some(mut base) = self.defs.get(&target_id).cloned() else {
            ref_stack.remove(&target_id);
            self.warn(format!("missing referenced component `{target_id}`"));
            // Stand-in frame so layout slots/positions stay intact.
            let mut placeholder = serde_json::json!({
                "type": "frame",
                "name": str_field(j, "name").unwrap_or(&format!("Ref {target_id}")),
                "layout": "horizontal"
            });
            if let (Some(dst), Some(src)) = (placeholder.as_object_mut(), j.as_object()) {
                for (k, val) in src {
                    if !matches!(k.as_str(), "type" | "ref" | "descendants") {
                        dst.insert(k.clone(), val.clone());
                    }
                }
            }
            return Some(placeholder);
        };

        // If `base` itself is a `ref`, resolve it first.
        if base.get("type").and_then(Value::as_str) == Some("ref") {
            let Some(resolved) = self.expand_ref(&base, depth + 1, ref_stack) else {
                ref_stack.remove(&target_id);
                return None;
            };
            base = resolved;
        }

        if let Some(base_obj) = base.as_object_mut() {
            base_obj.remove("reusable");
            if let Some(src_obj) = j.as_object() {
                for (k, val) in src_obj {
                    if !matches!(k.as_str(), "type" | "ref" | "descendants") {
                        base_obj.insert(k.clone(), val.clone());
                    }
                }
            }
        }

        if let Some(desc) = j.get("descendants").and_then(Value::as_object) {
            for (id_path, patch) in desc {
                let segs: Vec<&str> = id_path.split('/').filter(|s| !s.is_empty()).collect();
                if !segs.is_empty() {
                    self.apply_descendant_patch(&mut base, &segs, patch, depth + 1, ref_stack);
                }
            }
        }

        ref_stack.remove(&target_id);
        Some(base)
    }

    fn apply_descendant_patch(&mut self, node: &mut Value, segs: &[&str], patch: &Value, depth: usize, ref_stack: &mut BTreeSet<String>) {
        let Some((&head, tail)) = segs.split_first() else { return };
        let Some(children) = node.get_mut("children").and_then(Value::as_array_mut) else { return };
        let Some(child) = children.iter_mut().find(|c| str_field(c, "id") == Some(head)) else { return };

        if tail.is_empty() {
            if patch.get("type").is_some() {
                *child = patch.clone();
            } else if let (Some(dst), Some(src)) = (child.as_object_mut(), patch.as_object()) {
                for (k, val) in src {
                    dst.insert(k.clone(), val.clone());
                }
            }
            return;
        }

        // Expand nested `ref` on the fly so descendant paths through nested components resolve.
        if child.get("type").and_then(Value::as_str) == Some("ref")
            && let Some(expanded) = self.expand_ref(child, depth, ref_stack)
        {
            *child = expanded;
        }
        self.apply_descendant_patch(child, tail, patch, depth + 1, ref_stack);
    }

    #[allow(clippy::too_many_arguments)]
    fn build_node(
        &mut self,
        raw_j: &Value,
        parent: NodeId,
        parent_mode: Option<LayoutMode>,
        in_component: bool,
        inherited_theme: &BTreeMap<String, String>,
        depth: usize,
        ref_stack: &mut BTreeSet<String>,
    ) -> Option<NodeId> {
        if depth > MAX_DEPTH {
            self.warn(format!("layers nested deeper than {MAX_DEPTH} levels were skipped"));
            return None;
        }
        if self.count >= MAX_NODES {
            self.warn(format!("the payload has more than {MAX_NODES} layers; the rest were skipped"));
            return None;
        }

        let expanded;
        let j = if str_field(raw_j, "type") == Some("ref") {
            expanded = self.expand_ref(raw_j, 0, ref_stack)?;
            &expanded
        } else {
            raw_j
        };

        let Some(ty) = str_field(j, "type") else {
            self.warn("layers without a `type` were skipped");
            return None;
        };

        let theme = self.merged_theme(j, inherited_theme);
        match ty {
            "frame" => self.build_frame(j, parent, parent_mode, in_component, &theme, depth, ref_stack),
            "group" => self.build_group(j, parent, parent_mode, in_component, &theme, depth, ref_stack),
            "rectangle" => self.build_rectangle(j, parent, parent_mode, &theme),
            "ellipse" => self.build_ellipse(j, parent, parent_mode, &theme),
            "polygon" => self.build_polygon(j, parent, parent_mode, &theme),
            "path" => self.build_path(j, parent, parent_mode, &theme),
            "line" => self.build_line(j, parent, parent_mode, &theme),
            "text" => self.build_text(j, parent, parent_mode, &theme),
            "note" => self.build_note(j, parent, parent_mode, &theme),
            "icon" | "icon_font" => self.build_icon(j, parent, parent_mode, &theme),
            "browser" => self.build_browser(j, parent, parent_mode, &theme),
            "script" => self.build_script(j, parent, parent_mode, &theme),
            other => {
                self.warn(format!("unsupported .pen layer type `{}` was skipped", clip_chars(other, 48)));
                None
            }
        }
    }

    fn apply_entity_common(&mut self, node: &mut Node, j: &Value, parent_mode: Option<LayoutMode>, theme: &BTreeMap<String, String>) {
        if j.get("theme").is_some_and(Value::is_object)
            && let Some((coll, mode)) = self.mode_for_theme(theme)
        {
            node.var_modes.insert(coll, mode);
        }
        if let Some(enabled) = self.resolve_bool(j.get("enabled"), theme, Some((node, "visible"))) {
            node.visible = enabled;
        }
        if let Some(op) = self.resolve_num(j.get("opacity"), theme, Some((node, "opacity"))) {
            node.opacity = (op as f32).clamp(0.0, 1.0);
        }
        node.child_layout.absolute = str_field(j, "layoutPosition") == Some("absolute");

        let in_flex = parent_mode.is_some() && !node.child_layout.absolute;
        let x = if in_flex { 0.0 } else { clamp_coord(num_lit(j, "x").unwrap_or(0.0)) };
        let y = if in_flex { 0.0 } else { clamp_coord(num_lit(j, "y").unwrap_or(0.0)) };
        let rot_deg = self.resolve_num(j.get("rotation"), theme, None).unwrap_or(0.0);
        let flip_x = self.resolve_bool(j.get("flipX"), theme, None).unwrap_or(false);
        let flip_y = self.resolve_bool(j.get("flipY"), theme, None).unwrap_or(false);
        node.transform = compose_transform(x, y, node.width, node.height, rot_deg, flip_x, flip_y);

        if let Some(ctx_note) = str_field(j, "context").filter(|s| !s.trim().is_empty()) {
            node.plugin_data.insert("pen:context".into(), clip_chars(ctx_note, 2000));
        }
    }

    fn parse_axis_size(
        &mut self,
        v: Option<&Value>,
        field: &str,
        node: &mut Node,
        theme: &BTreeMap<String, String>,
        default: ParsedSize,
    ) -> ParsedSize {
        let Some(v) = v else { return default };
        if let Some(s) = v.as_str() {
            let trimmed = s.trim();
            if let Some(rest) = trimmed.strip_prefix("fit_content") {
                return ParsedSize::Hug(parse_paren_fallback(rest));
            }
            if let Some(rest) = trimmed.strip_prefix("fill_container") {
                return ParsedSize::Fill(parse_paren_fallback(rest));
            }
        }
        match self.resolve_num(Some(v), theme, Some((node, field))) {
            Some(px) => ParsedSize::Fixed(clamp_size(px)),
            None => default,
        }
    }

    fn apply_corners(&mut self, node: &mut Node, j: &Value, theme: &BTreeMap<String, String>) {
        let Some(cr) = j.get("cornerRadius") else { return };
        if let Some(arr) = cr.as_array() {
            let fields = ["topLeftRadius", "topRightRadius", "bottomRightRadius", "bottomLeftRadius"];
            let mut radii = [0.0; 4];
            for i in 0..4 {
                if let Some(val) = arr.get(i)
                    && let Some(r) = self.resolve_num(Some(val), theme, Some((node, fields[i])))
                {
                    radii[i] = clamp_size(r);
                }
            }
            node.corners = Corners { radii, smoothing: 0.0 };
        } else if let Some(r) = self.resolve_num(Some(cr), theme, Some((node, "cornerRadius"))) {
            let r = clamp_size(r);
            node.corners = Corners { radii: [r; 4], smoothing: 0.0 };
        }
    }

    fn apply_graphics(&mut self, node: &mut Node, j: &Value, theme: &BTreeMap<String, String>, fallback_fill: Option<Color>) {
        node.fills.clear();
        if let Some(fill_val) = j.get("fill") {
            let paints = self.parse_paints(fill_val, node.width, node.height, "fill", "fills", node, theme, fallback_fill);
            node.fills = paints;
        }

        node.strokes.clear();
        if let Some(stroke_val) = j.get("stroke") {
            // Support both `.pen` v2.22 (`stroke: Fills` + `strokeWidth`) and legacy (`stroke: { fill, thickness, align }`).
            let is_legacy_obj = stroke_val.is_object()
                && stroke_val.get("type").is_none()
                && (stroke_val.get("fill").is_some() || stroke_val.get("thickness").is_some());
            if is_legacy_obj {
                if let Some(inner_fill) = stroke_val.get("fill") {
                    node.strokes = self.parse_paints(inner_fill, node.width, node.height, "stroke", "strokes", node, theme, None);
                }
                self.apply_stroke_width(node, stroke_val.get("thickness"), theme);
                if let Some(align) = str_field(stroke_val, "align") {
                    node.stroke.align = parse_stroke_align(align, node.stroke.align);
                }
            } else {
                node.strokes = self.parse_paints(stroke_val, node.width, node.height, "stroke", "strokes", node, theme, None);
                self.apply_stroke_width(node, j.get("strokeWidth"), theme);
            }
        } else if j.get("strokeWidth").is_some() {
            self.apply_stroke_width(node, j.get("strokeWidth"), theme);
        }

        if let Some(align) = str_field(j, "strokeAlignment") {
            node.stroke.align = parse_stroke_align(align, node.stroke.align);
        }
        if let Some(cap) = str_field(j, "strokeLinecap") {
            let sc = match cap {
                "round" => StrokeCap::Round,
                "square" => StrokeCap::Square,
                _ => StrokeCap::None,
            };
            node.stroke.cap_start = sc;
            node.stroke.cap_end = sc;
        }
        if let Some(join) = str_field(j, "strokeLinejoin") {
            node.stroke.join = match join {
                "bevel" => StrokeJoin::Bevel,
                "round" => StrokeJoin::Round,
                _ => StrokeJoin::Miter,
            };
        }

        if let Some(eff_val) = j.get("effect") {
            node.effects = self.parse_effects(eff_val, theme);
        }
    }

    fn apply_stroke_width(&mut self, node: &mut Node, sw: Option<&Value>, theme: &BTreeMap<String, String>) {
        let Some(sw) = sw else {
            if !node.strokes.is_empty() {
                node.stroke.weight = 1.0;
            }
            return;
        };
        if let Some(obj) = sw.as_object() {
            let top = self.resolve_num(obj.get("top"), theme, Some((node, "strokeTopWeight"))).map(clamp_size).unwrap_or(0.0);
            let right = self.resolve_num(obj.get("right"), theme, Some((node, "strokeRightWeight"))).map(clamp_size).unwrap_or(0.0);
            let bottom = self.resolve_num(obj.get("bottom"), theme, Some((node, "strokeBottomWeight"))).map(clamp_size).unwrap_or(0.0);
            let left = self.resolve_num(obj.get("left"), theme, Some((node, "strokeLeftWeight"))).map(clamp_size).unwrap_or(0.0);
            let max_w = top.max(right).max(bottom).max(left);
            node.stroke.weight = max_w;
            node.stroke.sides = Some([top, right, bottom, left]);
        } else if let Some(w) = self.resolve_num(Some(sw), theme, Some((node, "strokeWeight"))) {
            node.stroke.weight = clamp_size(w);
            node.stroke.sides = None;
            if node.stroke.weight <= 0.0 {
                node.strokes.clear();
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn parse_paints(
        &mut self,
        v: &Value,
        w: f64,
        h: f64,
        single_field: &str,
        list_prefix: &str,
        node: &mut Node,
        theme: &BTreeMap<String, String>,
        fallback_color: Option<Color>,
    ) -> Vec<Paint> {
        let items: Vec<&Value> = match v {
            Value::Array(arr) => arr.iter().take(32).collect(),
            Value::Null => return Vec::new(),
            single => vec![single],
        };
        let total = items.len();
        let mut out = Vec::new();
        for item in items {
            let idx = out.len();
            let bind_key = if total == 1 && idx == 0 { single_field.to_string() } else { format!("{list_prefix}/{idx}/color") };
            if let Some(p) = self.parse_one_paint(item, w, h, &bind_key, list_prefix, idx, node, theme, fallback_color) {
                out.push(p);
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn parse_one_paint(
        &mut self,
        item: &Value,
        w: f64,
        h: f64,
        bind_key: &str,
        list_prefix: &str,
        paint_idx: usize,
        node: &mut Node,
        theme: &BTreeMap<String, String>,
        fallback_color: Option<Color>,
    ) -> Option<Paint> {
        if item.is_string() {
            let (c, var_id, unresolved) = self.resolve_color(Some(item), theme, fallback_color)?;
            if let Some(vid) = var_id {
                node.bound.insert(bind_key.to_string(), vid);
            } else if let Some(bare) = unresolved {
                node.plugin_data.insert(format!("pen:var:{bind_key}"), bare);
            }
            return Some(Paint::solid(c));
        }

        let obj = item.as_object()?;
        let visible = self.resolve_bool(obj.get("enabled"), theme, None).unwrap_or(true);
        let blend = str_field(item, "blendMode").map_or(BlendMode::Normal, parse_blend_mode);
        let opacity = self.resolve_num(obj.get("opacity"), theme, None).map(|o| (o as f32).clamp(0.0, 1.0)).unwrap_or(1.0);

        match str_field(item, "type").unwrap_or("color") {
            "color" => {
                let (c, var_id, unresolved) = self.resolve_color(obj.get("color"), theme, fallback_color)?;
                if let Some(vid) = var_id {
                    node.bound.insert(bind_key.to_string(), vid);
                } else if let Some(bare) = unresolved {
                    node.plugin_data.insert(format!("pen:var:{bind_key}"), bare);
                }
                let mut p = Paint::solid(c);
                p.opacity = (p.opacity * opacity).clamp(0.0, 1.0);
                p.visible = visible;
                p.blend = blend;
                Some(p)
            }
            "gradient" => {
                let kind = match str_field(item, "gradientType") {
                    Some("radial") => GradientKind::Radial,
                    Some("angular") => GradientKind::Angular,
                    _ => GradientKind::Linear,
                };
                let cx = obj.get("center").and_then(|c| num_lit(c, "x")).unwrap_or(0.5).clamp(-10.0, 10.0);
                let cy = obj.get("center").and_then(|c| num_lit(c, "y")).unwrap_or(0.5).clamp(-10.0, 10.0);
                let sw = obj.get("size").and_then(|s| self.resolve_num(s.get("width"), theme, None)).unwrap_or(1.0).clamp(1e-4, 20.0);
                let sh = obj.get("size").and_then(|s| self.resolve_num(s.get("height"), theme, None)).unwrap_or(1.0).clamp(1e-4, 20.0);
                // Pen rotation: degrees CCW, 0° = up (0, -1), 90° = left (-1, 0), 180° = down (0, +1).
                let rot_deg = self.resolve_num(obj.get("rotation"), theme, None).unwrap_or(0.0);
                let rad = rot_deg.to_radians();
                let (dx, dy) = (-rad.sin(), -rad.cos());

                let mut stops = Vec::new();
                if let Some(cols) = obj.get("colors").and_then(Value::as_array) {
                    for (si, stop) in cols.iter().take(64).enumerate() {
                        let pos = self.resolve_num(stop.get("position"), theme, None).unwrap_or(0.0).clamp(0.0, 1.0) as f32;
                        if let Some((col, vid, _)) = self.resolve_color(stop.get("color"), theme, Some(Color::BLACK)) {
                            if let Some(vid) = vid {
                                node.bound.insert(format!("{list_prefix}/{paint_idx}/stops/{si}/color"), vid);
                            }
                            stops.push(ColorStop { position: pos, color: col });
                        }
                    }
                }
                if stops.is_empty() {
                    stops.push(ColorStop { position: 0.0, color: Color::BLACK });
                    stops.push(ColorStop { position: 1.0, color: Color::WHITE });
                }

                let mut grad = match kind {
                    GradientKind::Linear => {
                        let half = 0.5 * sh;
                        let mut g =
                            Gradient { kind, stops, from: (cx - half * dx, cy - half * dy), to: (cx + half * dx, cy + half * dy), width: (cx, cy) };
                        g.square_linear_width(w.max(1.0), h.max(1.0));
                        g
                    }
                    _ => {
                        let rh = 0.5 * sh;
                        let rw = 0.5 * sw;
                        let (px, py) = (-dy, dx);
                        Gradient { kind, stops, from: (cx, cy), to: (cx + rh * dx, cy + rh * dy), width: (cx + rw * px, cy + rw * py) }
                    }
                };
                grad.stops = grad.sorted_stops();
                Some(Paint { kind: PaintKind::Gradient(grad), opacity, visible, blend })
            }
            "image" => {
                let url = str_field(item, "url").unwrap_or("");
                let hash = self.resolve_image_url(url);
                let mut scale_mode = match str_field(item, "mode") {
                    Some("contain") => ImageScaleMode::Fit,
                    _ => ImageScaleMode::Fill,
                };
                let mut crop = None;
                if str_field(item, "mode") == Some("stretch") {
                    scale_mode = ImageScaleMode::Crop;
                    crop = Some([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
                }
                if let Some(t) = parse_affine6(obj.get("transform"))
                    && t != [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]
                {
                    let inv = Affine::new(t).inverse().as_coeffs();
                    if inv.iter().all(|x| x.is_finite()) && (inv[0].abs() > 1e-9 || inv[1].abs() > 1e-9) {
                        scale_mode = ImageScaleMode::Crop;
                        crop = Some(inv);
                    }
                }
                Some(Paint {
                    kind: PaintKind::Image(ImagePaint {
                        image: hash,
                        scale_mode,
                        scale: 1.0,
                        rotation: 0,
                        crop,
                        filters: Default::default(),
                        video: None,
                    }),
                    opacity,
                    visible,
                    blend,
                })
            }
            "mesh_gradient" => {
                self.warn("mesh gradients were approximated as linear gradients");
                let mut stops = Vec::new();
                if let Some(cols) = obj.get("colors").and_then(Value::as_array) {
                    let denom = cols.len().saturating_sub(1).max(1) as f32;
                    for (i, cval) in cols.iter().take(16).enumerate() {
                        if let Some((c, _, _)) = self.resolve_color(Some(cval), theme, None) {
                            stops.push(ColorStop { position: (i as f32 / denom).clamp(0.0, 1.0), color: c });
                        }
                    }
                }
                if stops.is_empty() {
                    return None;
                }
                let mut g = Gradient { kind: GradientKind::Linear, stops, from: (0.0, 0.0), to: (1.0, 1.0), width: (0.0, 1.0) };
                g.square_linear_width(w.max(1.0), h.max(1.0));
                Some(Paint { kind: PaintKind::Gradient(g), opacity, visible, blend })
            }
            "shader" => {
                // Check if any uniform supplies a fallback colour or image URL.
                if let Some(uniforms) = obj.get("uniforms").and_then(Value::as_object) {
                    for (_, uval) in uniforms {
                        if let Some(s) = uval.as_str()
                            && (s.starts_with("data:image/") || s.starts_with("file://"))
                        {
                            let hash = self.resolve_image_url(s);
                            self.warn("shader fills were approximated using their texture image");
                            return Some(Paint {
                                kind: PaintKind::Image(ImagePaint {
                                    image: hash,
                                    scale_mode: ImageScaleMode::Fill,
                                    scale: 1.0,
                                    rotation: 0,
                                    crop: None,
                                    filters: Default::default(),
                                    video: None,
                                }),
                                opacity,
                                visible,
                                blend,
                            });
                        }
                    }
                    for (_, uval) in uniforms {
                        if uval.as_str().is_some_and(|s| s.starts_with('#') || s.starts_with('$'))
                            && let Some((c, _, _)) = self.resolve_color(Some(uval), theme, None)
                        {
                            self.warn("WebGL shader fills were approximated with a solid colour");
                            let mut p = Paint::solid(c);
                            p.opacity = (p.opacity * opacity).clamp(0.0, 1.0);
                            p.visible = visible;
                            p.blend = blend;
                            return Some(p);
                        }
                    }
                }
                self.warn("WebGL shader fills are not supported and were skipped");
                None
            }
            other => {
                self.warn(format!("unknown fill type `{}` was skipped", clip_chars(other, 32)));
                None
            }
        }
    }

    fn resolve_image_url(&mut self, url: &str) -> String {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            return MISSING_IMAGE.to_string();
        }
        if let Some(existing) = self.image_keys.get(trimmed) {
            return existing.clone();
        }
        if let Some(bytes) = decode_data_image(trimmed).or_else(|| (self.images)(trimmed)) {
            let hash = crate::image_hash(&bytes);
            if !self.doc.images.contains_key(&hash) && self.doc.images.len() < 20_000 {
                self.doc.images.insert(hash.clone(), crate::image_data(bytes));
            }
            self.image_keys.insert(trimmed.to_string(), hash.clone());
            return hash;
        }
        self.missing_images = self.missing_images.saturating_add(1);
        self.warn("some image fills could not be loaded and show placeholders");
        let placeholder = format!("pen-image-missing-{}", self.missing_images);
        self.image_keys.insert(trimmed.to_string(), placeholder.clone());
        placeholder
    }

    fn parse_effects(&mut self, v: &Value, theme: &BTreeMap<String, String>) -> Vec<Effect> {
        let items: Vec<&Value> = match v {
            Value::Array(arr) => arr.iter().take(32).collect(),
            Value::Null => return Vec::new(),
            single => vec![single],
        };
        let mut out = Vec::new();
        for item in items {
            let Some(obj) = item.as_object() else { continue };
            let visible = self.resolve_bool(obj.get("enabled"), theme, None).unwrap_or(true);
            match str_field(item, "type") {
                Some("blur") => {
                    let radius = self.resolve_num(obj.get("radius"), theme, None).unwrap_or(4.0).clamp(0.0, 10_000.0);
                    out.push(Effect { kind: EffectKind::LayerBlur(Blur::uniform(radius)), visible });
                }
                Some("background_blur") => {
                    let radius = self.resolve_num(obj.get("radius"), theme, None).unwrap_or(8.0).clamp(0.0, 10_000.0);
                    out.push(Effect { kind: EffectKind::BackgroundBlur(Blur::uniform(radius)), visible });
                }
                Some("shadow") => {
                    let ox = obj.get("offset").and_then(|o| self.resolve_num(o.get("x"), theme, None)).unwrap_or(0.0).clamp(-10_000.0, 10_000.0);
                    let oy = obj.get("offset").and_then(|o| self.resolve_num(o.get("y"), theme, None)).unwrap_or(4.0).clamp(-10_000.0, 10_000.0);
                    let radius = self.resolve_num(obj.get("blur"), theme, None).unwrap_or(4.0).clamp(0.0, 10_000.0);
                    let spread = self.resolve_num(obj.get("spread"), theme, None).unwrap_or(0.0).clamp(-10_000.0, 10_000.0);
                    let color = self
                        .resolve_color(obj.get("color"), theme, Some(Color::new(0.0, 0.0, 0.0, 0.25)))
                        .map(|(c, _, _)| c)
                        .unwrap_or_else(|| Color::new(0.0, 0.0, 0.0, 0.25));
                    let blend = str_field(item, "blendMode").map_or(BlendMode::Normal, parse_blend_mode);
                    let shadow = Shadow { color, offset: (ox, oy), radius, spread, blend, show_behind: false };
                    let kind = match str_field(item, "shadowType") {
                        Some("inner") => EffectKind::InnerShadow(shadow),
                        _ => EffectKind::DropShadow(shadow),
                    };
                    out.push(Effect { kind, visible });
                }
                Some("shader") => {
                    self.warn("WebGL shader effects are not supported and were skipped");
                }
                _ => {}
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn build_frame(
        &mut self,
        j: &Value,
        parent: NodeId,
        parent_mode: Option<LayoutMode>,
        in_component: bool,
        theme: &BTreeMap<String, String>,
        depth: usize,
        ref_stack: &mut BTreeSet<String>,
    ) -> Option<NodeId> {
        let is_reusable = !in_component && j.get("reusable").and_then(Value::as_bool).unwrap_or(false);
        let kind = if is_reusable { NodeKind::Component { description: String::new(), props: BTreeMap::new() } } else { NodeKind::Frame };
        let default_label = if is_reusable { "Component" } else { "Frame" };
        let name = str_field(j, "name").filter(|s| !s.is_empty()).map_or_else(|| default_label.to_string(), |s| clip_chars(s, MAX_NAME_CHARS));
        let id = self.doc.alloc_id();
        let mut node = Node::new(id, kind, name, 100.0, 100.0);
        // `.pen` frames default to transparent fills and `clip: false`.
        node.fills.clear();
        node.clip = self.resolve_bool(j.get("clip"), theme, None).unwrap_or(false);

        let layout_dir = match str_field(j, "layout") {
            Some("none") => None,
            Some("vertical") => Some(LayoutMode::Vertical),
            _ => Some(LayoutMode::Horizontal),
        };

        if let Some(mode) = layout_dir {
            let gap = self.resolve_num(j.get("gap"), theme, Some((&mut node, "gap"))).map(clamp_size).unwrap_or(0.0);
            let padding = self.parse_padding(j.get("padding"), &mut node, theme);
            let main_align = match str_field(j, "justifyContent") {
                Some("center") => MainAlign::Center,
                Some("end" | "flex-end" | "flex_end") => MainAlign::Max,
                Some("space_between" | "space-between") => MainAlign::SpaceBetween,
                Some("space_around" | "space-around") => MainAlign::SpaceAround,
                Some("space_evenly" | "space-evenly") => MainAlign::SpaceEvenly,
                _ => MainAlign::Min,
            };
            let cross_align = match str_field(j, "alignItems") {
                Some("center") => CrossAlign::Center,
                Some("end" | "flex-end" | "flex_end") => CrossAlign::Max,
                _ => CrossAlign::Min,
            };
            let strokes_included = j.get("layoutIncludeStroke").and_then(Value::as_bool).unwrap_or(false);
            node.layout = Some(AutoLayout {
                mode,
                wrap: false,
                gap,
                cross_gap: 0.0,
                padding,
                main_align,
                cross_align,
                strokes_included,
                ..AutoLayout::default()
            });
        }

        let in_flex = parent_mode.is_some() && str_field(j, "layoutPosition") != Some("absolute");
        let default_sz = if layout_dir.is_some() { ParsedSize::Hug(None) } else { ParsedSize::Fixed(100.0) };
        let parsed_w = self.parse_axis_size(j.get("width"), "width", &mut node, theme, default_sz);
        let parsed_h = self.parse_axis_size(j.get("height"), "height", &mut node, theme, default_sz);
        let (sizing_h, w_px) = resolve_box_axis(parsed_w, layout_dir.is_some(), in_flex, 100.0);
        let (sizing_v, h_px) = resolve_box_axis(parsed_h, layout_dir.is_some(), in_flex, 100.0);
        node.width = w_px;
        node.height = h_px;
        node.child_layout.sizing_h = sizing_h;
        node.child_layout.sizing_v = sizing_v;

        self.apply_corners(&mut node, j, theme);
        self.apply_graphics(&mut node, j, theme, None);
        self.apply_entity_common(&mut node, j, parent_mode, theme);

        let slot_recs: Option<Vec<String>> = match j.get("slot") {
            Some(Value::Array(arr)) => Some(arr.iter().filter_map(Value::as_str).map(str::to_string).collect()),
            Some(Value::Bool(true)) => Some(Vec::new()),
            _ => None,
        };

        self.doc.insert(node, parent, None).ok()?;
        self.count += 1;
        if let Some(pid) = str_field(j, "id").filter(|s| !s.is_empty()) {
            self.imported_ids.entry(pid.to_string()).or_insert(id);
        }

        // Register slot on enclosing component if `slot` was declared.
        if let Some(recs) = slot_recs
            && let Some(comp_id) = self.enclosing_component(id)
        {
            let slot_name = self.doc.node(id).map(|n| n.name.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| "Slot".into());
            let unique_name = self.register_component_slot(comp_id, &slot_name);
            if let Some(n) = self.doc.node_mut(id) {
                n.prop_refs.insert("slot".into(), unique_name.clone());
            }
            if !recs.is_empty() {
                self.pending_slots.push((comp_id, unique_name, recs));
            }
        }

        let now_in_comp = in_component || is_reusable;
        if let Some(kids) = j.get("children").and_then(Value::as_array) {
            for k in kids {
                self.build_node(k, id, layout_dir, now_in_comp, theme, depth + 1, ref_stack);
            }
        }

        // If `layout: "none"` omitted explicit width/height, wrap children bounds so content isn't 100×100.
        if layout_dir.is_none() && (j.get("width").is_none() || j.get("height").is_none()) {
            let (mut max_x, mut max_y) = (0.0_f64, 0.0_f64);
            if let Some(children) = self.doc.node(id).map(|n| n.children.clone()) {
                for cid in children {
                    if let Some(cn) = self.doc.node(cid) {
                        let b = figcraft_doc::geom::transform_rect_bbox(cn.transform, cn.local_rect());
                        max_x = max_x.max(b.x1);
                        max_y = max_y.max(b.y1);
                    }
                }
            }
            if let Some(n) = self.doc.node_mut(id) {
                if j.get("width").is_none() && max_x > 0.0 {
                    n.width = clamp_size(max_x);
                }
                if j.get("height").is_none() && max_y > 0.0 {
                    n.height = clamp_size(max_y);
                }
            }
        }

        Some(id)
    }

    fn parse_padding(&mut self, v: Option<&Value>, node: &mut Node, theme: &BTreeMap<String, String>) -> [f64; 4] {
        let Some(v) = v else { return [0.0; 4] };
        if let Some(arr) = v.as_array() {
            match arr.as_slice() {
                [vert, horiz] => {
                    let pv = self.resolve_num(Some(vert), theme, Some((node, "paddingVertical"))).map(clamp_size).unwrap_or(0.0);
                    let ph = self.resolve_num(Some(horiz), theme, Some((node, "paddingHorizontal"))).map(clamp_size).unwrap_or(0.0);
                    [pv, ph, pv, ph]
                }
                [t, h, b] => {
                    let pt = self.resolve_num(Some(t), theme, Some((node, "paddingTop"))).map(clamp_size).unwrap_or(0.0);
                    let ph = self.resolve_num(Some(h), theme, Some((node, "paddingHorizontal"))).map(clamp_size).unwrap_or(0.0);
                    let pb = self.resolve_num(Some(b), theme, Some((node, "paddingBottom"))).map(clamp_size).unwrap_or(0.0);
                    [pt, ph, pb, ph]
                }
                [t, r, b, l, ..] => {
                    let pt = self.resolve_num(Some(t), theme, Some((node, "paddingTop"))).map(clamp_size).unwrap_or(0.0);
                    let pr = self.resolve_num(Some(r), theme, Some((node, "paddingRight"))).map(clamp_size).unwrap_or(0.0);
                    let pb = self.resolve_num(Some(b), theme, Some((node, "paddingBottom"))).map(clamp_size).unwrap_or(0.0);
                    let pl = self.resolve_num(Some(l), theme, Some((node, "paddingLeft"))).map(clamp_size).unwrap_or(0.0);
                    [pt, pr, pb, pl]
                }
                [one] => {
                    let p = self.resolve_num(Some(one), theme, None).map(clamp_size).unwrap_or(0.0);
                    [p; 4]
                }
                [] => [0.0; 4],
            }
        } else if let Some(var_ref) = v.as_str().and_then(|s| s.trim().strip_prefix('$')) {
            let bare = normalize_var_name(var_ref);
            let p = self.resolve_num(Some(v), theme, None).map(clamp_size).unwrap_or(0.0);
            if let Some(vid) = self.vars.get(&bare).filter(|e| e.kind == VariableType::Number).map(|e| e.id.clone()) {
                node.bound.insert("paddingHorizontal".into(), vid.clone());
                node.bound.insert("paddingVertical".into(), vid);
            }
            [p; 4]
        } else {
            let p = self.resolve_num(Some(v), theme, None).map(clamp_size).unwrap_or(0.0);
            [p; 4]
        }
    }

    fn enclosing_component(&self, id: NodeId) -> Option<NodeId> {
        self.doc.ancestors(id).into_iter().find(|&anc| self.doc.node(anc).is_some_and(|n| matches!(n.kind, NodeKind::Component { .. })))
    }

    fn register_component_slot(&mut self, comp_id: NodeId, base_name: &str) -> String {
        let Some(NodeKind::Component { props, .. }) = self.doc.node_mut(comp_id).map(|n| &mut n.kind) else {
            return base_name.to_string();
        };
        let mut candidate = base_name.to_string();
        let mut n = 2usize;
        while props.contains_key(&candidate) {
            candidate = format!("{base_name} {n}");
            n += 1;
        }
        props.insert(candidate.clone(), PropDef::Slot { preferred: Vec::new() });
        candidate
    }

    #[allow(clippy::too_many_arguments)]
    fn build_group(
        &mut self,
        j: &Value,
        parent: NodeId,
        parent_mode: Option<LayoutMode>,
        in_component: bool,
        theme: &BTreeMap<String, String>,
        depth: usize,
        ref_stack: &mut BTreeSet<String>,
    ) -> Option<NodeId> {
        let kids = j.get("children").and_then(Value::as_array)?;
        if kids.is_empty() {
            return None;
        }
        let name = str_field(j, "name").filter(|s| !s.is_empty()).map_or_else(|| "Group".into(), |s| clip_chars(s, MAX_NAME_CHARS));
        let id = self.doc.alloc_id();
        let mut node = Node::new(id, NodeKind::Group, name, 0.0, 0.0);
        if let Some(eff_val) = j.get("effect") {
            node.effects = self.parse_effects(eff_val, theme);
        }
        self.apply_entity_common(&mut node, j, parent_mode, theme);
        self.doc.insert(node, parent, None).ok()?;
        self.count += 1;
        if let Some(pid) = str_field(j, "id").filter(|s| !s.is_empty()) {
            self.imported_ids.entry(pid.to_string()).or_insert(id);
        }
        for k in kids {
            self.build_node(k, id, None, in_component, theme, depth + 1, ref_stack);
        }
        let _ = self.doc.fit_to_children(id);
        Some(id)
    }

    fn build_rectangle(&mut self, j: &Value, parent: NodeId, parent_mode: Option<LayoutMode>, theme: &BTreeMap<String, String>) -> Option<NodeId> {
        let name = str_field(j, "name").filter(|s| !s.is_empty()).map_or_else(|| "Rectangle".into(), |s| clip_chars(s, MAX_NAME_CHARS));
        let id = self.doc.alloc_id();
        let mut node = Node::new(id, NodeKind::Rectangle, name, 100.0, 100.0);
        self.apply_leaf_sizing(&mut node, j, parent_mode, theme, (100.0, 100.0));
        self.apply_corners(&mut node, j, theme);
        self.apply_graphics(&mut node, j, theme, Some(Color::from_rgb8_const(0x88, 0x88, 0x88)));
        self.apply_entity_common(&mut node, j, parent_mode, theme);
        self.finish_leaf(node, parent, j)
    }

    fn build_ellipse(&mut self, j: &Value, parent: NodeId, parent_mode: Option<LayoutMode>, theme: &BTreeMap<String, String>) -> Option<NodeId> {
        let name = str_field(j, "name").filter(|s| !s.is_empty()).map_or_else(|| "Ellipse".into(), |s| clip_chars(s, MAX_NAME_CHARS));
        let id = self.doc.alloc_id();
        let mut node = Node::new(id, NodeKind::Ellipse { start: 0.0, sweep: std::f64::consts::TAU, inner: 0.0 }, name, 100.0, 100.0);
        let inner = self.resolve_num(j.get("innerRadius"), theme, None).unwrap_or(0.0).clamp(0.0, 1.0);
        let start_deg = self.resolve_num(j.get("startAngle"), theme, None).unwrap_or(0.0);
        let sweep_deg = self.resolve_num(j.get("sweepAngle"), theme, None).unwrap_or(360.0).clamp(-360.0, 360.0);
        let start = (-start_deg).to_radians();
        let sweep = if sweep_deg.abs() >= 360.0 - 1e-6 { std::f64::consts::TAU } else { (-sweep_deg).to_radians() };
        node.kind = NodeKind::Ellipse { start, sweep, inner };

        self.apply_leaf_sizing(&mut node, j, parent_mode, theme, (100.0, 100.0));
        self.apply_graphics(&mut node, j, theme, Some(Color::from_rgb8_const(0x88, 0x88, 0x88)));
        self.apply_entity_common(&mut node, j, parent_mode, theme);
        self.finish_leaf(node, parent, j)
    }

    fn build_polygon(&mut self, j: &Value, parent: NodeId, parent_mode: Option<LayoutMode>, theme: &BTreeMap<String, String>) -> Option<NodeId> {
        let name = str_field(j, "name").filter(|s| !s.is_empty()).map_or_else(|| "Polygon".into(), |s| clip_chars(s, MAX_NAME_CHARS));
        let id = self.doc.alloc_id();
        let count = self.resolve_num(j.get("polygonCount"), theme, None).unwrap_or(3.0).round().clamp(3.0, 120.0) as u32;
        let mut node = Node::new(id, NodeKind::Polygon { count }, name, 100.0, 100.0);
        self.apply_leaf_sizing(&mut node, j, parent_mode, theme, (100.0, 100.0));
        self.apply_corners(&mut node, j, theme);
        self.apply_graphics(&mut node, j, theme, Some(Color::from_rgb8_const(0x88, 0x88, 0x88)));
        self.apply_entity_common(&mut node, j, parent_mode, theme);
        self.finish_leaf(node, parent, j)
    }

    fn build_line(&mut self, j: &Value, parent: NodeId, parent_mode: Option<LayoutMode>, theme: &BTreeMap<String, String>) -> Option<NodeId> {
        let name = str_field(j, "name").filter(|s| !s.is_empty()).map_or_else(|| "Line".into(), |s| clip_chars(s, MAX_NAME_CHARS));
        let id = self.doc.alloc_id();
        let mut node = Node::new(id, NodeKind::Line, name, 100.0, 0.0);
        self.apply_leaf_sizing(&mut node, j, parent_mode, theme, (100.0, 0.0));
        self.apply_graphics(&mut node, j, theme, None);
        self.apply_entity_common(&mut node, j, parent_mode, theme);
        self.finish_leaf(node, parent, j)
    }

    fn build_path(&mut self, j: &Value, parent: NodeId, parent_mode: Option<LayoutMode>, theme: &BTreeMap<String, String>) -> Option<NodeId> {
        let geom_str = str_field(j, "geometry").unwrap_or("");
        if geom_str.len() > MAX_PATH_BYTES {
            self.warn("oversized SVG path geometry was skipped");
            return None;
        }
        let mut bp = if geom_str.trim().is_empty() {
            BezPath::new()
        } else {
            match BezPath::from_svg(geom_str) {
                Ok(p) if p.elements().len() <= MAX_PATH_ELEMENTS && path_is_finite(&p) => p,
                _ => {
                    self.warn("invalid SVG path geometry was skipped");
                    return None;
                }
            }
        };

        let tight = bp.bounding_box();
        let (vx, vy, vw, vh) = j
            .get("viewBox")
            .and_then(Value::as_array)
            .and_then(|a| {
                let x = a.first()?.as_f64()?;
                let y = a.get(1)?.as_f64()?;
                let w = a.get(2)?.as_f64()?;
                let h = a.get(3)?.as_f64()?;
                (x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite() && w > 1e-9 && h > 1e-9).then_some((x, y, w, h))
            })
            .unwrap_or_else(|| (tight.x0, tight.y0, tight.width(), tight.height()));

        let def_w = if vw > 1e-6 { clamp_size(vw) } else { 24.0 };
        let def_h = if vh > 1e-6 { clamp_size(vh) } else { 24.0 };
        let name = str_field(j, "name").filter(|s| !s.is_empty()).map_or_else(|| "Path".into(), |s| clip_chars(s, MAX_NAME_CHARS));
        let id = self.doc.alloc_id();
        let mut node = Node::new(id, NodeKind::Vector { network: VectorNetwork::default() }, name, def_w, def_h);
        self.apply_leaf_sizing(&mut node, j, parent_mode, theme, (def_w, def_h));

        let sx = if vw.abs() > 1e-9 { node.width / vw } else { 1.0 };
        let sy = if vh.abs() > 1e-9 { node.height / vh } else { 1.0 };
        bp.apply_affine(Affine::scale_non_uniform(sx, sy) * Affine::translate((-vx, -vy)));

        let mut network = VectorNetwork::from_path(&bp);
        let winding = if str_field(j, "fillRule") == Some("evenodd") { WindingRule::EvenOdd } else { WindingRule::NonZero };
        for r in &mut network.regions {
            r.winding = winding;
        }
        node.kind = NodeKind::Vector { network };

        self.apply_graphics(&mut node, j, theme, Some(Color::from_rgb8_const(0x88, 0x88, 0x88)));
        self.apply_entity_common(&mut node, j, parent_mode, theme);
        self.finish_leaf(node, parent, j)
    }

    fn build_text(&mut self, j: &Value, parent: NodeId, parent_mode: Option<LayoutMode>, theme: &BTreeMap<String, String>) -> Option<NodeId> {
        let id = self.doc.alloc_id();
        let mut node = Node::new(id, NodeKind::Text(TextData::default()), "Text", 100.0, 20.0);

        let characters = match j.get("content") {
            Some(Value::Array(segments)) => {
                let joined: String = segments
                    .iter()
                    .filter_map(|seg| seg.as_str().map(str::to_string).or_else(|| str_field(seg, "content").map(str::to_string)))
                    .collect();
                clip_chars(&joined, MAX_TEXT_CHARS)
            }
            other => self.resolve_string(other, theme, Some((&mut node, "characters"))).map(|s| clip_chars(&s, MAX_TEXT_CHARS)).unwrap_or_default(),
        };
        let normalized_chars = characters.replace("\r\n", "\n").replace('\r', "\n");

        let explicit_name = str_field(j, "name").filter(|s| !s.is_empty()).map(|s| clip_chars(s, MAX_NAME_CHARS));
        node.name = explicit_name.unwrap_or_else(|| {
            let first_line = normalized_chars.lines().next().unwrap_or("").trim();
            if first_line.is_empty() { "Text".into() } else { clip_chars(first_line, 48) }
        });

        let font_family = self
            .resolve_string(j.get("fontFamily"), theme, Some((&mut node, "fontFamily")))
            .filter(|s| !s.trim().is_empty())
            .map(|s| clip_chars(s.trim(), 128))
            .unwrap_or_else(|| "Inter".into());
        let font_size = self.resolve_num(j.get("fontSize"), theme, Some((&mut node, "fontSize"))).unwrap_or(16.0).clamp(1.0, 4000.0);
        let raw_weight = self.resolve_string(j.get("fontWeight"), theme, None);
        let raw_fstyle = self.resolve_string(j.get("fontStyle"), theme, None);
        let italic = raw_fstyle.as_deref().is_some_and(|s| s.to_ascii_lowercase().contains("italic"))
            || raw_weight.as_deref().is_some_and(|s| s.to_ascii_lowercase().contains("italic"));
        let (font_style, custom_wght) = map_font_weight_and_style(raw_weight.as_deref(), italic);

        let line_height = match self.resolve_num(j.get("lineHeight"), theme, None) {
            Some(ratio) if ratio > 0.0 => LineHeight::Percent((ratio * 100.0).clamp(1.0, 2000.0)),
            _ => LineHeight::Auto,
        };
        let letter_spacing = match self.resolve_num(j.get("letterSpacing"), theme, Some((&mut node, "letterSpacing"))) {
            Some(px) => LetterSpacing::Pixels(px.clamp(-500.0, 500.0)),
            None => LetterSpacing::default(),
        };
        let underline = self.resolve_bool(j.get("underline"), theme, None).unwrap_or(false);
        let strikethrough = self.resolve_bool(j.get("strikethrough"), theme, None).unwrap_or(false);
        let decoration = if underline {
            TextDecoration::Underline
        } else if strikethrough {
            TextDecoration::Strikethrough
        } else {
            TextDecoration::None
        };
        let link = str_field(j, "href").filter(|s| !s.trim().is_empty()).map(|s| clip_chars(s.trim(), 2048));

        let mut variations = BTreeMap::new();
        if let Some(w) = custom_wght {
            variations.insert("wght".into(), w);
        }

        let style = TextStyle {
            font_family,
            font_style,
            font_size,
            line_height,
            letter_spacing,
            decoration,
            decoration_look: None,
            case: Default::default(),
            fills: None,
            link,
            variations,
            style_id: None,
        };

        let align_h = match str_field(j, "textAlign") {
            Some("center") => TextAlignH::Center,
            Some("right") => TextAlignH::Right,
            Some("justify" | "justified") => TextAlignH::Justified,
            _ => TextAlignH::Left,
        };
        let align_v = match str_field(j, "textAlignVertical") {
            Some("middle" | "center") => TextAlignV::Middle,
            Some("bottom") => TextAlignV::Bottom,
            _ => TextAlignV::Top,
        };

        let in_flex = parent_mode.is_some() && str_field(j, "layoutPosition") != Some("absolute");
        let parsed_w = self.parse_axis_size(j.get("width"), "width", &mut node, theme, ParsedSize::Hug(None));
        let parsed_h = self.parse_axis_size(j.get("height"), "height", &mut node, theme, ParsedSize::Hug(None));

        let growth = match str_field(j, "textGrowth") {
            Some("fixed-width") => TextAutoResize::Height,
            Some("fixed-width-height") => TextAutoResize::None,
            Some("auto") => TextAutoResize::WidthAndHeight,
            _ if in_flex && matches!(parsed_w, ParsedSize::Fill(_)) => TextAutoResize::Height,
            _ => TextAutoResize::WidthAndHeight,
        };

        match growth {
            TextAutoResize::WidthAndHeight => {
                node.child_layout.sizing_h = Sizing::Hug;
                node.child_layout.sizing_v = Sizing::Hug;
                node.width = 100.0;
                node.height = (font_size * 1.25).max(16.0);
            }
            TextAutoResize::Height => {
                let (sh, w_px) = resolve_box_axis(parsed_w, false, in_flex, 200.0);
                node.child_layout.sizing_h = sh;
                node.child_layout.sizing_v = Sizing::Hug;
                node.width = w_px.max(1.0);
                node.height = (font_size * 1.25).max(16.0);
            }
            TextAutoResize::None | TextAutoResize::Truncate => {
                let (sh, w_px) = resolve_box_axis(parsed_w, false, in_flex, 200.0);
                let (sv, h_px) = resolve_box_axis(parsed_h, false, in_flex, (font_size * 1.5).max(20.0));
                node.child_layout.sizing_h = sh;
                node.child_layout.sizing_v = sv;
                node.width = w_px.max(1.0);
                node.height = h_px.max(1.0);
            }
        }

        node.kind = NodeKind::Text(TextData { characters: normalized_chars, style, auto_resize: growth, align_h, align_v, ..TextData::default() });

        // If `fill` was bound to an unresolved `$variable`, use `#888888` so text remains readable on both light & dark frames.
        self.apply_graphics(&mut node, j, theme, Some(Color::from_rgb8_const(0x88, 0x88, 0x88)));
        self.apply_entity_common(&mut node, j, parent_mode, theme);
        self.finish_leaf(node, parent, j)
    }

    fn build_note(&mut self, j: &Value, parent: NodeId, parent_mode: Option<LayoutMode>, theme: &BTreeMap<String, String>) -> Option<NodeId> {
        let name = str_field(j, "name").filter(|s| !s.is_empty()).map_or_else(|| "Note".into(), |s| clip_chars(s, MAX_NAME_CHARS));
        let id = self.doc.alloc_id();
        let mut node = Node::new(id, NodeKind::Frame, name, 200.0, 200.0);
        self.apply_leaf_sizing(&mut node, j, parent_mode, theme, (200.0, 200.0));
        node.clip = false;
        node.corners = Corners { radii: [8.0; 4], smoothing: 0.0 };
        let default_yellow = Color::from_rgb8_const(0xFE, 0xE4, 0x84);
        let bg = self.resolve_color(j.get("color"), theme, Some(default_yellow)).map(|(c, _, _)| c).unwrap_or(default_yellow);
        node.fills = vec![Paint::solid(bg)];
        node.layout = Some(AutoLayout { mode: LayoutMode::Vertical, padding: [16.0; 4], gap: 8.0, ..AutoLayout::default() });
        self.apply_entity_common(&mut node, j, parent_mode, theme);
        let frame_id = self.finish_leaf(node, parent, j)?;

        if let Some(txt) = self.resolve_string(j.get("content"), theme, None).filter(|s| !s.is_empty()) {
            let tid = self.doc.alloc_id();
            let mut tnode = Node::new(
                tid,
                NodeKind::Text(TextData {
                    characters: clip_chars(&txt, MAX_TEXT_CHARS),
                    style: TextStyle { font_size: 14.0, ..TextStyle::default() },
                    auto_resize: TextAutoResize::Height,
                    ..TextData::default()
                }),
                "Note text",
                168.0,
                20.0,
            );
            tnode.child_layout.sizing_h = Sizing::Fill;
            tnode.child_layout.sizing_v = Sizing::Hug;
            tnode.fills = vec![Paint::solid(Color::from_rgb8_const(0x1F, 0x29, 0x37))];
            let _ = self.doc.insert(tnode, frame_id, None);
        }
        Some(frame_id)
    }

    fn build_icon(&mut self, j: &Value, parent: NodeId, parent_mode: Option<LayoutMode>, theme: &BTreeMap<String, String>) -> Option<NodeId> {
        let icon_name = self.resolve_string(j.get("icon").or_else(|| j.get("iconFontName")), theme, None).unwrap_or_else(|| "icon".into());
        let lib_name = self.resolve_string(j.get("library").or_else(|| j.get("iconFontFamily")), theme, None).unwrap_or_default();
        let label =
            str_field(j, "name").filter(|s| !s.is_empty()).map(|s| clip_chars(s, MAX_NAME_CHARS)).unwrap_or_else(|| clip_chars(&icon_name, 64));

        let id = self.doc.alloc_id();
        let mut node = Node::new(id, NodeKind::Vector { network: VectorNetwork::default() }, label, 24.0, 24.0);
        self.apply_leaf_sizing(&mut node, j, parent_mode, theme, (24.0, 24.0));

        let bp = geometric_icon_primitive(&icon_name, node.width, node.height);
        node.kind = NodeKind::Vector { network: VectorNetwork::from_path(&bp) };

        // Paint geometric icon strokes using the icon's `fill` colours and `weight`.
        node.fills.clear();
        let paints = match j.get("fill") {
            Some(fv) => {
                self.parse_paints(fv, node.width, node.height, "stroke", "strokes", &mut node, theme, Some(Color::from_rgb8_const(0x6B, 0x72, 0x80)))
            }
            None => vec![Paint::solid(Color::from_rgb8_const(0x6B, 0x72, 0x80))],
        };
        node.strokes = paints;
        let w_factor = self.resolve_num(j.get("weight"), theme, None).unwrap_or(400.0).clamp(100.0, 900.0) / 400.0;
        let stroke_w = ((node.width.min(node.height) / 12.0) * w_factor).clamp(1.0, 8.0);
        node.stroke = StrokeProps {
            weight: stroke_w,
            align: StrokeAlign::Center,
            cap_start: StrokeCap::Round,
            cap_end: StrokeCap::Round,
            join: StrokeJoin::Round,
            ..StrokeProps::default()
        };
        if let Some(eff_val) = j.get("effect") {
            node.effects = self.parse_effects(eff_val, theme);
        }
        node.plugin_data.insert("pen:icon".into(), format!("{lib_name}:{icon_name}"));
        self.warn("icon library references were rendered as geometric stand-in vectors");
        self.apply_entity_common(&mut node, j, parent_mode, theme);
        self.finish_leaf(node, parent, j)
    }

    fn build_browser(&mut self, j: &Value, parent: NodeId, parent_mode: Option<LayoutMode>, theme: &BTreeMap<String, String>) -> Option<NodeId> {
        let url = str_field(j, "url").unwrap_or("").trim();
        let name = str_field(j, "name")
            .filter(|s| !s.is_empty())
            .map(|s| clip_chars(s, MAX_NAME_CHARS))
            .unwrap_or_else(|| if url.is_empty() { "Browser".into() } else { clip_chars(url, 64) });
        let id = self.doc.alloc_id();
        let mut node = Node::new(id, NodeKind::Frame, name, 800.0, 600.0);
        self.apply_leaf_sizing(&mut node, j, parent_mode, theme, (800.0, 600.0));
        node.clip = true;
        node.fills = vec![Paint::solid(Color::from_rgb8_const(0xF8, 0xFA, 0xFC))];
        self.apply_corners(&mut node, j, theme);
        self.apply_graphics(&mut node, j, theme, None);
        if node.fills.is_empty() {
            node.fills = vec![Paint::solid(Color::from_rgb8_const(0xF8, 0xFA, 0xFC))];
        }
        if !url.is_empty() {
            node.dev_resources.push(DevResource { url: clip_chars(url, 2048), name: "Browser URL".into() });
        }
        node.layout = Some(AutoLayout {
            mode: LayoutMode::Vertical,
            main_align: MainAlign::Center,
            cross_align: CrossAlign::Center,
            padding: [24.0; 4],
            gap: 8.0,
            ..AutoLayout::default()
        });
        self.apply_entity_common(&mut node, j, parent_mode, theme);
        let fid = self.finish_leaf(node, parent, j)?;

        let label = if url.is_empty() { "Browser preview".to_string() } else { clip_chars(url, 256) };
        let tid = self.doc.alloc_id();
        let mut tnode = Node::new(
            tid,
            NodeKind::Text(TextData {
                characters: label,
                style: TextStyle { font_size: 14.0, ..TextStyle::default() },
                auto_resize: TextAutoResize::WidthAndHeight,
                align_h: TextAlignH::Center,
                ..TextData::default()
            }),
            "URL",
            120.0,
            20.0,
        );
        tnode.child_layout.sizing_h = Sizing::Hug;
        tnode.child_layout.sizing_v = Sizing::Hug;
        tnode.fills = vec![Paint::solid(Color::from_rgb8_const(0x47, 0x55, 0x69))];
        let _ = self.doc.insert(tnode, fid, None);
        self.warn("live `browser` nodes were imported as placeholder frames");
        Some(fid)
    }

    fn build_script(&mut self, j: &Value, parent: NodeId, parent_mode: Option<LayoutMode>, theme: &BTreeMap<String, String>) -> Option<NodeId> {
        let name = str_field(j, "name").filter(|s| !s.is_empty()).map_or_else(|| "Script".into(), |s| clip_chars(s, MAX_NAME_CHARS));
        let id = self.doc.alloc_id();
        let mut node = Node::new(id, NodeKind::Frame, name, 100.0, 100.0);
        node.fills.clear();
        node.clip = self.resolve_bool(j.get("clip"), theme, None).unwrap_or(false);
        self.apply_leaf_sizing(&mut node, j, parent_mode, theme, (100.0, 100.0));
        if let Some(uri) = str_field(j, "scriptUri") {
            node.plugin_data.insert("pen:scriptUri".into(), clip_chars(uri, 512));
        }
        self.apply_entity_common(&mut node, j, parent_mode, theme);
        self.warn("`script` layers cannot run JavaScript and were imported as empty frames");
        self.finish_leaf(node, parent, j)
    }

    fn apply_leaf_sizing(
        &mut self,
        node: &mut Node,
        j: &Value,
        parent_mode: Option<LayoutMode>,
        theme: &BTreeMap<String, String>,
        defaults: (f64, f64),
    ) {
        let in_flex = parent_mode.is_some() && str_field(j, "layoutPosition") != Some("absolute");
        let pw = self.parse_axis_size(j.get("width"), "width", node, theme, ParsedSize::Fixed(defaults.0));
        let ph = self.parse_axis_size(j.get("height"), "height", node, theme, ParsedSize::Fixed(defaults.1));
        let (sh, w_px) = resolve_box_axis(pw, false, in_flex, defaults.0);
        let (sv, h_px) = resolve_box_axis(ph, false, in_flex, defaults.1);
        node.width = w_px;
        node.height = h_px;
        node.child_layout.sizing_h = sh;
        node.child_layout.sizing_v = sv;
    }

    fn finish_leaf(&mut self, node: Node, parent: NodeId, j: &Value) -> Option<NodeId> {
        let id = node.id;
        self.doc.insert(node, parent, None).ok()?;
        self.count += 1;
        if let Some(pid) = str_field(j, "id").filter(|s| !s.is_empty()) {
            self.imported_ids.entry(pid.to_string()).or_insert(id);
        }
        Some(id)
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn num_lit(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|n| n.is_finite())
}

fn clamp_coord(v: f64) -> f64 {
    if v.is_finite() { v.clamp(-MAX_COORD, MAX_COORD) } else { 0.0 }
}

fn clamp_size(v: f64) -> f64 {
    if v.is_finite() { v.clamp(0.0, MAX_SIZE) } else { 0.0 }
}

fn clip_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn normalize_var_name(name: &str) -> String {
    let trimmed = name.trim();
    trimmed.strip_prefix('$').unwrap_or(trimmed).trim().to_string()
}

fn pick_themed_raw<'a>(entries: &'a [(BTreeMap<String, String>, Value)], theme: &BTreeMap<String, String>) -> Option<&'a Value> {
    // Prefer the most specific matching theme rule (and latest matching entry on ties), else fallback to first entry.
    let mut best: Option<(usize, &'a Value)> = None;
    for (cond, val) in entries {
        if cond.iter().all(|(k, v)| theme.get(k) == Some(v)) && best.is_none_or(|(score, _)| cond.len() >= score) {
            best = Some((cond.len(), val));
        }
    }
    best.map(|(_, v)| v).or_else(|| entries.first().map(|(_, v)| v))
}

fn parse_literal_var_value(kind: VariableType, raw: &Value) -> Option<VariableValue> {
    match kind {
        VariableType::Color => {
            let c = raw.as_str().and_then(|s| Color::parse_css(s.trim())).or_else(|| serde_json::from_value::<Color>(raw.clone()).ok())?;
            Some(VariableValue::Color(c))
        }
        VariableType::Number => raw
            .as_f64()
            .or_else(|| raw.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
            .filter(|n| n.is_finite())
            .map(|n| VariableValue::Number(n.clamp(-MAX_COORD, MAX_COORD))),
        VariableType::String => raw.as_str().map(|s| VariableValue::String(clip_chars(s, 10_000))),
        VariableType::Boolean => raw.as_bool().map(VariableValue::Boolean),
    }
}

fn parse_paren_fallback(rest: &str) -> Option<f64> {
    let inner = rest.trim().strip_prefix('(')?.strip_suffix(')')?;
    inner.trim().parse::<f64>().ok().filter(|n| n.is_finite()).map(clamp_size)
}

fn resolve_box_axis(parsed: ParsedSize, can_hug: bool, in_flex: bool, default_px: f64) -> (Sizing, f64) {
    match parsed {
        ParsedSize::Fixed(px) => (Sizing::Fixed, px),
        ParsedSize::Hug(fb) => {
            let px = fb.unwrap_or(default_px);
            if can_hug { (Sizing::Hug, px) } else { (Sizing::Fixed, px) }
        }
        ParsedSize::Fill(fb) => {
            let px = fb.unwrap_or(default_px);
            if in_flex { (Sizing::Fill, px) } else { (Sizing::Fixed, px) }
        }
    }
}

fn compose_transform(x: f64, y: f64, w: f64, h: f64, rot_ccw_deg: f64, flip_x: bool, flip_y: bool) -> Affine {
    let mut xf = Affine::translate((x, y));
    if rot_ccw_deg.is_finite() && rot_ccw_deg.abs() > 1e-9 {
        xf *= Affine::rotate((-rot_ccw_deg).to_radians());
    }
    if flip_x {
        xf *= Affine::translate((w, 0.0)) * Affine::scale_non_uniform(-1.0, 1.0);
    }
    if flip_y {
        xf *= Affine::translate((0.0, h)) * Affine::scale_non_uniform(1.0, -1.0);
    }
    xf
}

fn parse_blend_mode(s: &str) -> BlendMode {
    match s {
        "darken" => BlendMode::Darken,
        "multiply" => BlendMode::Multiply,
        "linearBurn" => BlendMode::PlusDarker,
        "colorBurn" => BlendMode::ColorBurn,
        "light" | "lighten" => BlendMode::Lighten,
        "screen" => BlendMode::Screen,
        "linearDodge" => BlendMode::PlusLighter,
        "colorDodge" => BlendMode::ColorDodge,
        "overlay" => BlendMode::Overlay,
        "softLight" => BlendMode::SoftLight,
        "hardLight" => BlendMode::HardLight,
        "difference" => BlendMode::Difference,
        "exclusion" => BlendMode::Exclusion,
        "hue" => BlendMode::Hue,
        "saturation" => BlendMode::Saturation,
        "color" => BlendMode::Color,
        "luminosity" => BlendMode::Luminosity,
        _ => BlendMode::Normal,
    }
}

fn parse_stroke_align(s: &str, default: StrokeAlign) -> StrokeAlign {
    match s {
        "inner" | "inside" => StrokeAlign::Inside,
        "center" => StrokeAlign::Center,
        "outer" | "outside" => StrokeAlign::Outside,
        _ => default,
    }
}

fn parse_affine6(v: Option<&Value>) -> Option<[f64; 6]> {
    let arr = v?.as_array()?;
    if arr.len() < 6 {
        return None;
    }
    let mut out = [0.0; 6];
    for i in 0..6 {
        let x = arr.get(i)?.as_f64()?;
        if !x.is_finite() {
            return None;
        }
        out[i] = x;
    }
    Some(out)
}

fn path_is_finite(bp: &BezPath) -> bool {
    use figcraft_doc::geom::kurbo::PathEl;
    let ok = |p: Point| p.x.is_finite() && p.y.is_finite() && p.x.abs() <= MAX_COORD && p.y.abs() <= MAX_COORD;
    bp.elements().iter().all(|el| match *el {
        PathEl::MoveTo(p) | PathEl::LineTo(p) => ok(p),
        PathEl::QuadTo(c, p) => ok(c) && ok(p),
        PathEl::CurveTo(c1, c2, p) => ok(c1) && ok(c2) && ok(p),
        PathEl::ClosePath => true,
    })
}

fn map_font_weight_and_style(raw_weight: Option<&str>, italic: bool) -> (String, Option<f64>) {
    let mut custom = None;
    let base = match raw_weight.map(str::trim) {
        None | Some("") => "Regular",
        Some(w) => {
            if let Ok(num) = w.parse::<f64>()
                && num.is_finite()
            {
                let clamped = num.clamp(100.0, 900.0);
                let rounded = ((clamped / 100.0).round() * 100.0) as i32;
                if (clamped - f64::from(rounded)).abs() > 1.0 {
                    custom = Some(clamped);
                }
                match rounded {
                    100 => "Thin",
                    200 => "Extra Light",
                    300 => "Light",
                    400 => "Regular",
                    500 => "Medium",
                    600 => "Semi Bold",
                    700 => "Bold",
                    800 => "Extra Bold",
                    900 => "Black",
                    _ => "Regular",
                }
            } else {
                let key: String = w.to_ascii_lowercase().replace("italic", "").chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                match key.as_str() {
                    "thin" | "hairline" => "Thin",
                    "extralight" | "ultralight" => "Extra Light",
                    "light" => "Light",
                    "normal" | "regular" | "book" => "Regular",
                    "medium" => "Medium",
                    "semibold" | "demibold" => "Semi Bold",
                    "bold" => "Bold",
                    "extrabold" | "ultrabold" => "Extra Bold",
                    "black" | "heavy" => "Black",
                    _ => "Regular",
                }
            }
        }
    };
    let style_name = match (base, italic) {
        ("Regular", true) => "Italic".into(),
        (b, true) => format!("{b} Italic"),
        (b, false) => b.into(),
    };
    (style_name, custom)
}

fn decode_data_image(url: &str) -> Option<Vec<u8>> {
    let rest = url.strip_prefix("data:image/")?;
    let (_, b64) = rest.split_once(";base64,")?;
    if b64.len() > MAX_DATA_IMAGE_BYTES * 2 {
        return None;
    }
    let cleaned: String = b64.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&cleaned)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&cleaned))
        .ok()?;
    (decoded.len() <= MAX_DATA_IMAGE_BYTES).then_some(decoded)
}

/// Pure-math geometric stand-in primitives for `.pen` `icon` nodes (no third-party icon assets).
fn geometric_icon_primitive(name: &str, width: f64, height: f64) -> BezPath {
    let w = width.max(4.0);
    let h = height.max(4.0);
    let p = |nx: f64, ny: f64| Point::new(nx * w, ny * h);
    let key = name.to_ascii_lowercase();
    let mut bp = BezPath::new();

    if key.contains("close") || key == "x" || key.contains("cancel") || key.contains("multiply") {
        bp.move_to(p(0.25, 0.25));
        bp.line_to(p(0.75, 0.75));
        bp.move_to(p(0.75, 0.25));
        bp.line_to(p(0.25, 0.75));
    } else if key.contains("check") || key.contains("done") || key.contains("tick") {
        bp.move_to(p(0.20, 0.52));
        bp.line_to(p(0.42, 0.74));
        bp.line_to(p(0.80, 0.28));
    } else if key.contains("plus") || key.contains("add") {
        bp.move_to(p(0.50, 0.22));
        bp.line_to(p(0.50, 0.78));
        bp.move_to(p(0.22, 0.50));
        bp.line_to(p(0.78, 0.50));
    } else if key.contains("minus") || key.contains("remove") {
        bp.move_to(p(0.22, 0.50));
        bp.line_to(p(0.78, 0.50));
    } else if key.contains("chevron") || key.contains("caret") {
        if key.contains("left") {
            bp.move_to(p(0.62, 0.25));
            bp.line_to(p(0.38, 0.50));
            bp.line_to(p(0.62, 0.75));
        } else if key.contains("up") {
            bp.move_to(p(0.25, 0.62));
            bp.line_to(p(0.50, 0.38));
            bp.line_to(p(0.75, 0.62));
        } else if key.contains("down") {
            bp.move_to(p(0.25, 0.38));
            bp.line_to(p(0.50, 0.62));
            bp.line_to(p(0.75, 0.38));
        } else {
            bp.move_to(p(0.38, 0.25));
            bp.line_to(p(0.62, 0.50));
            bp.line_to(p(0.38, 0.75));
        }
    } else if key.contains("arrow") {
        if key.contains("left") {
            bp.move_to(p(0.78, 0.50));
            bp.line_to(p(0.22, 0.50));
            bp.move_to(p(0.44, 0.28));
            bp.line_to(p(0.22, 0.50));
            bp.line_to(p(0.44, 0.72));
        } else if key.contains("up") {
            bp.move_to(p(0.50, 0.78));
            bp.line_to(p(0.50, 0.22));
            bp.move_to(p(0.28, 0.44));
            bp.line_to(p(0.50, 0.22));
            bp.line_to(p(0.72, 0.44));
        } else if key.contains("down") {
            bp.move_to(p(0.50, 0.22));
            bp.line_to(p(0.50, 0.78));
            bp.move_to(p(0.28, 0.56));
            bp.line_to(p(0.50, 0.78));
            bp.line_to(p(0.72, 0.56));
        } else {
            bp.move_to(p(0.22, 0.50));
            bp.line_to(p(0.78, 0.50));
            bp.move_to(p(0.56, 0.28));
            bp.line_to(p(0.78, 0.50));
            bp.line_to(p(0.56, 0.72));
        }
    } else if key.contains("menu") || key.contains("bars") || key.contains("hamburger") {
        for y in [0.28, 0.50, 0.72] {
            bp.move_to(p(0.20, y));
            bp.line_to(p(0.80, y));
        }
    } else if key.contains("search") || key.contains("magnif") {
        let c = figcraft_doc::geom::kurbo::Circle::new(p(0.44, 0.44), 0.22 * w.min(h));
        bp.extend(c.path_elements(0.1));
        bp.move_to(p(0.60, 0.60));
        bp.line_to(p(0.80, 0.80));
    } else if key.contains("user") || key.contains("person") || key.contains("avatar") {
        let head = figcraft_doc::geom::kurbo::Circle::new(p(0.50, 0.34), 0.14 * w.min(h));
        bp.extend(head.path_elements(0.1));
        bp.move_to(p(0.24, 0.80));
        bp.curve_to(p(0.26, 0.60), p(0.74, 0.60), p(0.76, 0.80));
    } else if key.contains("star") {
        let pts = figcraft_doc::geom::star_points(w * 0.68, h * 0.68, 5, 0.42);
        let shift = Vec2::new(w * 0.16, h * 0.16);
        for (i, pt) in pts.into_iter().enumerate() {
            if i == 0 {
                bp.move_to(pt + shift);
            } else {
                bp.line_to(pt + shift);
            }
        }
        bp.close_path();
    } else {
        // Clean geometric rounded box + centre dot placeholder.
        let r = figcraft_doc::geom::rounded_rect(w * 0.64, h * 0.64, [w.min(h) * 0.14; 4], 0.0);
        let mut boxed = r;
        boxed.apply_affine(Affine::translate((w * 0.18, h * 0.18)));
        bp.extend(boxed);
    }
    bp
}
