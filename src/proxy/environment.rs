use anyhow::{anyhow, bail, Context};
use std::{collections::HashMap, fs, path::Path};

pub fn load_dotenv(path: &Path) -> anyhow::Result<HashMap<String, String>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("read tool environment {}", path.display()))?;
    parse_dotenv(&content)
}
pub fn parse_dotenv(content: &str) -> anyhow::Result<HashMap<String, String>> {
    let mut values = HashMap::new();
    for (line_number, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (raw_key, raw_value) = line
            .split_once('=')
            .ok_or_else(|| anyhow!("line {} must use KEY=VALUE", line_number + 1))?;
        let key = raw_key.trim();
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            bail!("line {} has an invalid environment key", line_number + 1);
        }
        let mut value = raw_value.trim().to_owned();
        if value.len() >= 2
            && ((value.starts_with('\"') && value.ends_with('\"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = value[1..value.len() - 1].to_owned();
        }
        if value.contains('\0') {
            bail!("line {} contains NUL", line_number + 1);
        }
        values.insert(key.to_owned(), value);
    }
    Ok(values)
}
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    #[test]
    fn parses_comments_empty_values_and_quotes() {
        let values =
            parse_dotenv("\n# comment\n A = one \nB=\"two\"\nC='three'\nEMPTY=\n").unwrap();
        assert_eq!(values["A"], "one");
        assert_eq!(values["B"], "two");
        assert_eq!(values["C"], "three");
        assert_eq!(values["EMPTY"], "");
    }
    #[test]
    fn rejects_invalid_entries() {
        for input in ["MISSING", "=value", "A=bad\0value", "A\0=value"] {
            assert!(parse_dotenv(input).is_err(), "{input:?}");
        }
    }
    #[test]
    fn reloads_file_on_every_call() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("tool.env");
        fs::write(&path, "SECRET=one\n").unwrap();
        assert_eq!(load_dotenv(&path).unwrap()["SECRET"], "one");
        fs::write(&path, "SECRET=two\n").unwrap();
        assert_eq!(load_dotenv(&path).unwrap()["SECRET"], "two");
    }
}
