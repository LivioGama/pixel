//! CLI command renames are complete; this module is kept as an empty shell to avoid breaking downstream imports.

pub const RENAMED_COMMANDS: &[(&str, &str)] = &[];
pub const ALIAS_REMOVAL_VERSION: &str = "0.0";

pub fn renamed_to(_old: &str) -> Option<&'static str> {
    None
}
pub fn former_name(_new: &str) -> Option<&'static str> {
    None
}
pub fn current_name(name: &str) -> &str {
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renames_are_empty() {
        assert!(RENAMED_COMMANDS.is_empty());
    }
}
