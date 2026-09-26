//! The user agent stylesheet.

use std::sync::{Arc, OnceLock};

use crate::stylesheet::{Origin, Stylesheet};

const UA_CSS: &str = include_str!("ua.css");

/// The parsed UA sheet, parsed once per process.
pub fn ua_stylesheet() -> Arc<Stylesheet> {
    static SHEET: OnceLock<Arc<Stylesheet>> = OnceLock::new();
    SHEET
        .get_or_init(|| Arc::new(Stylesheet::parse(UA_CSS, Origin::UserAgent)))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ua_sheet_parses_fully() {
        let sheet = ua_stylesheet();
        // Every rule in the UA sheet must parse; a dropped rule is a bug here.
        let expected = UA_CSS.matches('{').count();
        assert_eq!(sheet.rule_count(), expected);
    }
}
