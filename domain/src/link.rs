//! `pwr://` links that are not artifacts.

pub const DAG_URI_PREFIX: &str = "pwr://dag/";

/// The link that opens a DAG run's graph in the desktop.
pub fn dag_uri(run: &str) -> String {
    format!("{DAG_URI_PREFIX}{run}")
}

/// The run ID in a `pwr://dag/<run>` link.
pub fn dag_run_id(uri: &str) -> Option<&str> {
    uri.strip_prefix(DAG_URI_PREFIX)
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dag_links() {
        assert_eq!(dag_run_id(&dag_uri("0b1c-2d")), Some("0b1c-2d"));
        assert_eq!(dag_run_id("pwr://dag/"), None);
        assert_eq!(dag_run_id("pwr://artifact/x"), None);
        assert_eq!(dag_run_id("pwr://dag/a b"), None);
    }
}
