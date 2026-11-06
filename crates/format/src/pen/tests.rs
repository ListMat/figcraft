use base64::Engine as _;
use figcraft_doc::color::{Color, GradientKind, ImagePaint, ImageScaleMode, PaintKind};
use figcraft_doc::{
    CrossAlign, EffectKind, LayoutMode, LetterSpacing, LineHeight, MainAlign, NodeKind, PropDef, Sizing, StrokeAlign, StrokeCap, StrokeJoin,
    TextAlignH, TextAlignV, TextAutoResize, TextDecoration, VariableType, WindingRule,
};
use serde_json::json;

use super::{extract_clipboard_html, import, is_pen_json, looks_like_pen_json, try_import};
use crate::rest::no_images;

#[test]
fn extracts_pen_clipboard_html_and_ignores_node_id_fallback() {
    let payload = json!({
        "source": "9f63a8b0-0000-4000-8000-000000000001",
        "localData": ["rK3Pf"],
        "remoteData": {
            "nodes": [
                {
                    "id": "rK3Pf",
                    "type": "frame",
                    "name": "Hero Block",
                    "width": 1200,
                    "height": 640,
                    "layout": "vertical",
                    "padding": [48, 64],
                    "gap": 24,
                    "fill": "#0F172A",
                    "children": [
                        {
                            "id": "t1",
                            "type": "text",
                            "content": "Build faster with FigCraft",
                            "fontSize": 48,
                            "fontWeight": "700",
                            "fill": "#F8FAFC"
                        }
                    ]
                }
            ],
            "variables": {},
            "themes": {}
        }
    });
    let b64 = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&payload).unwrap());
    let html = format!(
        "Version:0.9\r\nStartHTML:00000097\r\n<!--StartFragment--><span data-pen-node-clipboard=\"{b64}\">Node ID: rK3Pf</span><!--EndFragment-->"
    );

    let extracted = extract_clipboard_html(&html).expect("clipboard payload should be extracted");
    let (doc, warnings) = try_import(&extracted, "Clipboard", &no_images).expect("recognised as pen").expect("imported");
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    let page = doc.first_page().unwrap();
    let roots = &doc.node(page).unwrap().children;
    assert_eq!(roots.len(), 1);
    let hero = doc.node(roots[0]).unwrap();
    assert_eq!(hero.name, "Hero Block");
    assert_eq!(hero.width, 1200.0);
    assert_eq!(hero.height, 640.0);
    assert!(!hero.clip, ".pen frames default to clip=false");
    let layout = hero.layout.as_ref().expect("vertical layout");
    assert_eq!(layout.mode, LayoutMode::Vertical);
    assert_eq!(layout.padding, [48.0, 64.0, 48.0, 64.0]);
    assert_eq!(layout.gap, 24.0);
    assert_eq!(hero.fills[0].solid_color(), Some(Color::from_hex("#0F172A").unwrap()));

    let heading = doc.node(hero.children[0]).unwrap();
    let NodeKind::Text(td) = &heading.kind else { panic!("expected text child") };
    assert_eq!(td.characters, "Build faster with FigCraft");
    assert_eq!(td.style.font_size, 48.0);
    assert_eq!(td.style.font_style, "Bold");
    assert_eq!(td.auto_resize, TextAutoResize::WidthAndHeight);
}

#[test]
fn frame_defaults_and_flexbox_sizing_match_pen_schema() {
    let v = json!({
        "version": "2.22",
        "children": [
            {
                "id": "row",
                "type": "frame",
                "justifyContent": "space_between",
                "alignItems": "center",
                "children": [
                    {
                        "id": "c1",
                        "type": "rectangle",
                        "width": "fill_container(80)",
                        "height": 40,
                        "fill": "#3B82F6"
                    },
                    {
                        "id": "badge",
                        "type": "rectangle",
                        "layoutPosition": "absolute",
                        "x": 12,
                        "y": -6,
                        "width": 16,
                        "height": 16,
                        "cornerRadius": 8,
                        "fill": "#EF4444"
                    }
                ]
            }
        ]
    });
    assert!(is_pen_json(&v));
    assert!(looks_like_pen_json(&serde_json::to_string(&v).unwrap()));

    let (doc, _) = import(&v, "FlexTest", &no_images).unwrap();
    let page = doc.first_page().unwrap();
    let row = doc.node(doc.node(page).unwrap().children[0]).unwrap();
    // Default frame: horizontal layout, fit_content (Hug) on both axes, transparent fill, gap=0, padding=0.
    assert!(row.fills.is_empty(), "omitted fill on .pen frame must be transparent, not white");
    assert_eq!(row.child_layout.sizing_h, Sizing::Hug);
    assert_eq!(row.child_layout.sizing_v, Sizing::Hug);
    let al = row.layout.as_ref().unwrap();
    assert_eq!(al.mode, LayoutMode::Horizontal);
    assert_eq!(al.gap, 0.0);
    assert_eq!(al.padding, [0.0; 4]);
    assert_eq!(al.main_align, MainAlign::SpaceBetween);
    assert_eq!(al.cross_align, CrossAlign::Center);

    let c1 = doc.node(row.children[0]).unwrap();
    assert_eq!(c1.child_layout.sizing_h, Sizing::Fill);
    assert_eq!(c1.child_layout.sizing_v, Sizing::Fixed);
    assert_eq!(c1.height, 40.0);

    let badge = doc.node(row.children[1]).unwrap();
    assert!(badge.child_layout.absolute);
    assert_eq!(badge.transform.translation(), Vec2::new(12.0, -6.0));
    assert_eq!(badge.corners.radii, [8.0; 4]);
}

use figcraft_doc::geom::Vec2;

#[test]
fn text_growth_line_height_and_typography_mapping() {
    let v = json!({
        "version": "2.22",
        "children": [
            {
                "id": "wrap",
                "type": "frame",
                "layout": "vertical",
                "width": 400,
                "children": [
                    {
                        "id": "p1",
                        "type": "text",
                        "content": "Wrapped paragraph body",
                        "textGrowth": "fixed-width",
                        "width": "fill_container",
                        "fontFamily": "Plus Jakarta Sans",
                        "fontSize": 18,
                        "fontWeight": "600",
                        "fontStyle": "italic",
                        "lineHeight": 1.5,
                        "letterSpacing": -0.4,
                        "underline": true,
                        "textAlign": "center",
                        "textAlignVertical": "middle",
                        "href": "https://example.com",
                        "fill": "#111827"
                    }
                ]
            }
        ]
    });
    let (doc, _) = import(&v, "TextTest", &no_images).unwrap();
    let page = doc.first_page().unwrap();
    let wrap = doc.node(doc.node(page).unwrap().children[0]).unwrap();
    let p1 = doc.node(wrap.children[0]).unwrap();
    assert_eq!(p1.child_layout.sizing_h, Sizing::Fill);
    assert_eq!(p1.child_layout.sizing_v, Sizing::Hug);
    let NodeKind::Text(td) = &p1.kind else { panic!("expected text") };
    assert_eq!(td.auto_resize, TextAutoResize::Height);
    assert_eq!(td.style.font_family, "Plus Jakarta Sans");
    assert_eq!(td.style.font_style, "Semi Bold Italic");
    assert_eq!(td.style.line_height, LineHeight::Percent(150.0));
    assert_eq!(td.style.letter_spacing, LetterSpacing::Pixels(-0.4));
    assert_eq!(td.style.decoration, TextDecoration::Underline);
    assert_eq!(td.style.link.as_deref(), Some("https://example.com"));
    assert_eq!(td.align_h, TextAlignH::Center);
    assert_eq!(td.align_v, TextAlignV::Middle);
}

#[test]
fn shapes_paths_gradients_strokes_effects_and_crop_transform() {
    // 1x1 red PNG as data URL.
    let mut png_bytes = Vec::new();
    let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
    img.write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png).unwrap();
    let data_url = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&png_bytes));

    let v = json!({
        "version": "2.22",
        "children": [
            {
                "id": "donut",
                "type": "ellipse",
                "width": 120,
                "height": 120,
                "innerRadius": 0.5,
                "startAngle": 90,
                "sweepAngle": 180,
                "fill": {
                    "type": "gradient",
                    "gradientType": "linear",
                    "rotation": 180,
                    "colors": [
                        {"color": "#FF0000", "position": 0},
                        {"color": "#0000FF", "position": 1}
                    ]
                }
            },
            {
                "id": "vec",
                "type": "path",
                "width": 48,
                "height": 24,
                "viewBox": [0, 0, 24, 12],
                "fillRule": "evenodd",
                "geometry": "M 0 0 L 24 0 L 24 12 L 0 12 Z",
                "fill": "#22C55E",
                "stroke": "#14532D",
                "strokeWidth": {"top": 1, "right": 2, "bottom": 3, "left": 4},
                "strokeAlignment": "outer",
                "strokeLinecap": "round",
                "strokeLinejoin": "bevel",
                "effect": [
                    {"type": "blur", "radius": 6},
                    {"type": "background_blur", "radius": 12},
                    {"type": "shadow", "shadowType": "outer", "offset": {"x": 2, "y": 8}, "blur": 16, "color": "#00000040"},
                    {"type": "shadow", "shadowType": "inner", "offset": {"x": 0, "y": 1}, "blur": 2, "color": "#FFFFFF80"}
                ]
            },
            {
                "id": "pic",
                "type": "rectangle",
                "width": 200,
                "height": 100,
                "cornerRadius": [4, 8, 12, 16],
                "fill": {
                    "type": "image",
                    "url": data_url,
                    "mode": "cover",
                    "transform": [2, 0, 0, 1, -1, 0]
                }
            }
        ]
    });

    let (doc, warnings) = import(&v, "GraphicsTest", &no_images).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let page = doc.first_page().unwrap();
    let kids = &doc.node(page).unwrap().children;

    // Donut ellipse: 90° CCW -> -PI/2 CW, 180° CCW -> -PI CW, linear gradient 180° -> top-to-bottom.
    let donut = doc.node(kids[0]).unwrap();
    let NodeKind::Ellipse { start, sweep, inner } = donut.kind else { panic!("expected ellipse") };
    assert!((start - (-std::f64::consts::FRAC_PI_2)).abs() < 1e-6);
    assert!((sweep - (-std::f64::consts::PI)).abs() < 1e-6);
    assert!((inner - 0.5).abs() < 1e-6);
    let PaintKind::Gradient(grad) = &donut.fills[0].kind else { panic!("expected gradient") };
    assert_eq!(grad.kind, GradientKind::Linear);
    assert!((grad.from.1 - 0.0).abs() < 1e-6 && (grad.to.1 - 1.0).abs() < 1e-6, "180° goes down: {grad:?}");

    // Path with viewBox [0,0,24,12] stretched onto 48x24.
    let vec_node = doc.node(kids[1]).unwrap();
    let NodeKind::Vector { network } = &vec_node.kind else { panic!("expected vector") };
    assert_eq!(network.regions[0].winding, WindingRule::EvenOdd);
    let max_x = network.vertices.iter().map(|v| v.x).fold(0.0_f64, f64::max);
    let max_y = network.vertices.iter().map(|v| v.y).fold(0.0_f64, f64::max);
    assert!((max_x - 48.0).abs() < 1e-6 && (max_y - 24.0).abs() < 1e-6);
    assert_eq!(vec_node.stroke.sides, Some([1.0, 2.0, 3.0, 4.0]));
    assert_eq!(vec_node.stroke.weight, 4.0);
    assert_eq!(vec_node.stroke.align, StrokeAlign::Outside);
    assert_eq!(vec_node.stroke.cap_start, StrokeCap::Round);
    assert_eq!(vec_node.stroke.join, StrokeJoin::Bevel);
    assert_eq!(vec_node.effects.len(), 4);
    assert!(matches!(vec_node.effects[0].kind, EffectKind::LayerBlur(_)));
    assert!(matches!(vec_node.effects[1].kind, EffectKind::BackgroundBlur(_)));
    assert!(matches!(vec_node.effects[2].kind, EffectKind::DropShadow(_)));
    assert!(matches!(vec_node.effects[3].kind, EffectKind::InnerShadow(_)));

    // Image with crop `[2, 0, 0, 1, -1, 0]` (right half of image: UV 0.5..1.0, full image spans -1..1 in unit box).
    let pic = doc.node(kids[2]).unwrap();
    assert_eq!(pic.corners.radii, [4.0, 8.0, 12.0, 16.0]);
    let PaintKind::Image(ip) = &pic.fills[0].kind else { panic!("expected image paint") };
    assert_eq!(ip.scale_mode, ImageScaleMode::Crop);
    assert!(doc.images.contains_key(&ip.image));
    let c = ip.crop.unwrap();
    assert!((c[0] - 0.5).abs() < 1e-6 && (c[4] - 0.5).abs() < 1e-6);
    let (x0, y0, x1, y1) = ImagePaint::crop_rect(c);
    assert!((x0 - (-1.0)).abs() < 1e-6 && (x1 - 1.0).abs() < 1e-6 && (y0 - 0.0).abs() < 1e-6 && (y1 - 1.0).abs() < 1e-6);
}

#[test]
fn reusable_components_slots_refs_descendants_and_cycle_guard() {
    let v = json!({
        "version": "2.22",
        "children": [
            {
                "id": "btn_comp",
                "type": "frame",
                "name": "Button",
                "reusable": true,
                "padding": [10, 20],
                "gap": 8,
                "fill": "#2563EB",
                "cornerRadius": 8,
                "children": [
                    {
                        "id": "lbl",
                        "type": "text",
                        "content": "Default",
                        "fill": "#FFFFFF"
                    },
                    {
                        "id": "trailing_slot",
                        "type": "frame",
                        "name": "Trailing",
                        "slot": ["badge_comp"]
                    }
                ]
            },
            {
                "id": "badge_comp",
                "type": "frame",
                "name": "Badge",
                "reusable": true,
                "width": 16,
                "height": 16
            },
            {
                "id": "inst_1",
                "type": "ref",
                "ref": "btn_comp",
                "name": "CTA Button",
                "fill": "#16A34A",
                "descendants": {
                    "lbl": {"content": "Get Started"},
                    "trailing_slot": {
                        "type": "rectangle",
                        "id": "replaced_dot",
                        "width": 10,
                        "height": 10,
                        "fill": "#FACC15"
                    }
                }
            },
            {
                "id": "self_cycle",
                "type": "ref",
                "ref": "self_cycle"
            }
        ]
    });

    let (doc, _) = import(&v, "Components", &no_images).unwrap();
    let page = doc.first_page().unwrap();
    let kids = &doc.node(page).unwrap().children;
    assert_eq!(kids.len(), 4);

    let comp = doc.node(kids[0]).unwrap();
    let badge_id = kids[1];
    let NodeKind::Component { props, .. } = &comp.kind else { panic!("expected Component") };
    assert_eq!(props.get("Trailing"), Some(&PropDef::Slot { preferred: vec![badge_id] }));
    let slot_frame = doc.node(comp.children[1]).unwrap();
    assert_eq!(slot_frame.prop_refs.get("slot").map(String::as_str), Some("Trailing"));

    let inst = doc.node(kids[2]).unwrap();
    assert_eq!(inst.name, "CTA Button");
    assert_eq!(inst.fills[0].solid_color(), Some(Color::from_hex("#16A34A").unwrap()));
    let inst_lbl = doc.node(inst.children[0]).unwrap();
    assert_eq!(inst_lbl.kind.text().unwrap().characters, "Get Started");
    let replaced = doc.node(inst.children[1]).unwrap();
    assert!(matches!(replaced.kind, NodeKind::Rectangle));
    assert_eq!(replaced.width, 10.0);
}

#[test]
fn variables_and_multi_mode_themes_resolve_and_bind() {
    let v = json!({
        "remoteData": {
            "themes": {
                "mode": ["light", "dark"]
            },
            "variables": {
                "bg-surface": {
                    "type": "color",
                    "value": [
                        {"value": "#FFFFFF", "theme": {"mode": "light"}},
                        {"value": "#09090B", "theme": {"mode": "dark"}}
                    ]
                },
                "card-pad": {
                    "type": "number",
                    "value": 24
                }
            },
            "nodes": [
                {
                    "id": "light_card",
                    "type": "frame",
                    "fill": "$bg-surface",
                    "padding": "$card-pad",
                    "width": 320,
                    "height": 180
                },
                {
                    "id": "dark_card",
                    "type": "frame",
                    "theme": {"mode": "dark"},
                    "fill": "$bg-surface",
                    "padding": "$card-pad",
                    "width": 320,
                    "height": 180
                }
            ]
        }
    });

    let (doc, warnings) = import(&v, "Tokens", &no_images).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(doc.variables.collections.len(), 1);
    assert_eq!(doc.variables.collections[0].modes.len(), 2);
    assert_eq!(doc.variables.variables.len(), 2);
    assert_eq!(doc.variables.variables[0].kind, VariableType::Color);

    let page = doc.first_page().unwrap();
    let kids = &doc.node(page).unwrap().children;
    let light = doc.node(kids[0]).unwrap();
    let dark = doc.node(kids[1]).unwrap();

    assert_eq!(light.fills[0].solid_color(), Some(Color::from_hex("#FFFFFF").unwrap()));
    assert_eq!(dark.fills[0].solid_color(), Some(Color::from_hex("#09090B").unwrap()));
    assert!(light.bound.contains_key("fill"));
    assert!(dark.bound.contains_key("fill"));
    assert!(!dark.var_modes.is_empty(), "dark card carries explicit variable mode");
    assert_eq!(light.layout.as_ref().unwrap().padding, [24.0; 4]);
}

#[test]
fn malformed_and_adversarial_inputs_never_panic() {
    assert!(extract_clipboard_html("").is_none());
    assert!(extract_clipboard_html("<span data-pen-node-clipboard=\"not-base64!!!\">x</span>").is_none());
    assert!(extract_clipboard_html("<span data-pen-node-clipboard='e30='>x</span>").is_none()); // `{}` is not pen

    let weird = json!({
        "version": "2.22",
        "children": [
            {"type": "unknown_widget"},
            {"id": "no_type"},
            {"type": "path", "geometry": "NOT AN SVG PATH $$$"},
            {"type": "icon", "icon": "arrow-right", "library": "lucide", "width": 20, "height": 20, "fill": "$missing-var"},
            {"type": "note", "content": "Check spacing"},
            {"type": "browser", "url": "http://localhost:3000"},
            {"type": "script", "scriptUri": "./gen.js"}
        ]
    });
    let (doc, warnings) = import(&weird, "Adversarial", &no_images).unwrap();
    assert!(!warnings.is_empty());
    let page = doc.first_page().unwrap();
    assert_eq!(doc.node(page).unwrap().children.len(), 4);
}
