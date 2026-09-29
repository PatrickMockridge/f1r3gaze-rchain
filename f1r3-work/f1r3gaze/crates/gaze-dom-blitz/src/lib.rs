//! `gaze-dom-blitz` — the native document engine for F1R3Gaze (spec §7, WP
//! D1/D2): the DOM protocol's backend over `blitz-dom`, the event handler that
//! routes Blitz's events into the tab executive, and `RhoDocument`, a Blitz
//! `Document` whose only execution mechanism is f1r3lang.

#![forbid(unsafe_code)]

pub mod backend;
pub mod document;
pub mod events;

pub use backend::{BNode, BlitzDom, scope_selector};
pub use document::{Delivery, PageState, RhoDocument, ScriptRef, Services, WakeHandle};
pub use events::{RhoEventHandler, event_fields};

use blitz_dom::{BaseDocument, DEFAULT_CSS, DocumentConfig};
use blitz_html::{DocumentHtmlParser, HtmlProvider};
use std::sync::Arc;

/// Parse HTML into a `BaseDocument` with the user-agent stylesheet and an
/// HTML parser for `setHTML`.
pub fn parse_html(html: &str, mut config: DocumentConfig) -> BaseDocument {
    match &mut config.ua_stylesheets {
        Some(ss) if !ss.iter().any(|s| s == DEFAULT_CSS) => ss.push(DEFAULT_CSS.to_string()),
        Some(_) => {}
        None => config.ua_stylesheets = Some(vec![DEFAULT_CSS.to_string()]),
    }
    if config.html_parser_provider.is_none() {
        config.html_parser_provider = Some(Arc::new(HtmlProvider));
    }
    let mut doc = BaseDocument::new(config);
    {
        let mut m = doc.mutate();
        DocumentHtmlParser::parse_into_mutator(&mut m, html);
    }
    doc
}
