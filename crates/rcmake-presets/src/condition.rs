//! Preset condition parsing and short-circuit evaluation.

use rcmake_base::{BString, Limits};
use rcmake_json::Value;
use rcmake_regex::{MatchOptions, Regex};

use crate::PresetsError;
use crate::schema::{self, Decode, Expand, Field, Reader};

#[derive(Clone, Debug)]
pub(crate) enum Condition {
    Null,
    Const(bool),
    Equals(Field, Field, bool),
    InList(Field, Vec<Field>, bool),
    Matches(Field, Field, bool),
    Group(Vec<Self>, bool),
    Not(Box<Self>),
}

impl Condition {
    pub(crate) fn parse(field: &Field, name: &BString, depth: usize) -> Result<Self, PresetsError> {
        schema::depth(depth, "Preset condition")?;
        if let Value::Bool(value) = field.value.as_ref() {
            return Ok(Self::Const(*value));
        }
        if matches!(field.value.as_ref(), Value::Null) {
            return if depth == 0 {
                Ok(Self::Null)
            } else {
                Err(PresetsError::new(format!(
                    "Invalid condition for preset \"{name}\""
                )))
            };
        }
        let label = format!(" condition for preset \"{name}\"");
        let mut reader = Reader::object(field, &label)
            .map_err(|_| field.invalid(format_args!("Invalid condition for preset \"{name}\"")))?;
        let kind = reader.required("type")?;
        let condition = match schema::text(&kind)?.as_slice() {
            b"const" => Self::Const(bool::decode(
                &reader.required("value")?,
                &mut schema::literal,
            )?),
            op @ (b"equals" | b"notEquals") => Self::Equals(
                operand(&mut reader, "lhs")?,
                operand(&mut reader, "rhs")?,
                op == b"equals",
            ),
            op @ (b"inList" | b"notInList") => {
                let needle = operand(&mut reader, "string")?;
                let list = schema::array(&reader.required("list")?)?
                    .map(|item| {
                        schema::text(&item)?;
                        Ok(item)
                    })
                    .collect::<Result<_, PresetsError>>()?;
                Self::InList(needle, list, op == b"inList")
            }
            op @ (b"matches" | b"notMatches") => Self::Matches(
                operand(&mut reader, "string")?,
                operand(&mut reader, "regex")?,
                op == b"matches",
            ),
            op @ (b"allOf" | b"anyOf") => {
                let conditions = schema::array(&reader.required("conditions")?)?
                    .map(|item| Self::parse(&item, name, depth + 1))
                    .collect::<Result<_, _>>()?;
                Self::Group(conditions, op == b"allOf")
            }
            b"not" => Self::Not(Box::new(Self::parse(
                &reader.required("condition")?,
                name,
                depth + 1,
            )?)),
            _ => return Err(kind.invalid(format_args!("Invalid condition for preset \"{name}\""))),
        };
        reader.finish()?;
        Ok(condition)
    }

    pub(crate) fn evaluate(&self, expand: &mut Expand<'_>) -> Result<bool, PresetsError> {
        match self {
            Self::Null => Ok(true),
            Self::Const(value) => Ok(*value),
            Self::Equals(lhs, rhs, equals) => Ok((expand(lhs)? == expand(rhs)?) == *equals),
            Self::InList(needle, list, present) => {
                let needle = expand(needle)?;
                for value in list {
                    if expand(value)? == needle {
                        return Ok(*present);
                    }
                }
                Ok(!present)
            }
            Self::Matches(text, regex, matches) => {
                let text = expand(text)?;
                let pattern = expand(regex)?;
                let compiled = Regex::new(pattern.as_slice())
                    .map_err(|error| PresetsError::new(error.message))?;
                let found = compiled
                    .find_at(
                        text.as_slice(),
                        0,
                        MatchOptions::default(),
                        &mut Limits::default().regex_steps,
                    )
                    .map_err(|error| PresetsError::new(error.to_string()))?
                    .is_some();
                Ok(found == *matches)
            }
            Self::Group(conditions, all) => {
                for condition in conditions {
                    if condition.evaluate(expand)? != *all {
                        return Ok(!all);
                    }
                }
                Ok(*all)
            }
            Self::Not(condition) => Ok(!condition.evaluate(expand)?),
        }
    }
}

fn operand(reader: &mut Reader<'_>, name: &str) -> Result<Field, PresetsError> {
    let field = reader.required(name)?;
    schema::text(&field)?;
    Ok(field)
}
