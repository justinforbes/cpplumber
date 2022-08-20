use std::{borrow::Cow, hash::Hash, path::PathBuf};

use clang::{Entity, EntityKind};
use widestring::{encode_utf16, encode_utf32};

#[derive(Debug, Clone, Eq)]
pub struct InformationLeakDescription {
    /// Leaked information, as represented in the source code
    pub leaked_information: String,
    /// Byte pattern to match (i.e., leaked information, as represented in the
    /// binary file)
    pub bytes: Vec<u8>,
    /// Data on where the leaked information is declared in the
    /// source code (file name, line number)
    pub declaration_metadata: (PathBuf, u32),
}

impl TryFrom<Entity<'_>> for InformationLeakDescription {
    type Error = ();

    fn try_from(entity: Entity) -> Result<Self, Self::Error> {
        match entity.get_kind() {
            EntityKind::StringLiteral => {
                let leaked_information = entity.get_display_name().unwrap();
                let location = entity.get_location().unwrap().get_file_location();
                let file_location = location.file.unwrap().get_path();
                let line_location = location.line;

                Ok(Self {
                    bytes: string_literal_to_bytes(&leaked_information),
                    leaked_information,
                    declaration_metadata: (file_location, line_location),
                })
            }
            _ => Err(()),
        }
    }
}

impl PartialEq for InformationLeakDescription {
    fn eq(&self, other: &Self) -> bool {
        self.leaked_information == other.leaked_information
    }
}

impl Hash for InformationLeakDescription {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.leaked_information.hash(state);
    }
}

/// We have to reimplement this ourselves since the `clang` crate doesn't
/// provide an easy way to get byte representations of `StringLiteral` entities.
fn string_literal_to_bytes(string_literal: &str) -> Vec<u8> {
    let mut char_it = string_literal.chars();
    let first_char = char_it.next();
    match first_char {
        None => return vec![],
        Some(first_char) => match first_char {
            // Ordinary string (we assume it'll be encoded to ASCII)
            '"' => process_escape_sequences(&string_literal[1..string_literal.len() - 1])
                .unwrap()
                .as_bytes()
                .to_owned(),
            // Wide string (we assume it'll be encoded to UTF-16LE)
            'L' => encode_utf16(
                process_escape_sequences(&string_literal[2..string_literal.len() - 1])
                    .unwrap()
                    .chars(),
            )
            .map(u16::to_le_bytes)
            .fold(Vec::new(), |mut acc: Vec<u8>, e| {
                acc.extend(e);
                acc
            }),
            // UTF-32 string
            'U' => encode_utf32(
                process_escape_sequences(&string_literal[2..string_literal.len() - 1])
                    .unwrap()
                    .chars(),
            )
            .map(u32::to_le_bytes)
            .fold(Vec::new(), |mut acc: Vec<u8>, e| {
                acc.extend(e);
                acc
            }),
            // UTF-8 or UTF-16LE string
            'u' => {
                let second_char = char_it.next().unwrap();
                let third_char = char_it.next().unwrap();
                if second_char == '8' && third_char == '"' {
                    // UTF-8
                    process_escape_sequences(&string_literal[3..string_literal.len() - 1])
                        .unwrap()
                        .as_bytes()
                        .to_owned()
                } else {
                    // UTF-16LE
                    encode_utf16(
                        process_escape_sequences(&string_literal[2..string_literal.len() - 1])
                            .unwrap()
                            .chars(),
                    )
                    .map(u16::to_le_bytes)
                    .fold(Vec::new(), |mut acc: Vec<u8>, e| {
                        acc.extend(e);
                        acc
                    })
                }
            }
            _ => unreachable!("New string literal prefix introduced in the standard?"),
        },
    }
}

fn process_escape_sequences(string: &str) -> Option<Cow<str>> {
    let mut owned: Option<String> = None;
    let mut skip_until: usize = 0;
    for (position, char) in string.chars().enumerate() {
        if position <= skip_until {
            continue;
        }

        if char == '\\' {
            if owned.is_none() {
                owned = Some(string[..position].to_owned());
            }
            let b = owned.as_mut().unwrap();
            let mut escape_char_it = string.chars();
            let first_char = escape_char_it.nth(position + 1);
            if let Some(first_char) = first_char {
                skip_until = position + 1;
                match first_char {
                    // Simple escape sequences
                    'a' => b.push('\x07'),
                    'b' => b.push('\x08'),
                    't' => b.push('\t'),
                    'n' => b.push('\n'),
                    'v' => b.push('\x0b'),
                    'f' => b.push('\x0c'),
                    'r' => b.push('\r'),
                    ' ' => b.push(' '),
                    '\\' => b.push('\\'),
                    '0' | '1' | '2' | '3' | '4' | '5' | '6' | '7' => {
                        let start_position = position + 1;
                        let mut end_position = start_position + 1;
                        if let Some(second_char) = escape_char_it.next() {
                            if second_char.is_digit(8) {
                                end_position += 1;
                            }
                        }
                        if let Some(third_char) = escape_char_it.next() {
                            if third_char.is_digit(8) {
                                end_position += 1;
                            }
                        }

                        // Octal escape sequence (\nnn)
                        let octal_value =
                            u8::from_str_radix(&string[start_position..end_position], 8).unwrap();
                        // TODO: Fix wrong multibyte transformations in some cases
                        b.push(octal_value as char);
                        skip_until = end_position;
                    }
                    a => b.push(a),
                }
            } else {
                return None;
            }
        } else if let Some(o) = owned.as_mut() {
            o.push(char);
        }
    }

    if let Some(owned) = owned {
        Some(Cow::Owned(owned))
    } else {
        Some(Cow::Borrowed(string))
    }
}
