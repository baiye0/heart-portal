//! Literal dotenv values. No shell expansion, substitution or parent-env reads.
use std::collections::HashMap;

pub(super) fn parse(content: &str) -> Result<HashMap<String, String>, ()> {
    let mut values = HashMap::new();
    let mut lines = content.trim_start_matches('\u{feff}').lines();
    while let Some(line) = lines.next() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let (key, raw) = line.split_once('=').ok_or(())?;
        let key = key.trim();
        if !super::environment::valid_name(key) || key.len() > 128 {
            return Err(());
        }
        let raw = raw.trim_start();
        let value = if raw.starts_with(['\'', '"']) {
            let quote = raw.chars().next().ok_or(())?;
            let mut fragment = raw[1..].to_string();
            let mut value = String::new();
            loop {
                let mut chars = fragment.char_indices();
                let mut end = None;
                while let Some((index, character)) = chars.next() {
                    if character == quote {
                        end = Some(index + character.len_utf8());
                        break;
                    }
                    if character == '\\' && quote == '"' {
                        let (_, escaped) = chars.next().ok_or(())?;
                        match escaped {
                            'n' => value.push('\n'),
                            'r' => value.push('\r'),
                            't' => value.push('\t'),
                            '\\' | '"' | '$' => value.push(escaped),
                            _ => {
                                value.push('\\');
                                value.push(escaped);
                            }
                        }
                    } else {
                        value.push(character);
                    }
                }
                if let Some(end) = end {
                    let tail = fragment[end..].trim();
                    if !tail.is_empty() && !tail.starts_with('#') {
                        return Err(());
                    }
                    break value;
                }
                value.push('\n');
                fragment = lines.next().ok_or(())?.to_string();
            }
        } else {
            let mut previous_space = true;
            let comment = raw.char_indices().find_map(|(index, character)| {
                let comment = character == '#' && previous_space;
                previous_space = character.is_whitespace();
                comment.then_some(index)
            });
            raw[..comment.unwrap_or(raw.len())].trim_end().to_string()
        };
        if value.contains('\0') || values.len() >= 256 {
            return Err(());
        }
        values.insert(key.to_string(), value);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literal_credentials_are_not_expanded_and_quotes_comments_are_preserved() {
        let values = parse("\u{feff}# header\r\nexport BASE=local\r\nCOPY=${BASE}\nTOKEN=abc$def#ghi\nQUOTED=\"one\\ntwo $literal\" # comment\nSINGLE='a=b # $literal'\nMULTI='one\ntwo'\nEMPTY= # missing\n").unwrap();
        assert_eq!(values["COPY"], "${BASE}");
        assert_eq!(values["TOKEN"], "abc$def#ghi");
        assert_eq!(values["QUOTED"], "one\ntwo $literal");
        assert_eq!(values["SINGLE"], "a=b # $literal");
        assert_eq!(values["MULTI"], "one\ntwo");
        assert_eq!(values["EMPTY"], "");
        for invalid in [
            "TOKEN='private",
            "TOKEN=\"private\" trailing",
            "1TOKEN=private",
            "=private",
        ] {
            assert!(parse(invalid).is_err());
        }
    }
}
