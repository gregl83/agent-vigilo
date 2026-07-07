//! Shared clap argument parsing helpers.
//!
//! Command modules use these functions as value parsers when an argument must
//! resolve to an existing file or directory before command execution begins.
//! Keep this module limited to reusable parsing/validation helpers; command
//! semantics belong in the command modules that consume the parsed values.

/// Parser functions intended for direct use from clap `value_parser` fields.
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
