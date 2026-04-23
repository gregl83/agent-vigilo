pub mod parsers {
    use std::path::PathBuf;

    pub fn parse_dir(s: &str) -> Result<PathBuf, String> {
        let p = PathBuf::from(s);
        if p.is_dir() {
            Ok(p)
        } else {
            Err(format!("'{}' is not a valid directory", s))
        }
    }
}