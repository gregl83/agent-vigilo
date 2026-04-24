pub mod parsers {
    use std::path::PathBuf;

    pub(crate) fn parse_dir(s: &str) -> Result<PathBuf, String> {
        let p = PathBuf::from(s);
        if p.is_dir() {
            Ok(p)
        } else {
            Err(format!("'{}' is not a valid directory", s))
        }
    }

    pub(crate) fn parse_filepath(s: &str) -> Result<PathBuf, String> {
        let p = PathBuf::from(s);
        if p.is_file() {
            Ok(p)
        } else {
            Err(format!("'{}' is not a valid filepath", s))
        }
    }
}
