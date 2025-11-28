// Classification Helpers

pub fn validate_dewey(ddc: &str) -> bool {
    // Basic validation for Dewey Decimal Classification
    // e.g. "005.133"
    ddc.chars().all(|c| c.is_ascii_digit() || c == '.')
}

pub fn validate_lcc(lcc: &str) -> bool {
    // Basic validation for Library of Congress Classification
    !lcc.is_empty()
}
