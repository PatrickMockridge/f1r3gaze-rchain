//! Built-in pages (`gaze://…`).

const STYLE: &str = "body{font-family:system-ui,sans-serif;margin:48px auto;max-width:680px;padding:0 24px;color:#1d2330;line-height:1.5}
h1{font-weight:600}code{background:#eef1f6;padding:1px 4px;border-radius:3px}
button{font:inherit;padding:8px 16px;border:1px solid #9aa4b5;border-radius:6px;background:#fff}
button.lit{background:#ffcf3f;border-color:#c79a00}.muted{color:#667085}";

pub fn builtin(url: &str) -> Option<String> {
    let page = url.strip_prefix("gaze://")?.split(['/', '?', '#']).next()?;
    Some(match page {
        "newtab" => format!(
            r##"<html><head><title>New tab</title><style>{STYLE}</style></head><body>
<h1>F1R3Gaze</h1>
<p>A browser whose only execution mechanism is f1r3lang on a native RSpace. Type an address above:
<code>https://…</code>, a shard site <code>f1r3://publisher/project/</code>, or content by hash
<code>f1r3h://blake2b-256/…</code>.</p>
<p>This page runs a f1r3lang program. It holds one capability, the document:</p>
<p><button id="lamp">lamp</button></p>
<p class="muted">Pages with JavaScript are shown without it. <a href="gaze://about">About F1R3Gaze</a></p>
<script type="application/f1r3lang" imports="doc">
new found, sub, clicks, on, off in {{
  doc!("query1", "#lamp", *found) |
  for (@("ok", *lamp) <- found) {{
    lamp!("listen", "click", *clicks, {{}}, *sub) |
    off!(Nil) |
    for (_ <= clicks & _ <- off) {{ lamp!("classAdd", "lit") | on!(Nil) }} |
    for (_ <= clicks & _ <- on)  {{ lamp!("classRemove", "lit") | off!(Nil) }}
  }}
}}
</script></body></html>"##
        ),
        "about" => format!(
            r#"<html><head><title>About F1R3Gaze</title><style>{STYLE}</style></head><body>
<h1>F1R3Gaze {}</h1>
<p>Document engine: Blitz. Executive: CampF1R3. License: Apache-2.0.</p>
<p>Settings live in <code>settings.conf</code> in the profile directory
(set <code>F1R3GAZE_PROFILE</code> to move it). Shard observers, the validator,
the quorum and blob mirrors are configured there.</p>
<p class="muted">Not in this release: graded and K2 pages (work packages U2, U5), the proof rung
(node work package N1), legacy JavaScript tabs, devtools time travel.</p></body></html>"#,
            env!("CARGO_PKG_VERSION")
        ),
        _ => return None,
    })
}

pub fn error(url: &str, why: &str) -> String {
    format!(
        r#"<html><head><title>Could not load</title><style>{STYLE}</style></head><body>
<h1>This page could not be loaded</h1><p><code>{}</code></p><p>{}</p></body></html>"#,
        crate::engine::escape(url),
        crate::engine::escape(why)
    )
}
